//! HTTP request handling and result streaming for a ClickHouse session.

use crate::FORMAT;
use crate::SESSION_TIMEOUT;
use crate::error::ended_early;
use crate::error::net_err;
use crate::error::server_error;
use crate::types::cell;
use crate::types::col_type;
use crate::types::split_tsv;
use databrain_connector_core::BatchBuilder;
use databrain_connector_core::ColType;
use databrain_connector_core::Column;
use databrain_connector_core::ConnectorError;
use databrain_connector_core::ExecOptions;
use databrain_connector_core::ExecSummary;
use databrain_connector_core::Result;
use databrain_connector_core::StreamEvent;
use databrain_connector_core::StreamSender;
use std::sync::Arc;
use std::time::Duration;

pub(crate) struct Inner {
    pub(crate) http: reqwest::Client,
    pub(crate) base: String,
    pub(crate) user: String,
    pub(crate) password: String,
    pub(crate) session_id: String,
    pub(crate) read_only: bool,
    /// Current database (sent with each request; `USE` changes it).
    pub(crate) database: std::sync::Mutex<Option<String>>,
    /// ClickHouse runs one query at a time per session.
    pub(crate) lock: Arc<tokio::sync::Mutex<()>>,
}

impl Inner {
    pub(crate) fn request(&self, sql: &str, query_id: &str, in_session: bool) -> reqwest::RequestBuilder {
        let mut q: Vec<(&str, String)> = vec![("query_id", query_id.to_string()), ("default_format", FORMAT.into())];
        if in_session {
            q.push(("session_id", self.session_id.clone()));
            q.push(("session_timeout", SESSION_TIMEOUT.into()));
        }
        if let Some(db) = self.database.lock().unwrap().clone() {
            q.push(("database", db));
        }
        if self.read_only {
            // Read and SET only; enforced by the server.
            q.push(("readonly", "2".into()));
        }
        self.http.post(&self.base).query(&q).header("X-ClickHouse-User", &self.user).header("X-ClickHouse-Key", &self.password).body(sql.to_string())
    }

    pub(crate) async fn send(&self, sql: &str, query_id: &str, in_session: bool) -> Result<reqwest::Response> {
        // A cancelled query can hold the session for a moment after KILL.
        let mut tries = 0;
        loop {
            match self.send_once(sql, query_id, in_session).await {
                Err(e) if in_session && tries < 40 && e.code.as_deref().is_some_and(|c| c.starts_with("373 ")) => {
                    tries += 1;
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
                r => return r,
            }
        }
    }

    pub(crate) async fn send_once(&self, sql: &str, query_id: &str, in_session: bool) -> Result<reqwest::Response> {
        let resp = self.request(sql, query_id, in_session).send().await.map_err(net_err)?;
        if resp.status().is_success() {
            return Ok(resp);
        }
        let status = resp.status();
        let code = resp.headers().get("X-ClickHouse-Exception-Code").and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<u32>().ok());
        let text = resp.text().await.unwrap_or_default();
        Err(server_error(code, status.as_u16(), &text))
    }

    /// Ask the server to stop a query (from outside its session, which is busy).
    pub(crate) async fn kill(&self, query_id: &str) {
        let sql = format!("KILL QUERY WHERE query_id = {} ASYNC", lit(query_id));
        let id = uuid::Uuid::new_v4().to_string();
        if let Ok(r) = self.request(&sql, &id, false).send().await {
            let _ = r.bytes().await;
        }
    }

    /// A small query read whole (metadata), outside the session unless `in_session`.
    pub(crate) async fn rows(&self, sql: &str, in_session: bool) -> Result<Vec<Vec<Option<String>>>> {
        let id = uuid::Uuid::new_v4().to_string();
        let _guard = if in_session { Some(self.lock.lock().await) } else { None };
        let body = self.send(sql, &id, in_session).await?.bytes().await.map_err(net_err)?;
        let mut lines = body.split(|b| *b == b'\n').filter(|l| !l.is_empty());
        // Names and types.
        lines.next();
        lines.next();
        let mut out = Vec::new();
        for l in lines {
            if l.starts_with(b"Code: ") || l.starts_with(b"__exception__") {
                return Err(server_error(None, 500, &String::from_utf8_lossy(l)));
            }
            out.push(split_tsv(l));
        }
        Ok(out)
    }

    pub(crate) async fn stream(&self, sql: &str, opts: &ExecOptions, tx: &StreamSender) -> Result<()> {
        let _guard = self.lock.clone().lock_owned().await;
        let query_id = uuid::Uuid::new_v4().to_string();
        let mut resp = tokio::select! {
            r = self.send(sql, &query_id, true) => r?,
            _ = opts.cancel.cancelled() => {
                self.kill(&query_id).await;
                return Err(ConnectorError::cancelled());
            }
        };
        let written = resp
            .headers()
            .get("X-ClickHouse-Summary")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
            .and_then(|v| v["written_rows"].as_str().and_then(|n| n.parse::<u64>().ok()))
            .filter(|n| *n > 0);

        let mut buf: Vec<u8> = Vec::new();
        let mut header: Vec<Vec<Option<String>>> = Vec::new();
        let mut sink: Option<(BatchBuilder, Vec<ColType>)> = None;
        let mut exception: Option<String> = None;
        let max = opts.max_rows.unwrap_or(usize::MAX);
        let mut rows = 0usize;
        'read: loop {
            let chunk = tokio::select! {
                c = resp.chunk() => c.map_err(|e| ended_early(e, &query_id, rows))?,
                _ = opts.cancel.cancelled() => {
                    drop(resp);
                    self.kill(&query_id).await;
                    return Err(ConnectorError::cancelled());
                }
            };
            let done = chunk.is_none();
            if let Some(c) = chunk {
                buf.extend_from_slice(&c);
            } else if !buf.is_empty() && buf.last() != Some(&b'\n') {
                buf.push(b'\n');
            }
            while let Some(i) = buf.iter().position(|b| *b == b'\n') {
                let line: Vec<u8> = buf.drain(..=i).collect();
                let line = &line[..line.len() - 1];
                let line = line.strip_suffix(b"\r").unwrap_or(line);
                // An error after the result started (the HTTP status was already 200).
                if let Some(e) = exception.as_mut() {
                    if line.starts_with(b"__exception__") {
                        break 'read;
                    }
                    if !e.is_empty() {
                        e.push('\n');
                    }
                    e.push_str(&String::from_utf8_lossy(line));
                    continue;
                }
                if line.starts_with(b"__exception__") {
                    exception = Some(String::new());
                    continue;
                }
                let width = header.first().map(Vec::len);
                if line.starts_with(b"Code: ") && width.is_some_and(|w| line.split(|b| *b == b'\t').count() != w) {
                    exception = Some(String::from_utf8_lossy(line).into_owned());
                    continue;
                }
                if header.len() < 2 {
                    header.push(split_tsv(line));
                    if header.len() == 2 {
                        let cols: Vec<Column> = header[0]
                            .iter()
                            .zip(&header[1])
                            .map(|(n, t)| {
                                let t = t.clone().unwrap_or_default();
                                Column::new(n.clone().unwrap_or_default(), col_type(&t), t)
                            })
                            .collect();
                        let b = BatchBuilder::new(&cols, opts.batch_size.max(1));
                        if !tx.send(StreamEvent::Schema(b.schema())).await {
                            return Ok(());
                        }
                        sink = Some((b, cols.iter().map(|c| c.col_type).collect()));
                    }
                    continue;
                }
                let Some((b, types)) = sink.as_mut() else { continue };
                let cells = split_tsv(line);
                b.push_row(types.iter().enumerate().map(|(i, t)| cell(*t, cells.get(i).cloned().flatten())));
                rows += 1;
                if b.is_full() && !tx.send(StreamEvent::Batch(b.finish()?)).await {
                    return Ok(());
                }
                if rows >= max {
                    // Dropping the response stops a read-only query on the server.
                    break 'read;
                }
            }
            if done {
                break;
            }
        }
        if let Some(e) = exception {
            return Err(server_error(None, 500, e.trim()));
        }
        if let Some((mut b, _)) = sink {
            if !b.is_empty() && !tx.send(StreamEvent::Batch(b.finish()?)).await {
                return Ok(());
            }
            for n in b.notices(None) {
                tx.send(StreamEvent::Notice(n)).await;
            }
        } else if !header.is_empty() {
            return Err(ConnectorError::internal("ClickHouse: the result ended after its column names"));
        }
        if let Some(db) = use_target(sql) {
            *self.database.lock().unwrap() = Some(db);
        }
        tx.send(StreamEvent::Done(ExecSummary { rows_affected: written })).await;
        Ok(())
    }
}

/// The database a `USE db` statement switches to.
pub(crate) fn use_target(sql: &str) -> Option<String> {
    let s = sql.trim().trim_end_matches(';').trim();
    let (word, rest) = s.split_once(char::is_whitespace)?;
    if !word.eq_ignore_ascii_case("use") {
        return None;
    }
    let name = rest.trim();
    let quoted = name
        .strip_prefix('`')
        .and_then(|n| n.strip_suffix('`'))
        .or_else(|| name.strip_prefix('"').and_then(|n| n.strip_suffix('"')))
        .map(|n| n.replace("\\`", "`").replace("\\\"", "\""));
    match quoted {
        Some(q) => (!q.is_empty()).then_some(q),
        None => (!name.is_empty() && !name.contains(char::is_whitespace)).then(|| name.to_string()),
    }
}

/// A string literal (backslash escapes).
pub(crate) fn lit(s: &str) -> String {
    format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn use_statements() {
        assert_eq!(use_target("USE analytics;").as_deref(), Some("analytics"));
        assert_eq!(use_target("use `my db`").as_deref(), Some("my db"));
        assert_eq!(use_target("select 1"), None);
        assert_eq!(lit("a'b\\"), "'a\\'b\\\\'");
    }
}
