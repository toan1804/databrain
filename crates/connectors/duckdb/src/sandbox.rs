use databrain_connector_core::external::RESULTS_SCHEMA;
use databrain_connector_core::{Result, quote_literal};

use crate::error::map_err;
use crate::files::detect_format;

/// Lock a database instance down for SQL the user didn't write: no file or
/// network access (`read_csv`, `COPY … TO`, `ATTACH`, `httpfs`, extension
/// loading) and no setting can be changed back. `results` (DataBrain's
/// outputs, in memory) is attached first, as attaching needs access.
///
/// The attached `files` stay readable: their views are bound again on
/// every query, so each file (and each Parquet / Delta / Iceberg folder)
/// is allowed by name, and nothing else on disk.
pub(crate) fn seal(conn: &duckdb::Connection, files: &[String]) -> Result<()> {
    let attached: bool = conn.query_row("SELECT count(*) > 0 FROM duckdb_databases() WHERE database_name = 'results'", [], |r| r.get(0)).map_err(map_err)?;
    if !attached {
        conn.execute_batch(&format!("ATTACH ':memory:' AS {RESULTS_SCHEMA}")).map_err(map_err)?;
    }
    let (dirs, paths) = sandbox_allowed(files);
    let list = |v: &[String]| v.iter().map(|p| quote_literal(p)).collect::<Vec<_>>().join(", ");
    if !dirs.is_empty() {
        conn.execute_batch(&format!("SET allowed_directories = [{}]", list(&dirs))).map_err(map_err)?;
    }
    if !paths.is_empty() {
        conn.execute_batch(&format!("SET allowed_paths = [{}]", list(&paths))).map_err(map_err)?;
    }
    conn.execute_batch("SET enable_external_access = false; SET lock_configuration = true;").map_err(map_err)
}

/// (folders, files) a sealed instance may still read: the attached files
/// [`attach_files`](crate::files::attach_files) made views of. A folder ends with a separator so that
/// `/data/sales/` does not also allow `/data/sales_private/`.
pub(crate) fn sandbox_allowed(files: &[String]) -> (Vec<String>, Vec<String>) {
    let (mut dirs, mut paths) = (Vec::new(), Vec::new());
    for f in files {
        if detect_format(f).is_none() {
            continue;
        }
        if std::path::Path::new(f).is_dir() {
            let base = f.trim_end_matches(['/', '\\']);
            dirs.push(format!("{base}{}", std::path::MAIN_SEPARATOR));
        } else {
            paths.push(f.clone());
        }
    }
    (dirs, paths)
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::test_util::*;
    use databrain_auth::AuthMethod;

    use databrain_connector_core::{ConnectionConfig, ConnectorError, ConnectorKind, ExecOptions, SANDBOX_OPTION, Session, quote_literal};

    async fn q_err(s: &dyn Session, sql: &str) -> ConnectorError {
        match s.execute(sql, ExecOptions::default()).await {
            Ok(st) => st.collect().await.err().unwrap_or_else(|| panic!("{sql} ran in the sandbox")),
            Err(e) => e,
        }
    }

    /// Attached files and a Parquet folder, with neighbours the sandbox must not open.
    fn sandbox_files(dir: &std::path::Path) -> (std::path::PathBuf, std::path::PathBuf) {
        let csv = dir.join("sales.csv");
        std::fs::write(&csv, "region,amount\nEU,10\nUS,5\n").unwrap();
        std::fs::write(dir.join("secret.csv"), "k\nhidden\n").unwrap();
        let parts = dir.join("parts");
        let near = dir.join("parts_private");
        let c = duckdb::Connection::open_in_memory().unwrap();
        for d in [&parts, &near] {
            std::fs::create_dir_all(d).unwrap();
            c.execute_batch(&format!("COPY (SELECT range AS id FROM range(3)) TO {} (FORMAT parquet)", quote_literal(&d.join("p0.parquet").to_string_lossy())))
                .unwrap();
        }
        (csv, parts)
    }

    #[tokio::test]
    async fn sandbox_reads_attached_files_only() {
        let dir = tempfile::tempdir().unwrap();
        let (csv, parts) = sandbox_files(dir.path());
        let mut cfg = ConnectionConfig::new(ConnectorKind::Duckdb, AuthMethod::None);
        cfg.options.insert("files".into(), format!("{}\n{}", csv.display(), parts.display()));
        cfg.options.insert(SANDBOX_OPTION.into(), "1".into());
        let s = session(cfg).await;

        // The views are bound again on each query: the files they read stay readable.
        let r = q(&*s, "select sum(amount) from files.sales").await;
        assert_eq!(r.num_rows(), 1);
        assert_eq!(q(&*s, "select * from files.parts").await.num_rows(), 3);
        assert_eq!(q(&*s, &format!("select * from read_csv({})", quote_literal(&csv.to_string_lossy()))).await.num_rows(), 2);
        // The `results` catalog was attached before the lock.
        assert_eq!(q(&*s, "select database_name from duckdb_databases() where database_name = 'results'").await.num_rows(), 1);

        // Anything else on disk, the network and settings are refused.
        let other = dir.path().join("secret.csv");
        let e = q_err(&*s, &format!("select * from read_csv({})", quote_literal(&other.to_string_lossy()))).await;
        assert!(e.message.contains("sandbox"), "{}", e.message);
        let near = dir.path().join("parts_private").join("p0.parquet");
        let e = q_err(&*s, &format!("select * from read_parquet({})", quote_literal(&near.to_string_lossy()))).await;
        assert!(e.message.contains("sandbox"), "a folder with the same prefix: {}", e.message);
        let out = dir.path().join("out.csv");
        q_err(&*s, &format!("copy (select 1) to {}", quote_literal(&out.to_string_lossy()))).await;
        assert!(!out.exists());
        q_err(&*s, &format!("attach {} as other", quote_literal(&dir.path().join("x.duckdb").to_string_lossy()))).await;
        q_err(&*s, "select * from read_csv('https://example.com/a.csv')").await;
        for set in
            ["SET enable_external_access = true", "SET allowed_directories = ['/']", "SET allowed_paths = ['/etc/passwd']", "SET lock_configuration = false"]
        {
            let e = q_err(&*s, set).await;
            assert!(e.message.contains("sandbox"), "{set}: {}", e.message);
        }
    }

    #[tokio::test]
    async fn sandbox_on_a_database_file() {
        let dir = tempfile::tempdir().unwrap();
        let (csv, _) = sandbox_files(dir.path());
        let db = dir.path().join("local.duckdb");
        duckdb::Connection::open(&db).unwrap().execute_batch("CREATE TABLE t AS SELECT 1 AS a").unwrap();
        let mut cfg = ConnectionConfig::new(ConnectorKind::Duckdb, AuthMethod::None);
        cfg.file_path = Some(db.to_string_lossy().into_owned());
        cfg.options.insert("files".into(), csv.to_string_lossy().into_owned());
        cfg.options.insert(SANDBOX_OPTION.into(), "1".into());
        let s = session(cfg.clone()).await;
        assert_eq!(q(&*s, "select * from t").await.num_rows(), 1);
        assert_eq!(q(&*s, "select * from files.sales").await.num_rows(), 2);
        // A second sandboxed session shares the sealed instance.
        let s2 = session(cfg).await;
        assert_eq!(q(&*s2, "select * from files.sales").await.num_rows(), 2);
        let e = q_err(&*s2, &format!("select * from read_csv({})", quote_literal(&dir.path().join("secret.csv").to_string_lossy()))).await;
        assert!(e.message.contains("sandbox"), "{}", e.message);
    }

    #[test]
    fn sandbox_allows_attached_files_by_name() {
        let dir = tempfile::tempdir().unwrap();
        let (csv, parts) = sandbox_files(dir.path());
        let sep = std::path::MAIN_SEPARATOR;
        let files = vec![csv.to_string_lossy().into_owned(), format!("{}{sep}", parts.display()), "/x/notes.bin".into()];
        let (dirs, paths) = sandbox_allowed(&files);
        assert_eq!(dirs, vec![format!("{}{sep}", parts.display())], "one separator at the end");
        assert_eq!(paths, vec![csv.to_string_lossy().into_owned()], "files DataBrain makes no view of are not allowed");
    }
}
