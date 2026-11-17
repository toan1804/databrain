//! Bulk loads through a Unity Catalog volume: the rows' Parquet files are
//! uploaded to a folder of the connection's staging volume (Files API), one
//! statement reads them all into the table (one commit), and the folder is
//! deleted, also when the load fails or is stopped.

use std::sync::Arc;

use async_trait::async_trait;
use databrain_connector_core::{BULK_FILES, BulkWriter, CancellationToken, ConnectorError, Result};

use super::client::Inner;
use crate::common::{json_body, net_err};

/// Connection option: the volume (or a folder in it) bulk loads stage their
/// files in, `/Volumes/<catalog>/<schema>/<volume>[/<folder>…]`.
pub const STAGING_VOLUME_OPTION: &str = "staging_volume";

/// The staging volume as set (trailing `/` dropped), or why it can't be one.
/// Each part is a plain name: no `..`, quotes, backslashes or URL syntax,
/// since it goes into a URL path and an SQL string.
pub(super) fn staging_dir(v: &str) -> std::result::Result<String, String> {
    let t = v.trim().trim_end_matches('/');
    let parts: Vec<&str> = t.split('/').collect();
    let plain = |p: &&str| !p.is_empty() && *p != "." && *p != ".." && !p.chars().any(|c| c.is_control() || "\\'\"`?#%".contains(c));
    if parts.len() < 5 || !parts[0].is_empty() || parts[1] != "Volumes" || !parts[2..].iter().all(plain) {
        return Err(format!("Staging volume \"{}\" isn't a volume path: write /Volumes/<catalog>/<schema>/<volume> (optionally a folder after it)", v.trim()));
    }
    Ok(t.to_string())
}

/// A path for a URL: each part percent-encoded (spaces, non-ASCII), `/` kept.
fn url_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for b in path.bytes() {
        if b.is_ascii_alphanumeric() || b"/-_.~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

impl Inner {
    /// A Files API request on `path` (`files` or `directories`), its body if any.
    async fn files(&self, method: reqwest::Method, api: &str, path: &str, body: Option<Vec<u8>>) -> Result<()> {
        let url = format!("{}/api/2.0/fs/{api}{}{}", self.origin, url_path(path), if body.is_some() { "?overwrite=true" } else { "" });
        for attempt in 0..2 {
            let token = self.token().await?;
            let mut req = self.http.request(method.clone(), &url).bearer_auth(token);
            if let Some(b) = &body {
                req = req.header(reqwest::header::CONTENT_TYPE, "application/octet-stream").body(b.clone());
            }
            let resp = req.send().await.map_err(net_err)?;
            if resp.status().as_u16() == 401 && attempt == 0 {
                self.creds.invalidate().await;
                continue;
            }
            return json_body(resp, "Databricks Files API").await.map(|_| ());
        }
        unreachable!()
    }

    /// Delete the staged files and their folder, best effort.
    async fn delete_staged(&self, dir: &str, files: &[String]) {
        for f in files {
            if let Err(e) = self.files(reqwest::Method::DELETE, "files", f, None).await {
                tracing::warn!(file = %f, error = %e, "a staged file of a bulk load could not be deleted");
            }
        }
        if let Err(e) = self.files(reqwest::Method::DELETE, "directories", dir, None).await {
            tracing::warn!(dir = %dir, error = %e, "the staging folder of a bulk load could not be deleted");
        }
    }
}

/// One bulk load: each [`BulkWriter::write`] is a Parquet file uploaded to
/// `dir`; [`BulkWriter::finish`] runs `sql` with `dir` for [`BULK_FILES`].
pub(super) struct VolumeWriter {
    inner: Arc<Inner>,
    dir: String,
    sql: String,
    files: Vec<String>,
    cancel: CancellationToken,
    /// Finished: the files are deleted there; otherwise on drop.
    finished: bool,
}

impl VolumeWriter {
    pub(super) fn new(inner: Arc<Inner>, volume: &str, sql: &str, cancel: CancellationToken) -> Self {
        // Not hidden (`.` or `_` first): Spark skips such names when it lists files.
        let dir = format!("{volume}/databrain-load-{}", uuid::Uuid::new_v4().simple());
        Self { inner, dir, sql: sql.to_string(), files: Vec::new(), cancel, finished: false }
    }

    /// Run the statement over the uploaded files: rows it inserted, as Databricks reports them.
    async fn run(&self) -> Result<Option<u64>> {
        if self.files.is_empty() {
            return Ok(Some(0));
        }
        let literal = databrain_connector_core::sql::quote_string(&self.dir, databrain_connector_core::ConnectorKind::Databricks);
        let sql = self.sql.replace(BULK_FILES, &literal);
        let resp = self.inner.submit(&sql, None).await?;
        let resp = self.inner.wait(resp, &self.cancel, None).await?;
        Ok(inserted_rows(&resp))
    }
}

/// `num_inserted_rows` (else `num_affected_rows`) of an INSERT's result.
fn inserted_rows(resp: &serde_json::Value) -> Option<u64> {
    let cols = resp.pointer("/manifest/schema/columns")?.as_array()?;
    let at = |name: &str| cols.iter().position(|c| c["name"].as_str() == Some(name));
    let i = at("num_inserted_rows").or_else(|| at("num_affected_rows"))?;
    let v = resp.pointer("/result/data_array/0")?.get(i)?;
    v.as_u64().or_else(|| v.as_str().and_then(|s| s.parse().ok()))
}

#[async_trait]
impl BulkWriter for VolumeWriter {
    async fn write(&mut self, data: Vec<u8>) -> Result<()> {
        if self.cancel.is_cancelled() {
            return Err(ConnectorError::cancelled());
        }
        let path = format!("{}/part-{:05}.parquet", self.dir, self.files.len());
        // Listed first: a file cut off half way is deleted too.
        self.files.push(path.clone());
        tokio::select! {
            r = self.inner.files(reqwest::Method::PUT, "files", &path, Some(data)) => r,
            _ = self.cancel.cancelled() => Err(ConnectorError::cancelled()),
        }
    }

    async fn finish(mut self: Box<Self>) -> Result<Option<u64>> {
        self.finished = true;
        let r = self.run().await;
        self.inner.delete_staged(&self.dir, &self.files).await;
        r
    }
}

impl Drop for VolumeWriter {
    /// Stopped or failed before the end: its files are deleted in the background.
    fn drop(&mut self) {
        if self.finished || self.files.is_empty() {
            return;
        }
        let (inner, dir, files) = (self.inner.clone(), self.dir.clone(), std::mem::take(&mut self.files));
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            rt.spawn(async move { inner.delete_staged(&dir, &files).await });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn staging_volume_paths() {
        assert_eq!(staging_dir("/Volumes/main/etl/staging/").unwrap(), "/Volumes/main/etl/staging");
        assert_eq!(staging_dir(" /Volumes/main/etl/staging/loads/a b ").unwrap(), "/Volumes/main/etl/staging/loads/a b");
        for bad in [
            "",
            "/Volumes/main/etl",
            "Volumes/main/etl/v",
            "/volumes/main/etl/v",
            "/Volumes/main/../v/x",
            "/Volumes/main//v",
            "/Volumes/main/etl/v'; drop table t; --",
            "/Volumes/main/etl/v?x=1",
            "/Volumes/main/etl/v\\x",
            "dbfs:/tmp/x",
        ] {
            assert!(staging_dir(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn url_paths_are_encoded() {
        assert_eq!(url_path("/Volumes/main/etl/v/a b/ü.parquet"), "/Volumes/main/etl/v/a%20b/%C3%BC.parquet");
    }

    /// A bulk load uploads each file to a new folder of the staging volume,
    /// inserts from all of them with one statement, and deletes them; a
    /// load dropped half way deletes what it uploaded; without a staging
    /// volume there is no bulk load.
    #[tokio::test]
    async fn files_are_staged_inserted_and_deleted() {
        use databrain_connector_core::Session;
        let (origin, log) = crate::databricks::testutil::mock::serve("volume").await;
        let s = crate::databricks::testutil::mock::staging_session(&origin, "/Volumes/main/etl/stage");
        let sql = format!("INSERT INTO `t` (`a`) SELECT CAST(c0 AS int) FROM read_files({BULK_FILES}, format => 'parquet')");
        let mut w = s.bulk_load(&sql, CancellationToken::new()).await.unwrap().expect("a bulk load");
        w.write(b"PAR1-one".to_vec()).await.unwrap();
        w.write(b"PAR1-two".to_vec()).await.unwrap();
        assert_eq!(w.finish().await.unwrap(), Some(3), "as Databricks reports it");
        let reqs = log.lock().unwrap().requests.clone();
        let puts: Vec<&String> = reqs.iter().filter(|r| r.0 == "PUT").map(|r| &r.1).collect();
        assert_eq!(puts.len(), 2);
        let dir = puts[0].trim_start_matches("/api/2.0/fs/files").split("/part-").next().unwrap().to_string();
        assert!(dir.starts_with("/Volumes/main/etl/stage/databrain-load-"), "{dir}");
        assert!(puts[0].ends_with("/part-00000.parquet?overwrite=true") && puts[1].ends_with("/part-00001.parquet?overwrite=true"), "{puts:?}");
        let insert = reqs.iter().find(|r| r.1 == "/api/2.0/sql/statements" && r.3.contains("INSERT INTO")).unwrap();
        assert!(insert.3.contains(&format!("read_files('{dir}', format")), "{}", insert.3);
        let deletes: Vec<&String> = reqs.iter().filter(|r| r.0 == "DELETE").map(|r| &r.1).collect();
        assert_eq!(deletes.len(), 3, "both files, then the folder: {deletes:?}");
        assert_eq!(deletes[2], &format!("/api/2.0/fs/directories{dir}"));

        // Dropped before the end (stopped, or a later file failed): its files are deleted.
        log.lock().unwrap().requests.clear();
        let mut w = s.bulk_load(&sql, CancellationToken::new()).await.unwrap().unwrap();
        w.write(b"PAR1".to_vec()).await.unwrap();
        drop(w);
        for _ in 0..100 {
            if log.lock().unwrap().requests.iter().filter(|r| r.0 == "DELETE").count() == 2 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let reqs = log.lock().unwrap().requests.clone();
        assert_eq!(reqs.iter().filter(|r| r.0 == "DELETE").count(), 2, "{reqs:?}");
        assert!(!reqs.iter().any(|r| r.3.contains("INSERT INTO")), "nothing inserted");

        // No staging volume: no bulk load (the rows go by INSERTs).
        let plain = crate::databricks::testutil::mock::session(&origin, false);
        assert!(plain.bulk_load(&sql, CancellationToken::new()).await.unwrap().is_none());
    }

    /// A volume the user can't write to: the upload fails and says why.
    #[tokio::test]
    async fn a_volume_that_cant_be_written_says_why() {
        use databrain_connector_core::Session;
        let (origin, _log) = crate::databricks::testutil::mock::serve("volume_denied").await;
        let s = crate::databricks::testutil::mock::staging_session(&origin, "/Volumes/main/etl/stage");
        let mut w = s.bulk_load("INSERT INTO t SELECT 1", CancellationToken::new()).await.unwrap().unwrap();
        let e = w.write(b"PAR1".to_vec()).await.unwrap_err();
        assert!(e.message.contains("WRITE VOLUME"), "{}", e.message);
    }

    #[test]
    fn rows_inserted_as_databricks_reports_them() {
        let resp = serde_json::json!({
            "manifest": {"schema": {"columns": [{"name": "num_affected_rows"}, {"name": "num_inserted_rows"}]}},
            "result": {"data_array": [["2000000", "2000000"]]}
        });
        assert_eq!(inserted_rows(&resp), Some(2_000_000));
        assert_eq!(inserted_rows(&serde_json::json!({"result": {}})), None);
    }
}
