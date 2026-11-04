//! The ClickHouse session: query execution and schema introspection.

use crate::exec::Inner;
use crate::exec::lit;
use crate::schema::COLUMNS;
use crate::schema::column_info;
use crate::schema::object_kind;
use crate::schema::opt_text;
use async_trait::async_trait;
use databrain_connector_core::ConnectorError;
use databrain_connector_core::ConnectorKind;
use databrain_connector_core::DbObject;
use databrain_connector_core::ExecOptions;
use databrain_connector_core::IndexInfo;
use databrain_connector_core::ObjectDetail;
use databrain_connector_core::QueryStream;
use databrain_connector_core::Result;
use databrain_connector_core::SchemaInfo;
use databrain_connector_core::Session;
use databrain_connector_core::TableColumns;
use databrain_connector_core::TableLayout;
use databrain_connector_core::split_key_list;
use std::sync::Arc;

pub struct ChSession(pub(crate) Arc<Inner>);

#[async_trait]
impl Session for ChSession {
    fn kind(&self) -> ConnectorKind {
        ConnectorKind::Clickhouse
    }

    async fn server_version(&self) -> Result<String> {
        let r = self.0.rows("SELECT version()", false).await?;
        Ok(format!("ClickHouse {}", r.first().and_then(|r| r.first().cloned().flatten()).unwrap_or_default()))
    }

    async fn ping(&self) -> Result<()> {
        self.0.rows("SELECT 1", false).await.map(|_| ())
    }

    async fn execute(&self, sql: &str, opts: ExecOptions) -> Result<QueryStream> {
        let (tx, stream) = QueryStream::channel(4);
        let inner = self.0.clone();
        let sql = sql.to_string();
        tokio::spawn(async move {
            if let Err(e) = inner.stream(&sql, &opts, &tx).await {
                tx.send_err(e).await;
            }
        });
        Ok(stream)
    }

    async fn list_schemas(&self) -> Result<Vec<SchemaInfo>> {
        let current = self.0.database.lock().unwrap().clone();
        let rows = self.0.rows("SELECT name FROM system.databases WHERE lower(name) != 'information_schema' ORDER BY name", false).await?;
        let mut out: Vec<SchemaInfo> = rows
            .into_iter()
            .filter_map(|r| r.into_iter().next().flatten())
            .map(|n| {
                let d = current.as_deref() == Some(n.as_str());
                SchemaInfo::new(n, d)
            })
            .collect();
        out.sort_by_key(|s| !s.is_default);
        Ok(out)
    }

    async fn list_objects(&self, schema: &str) -> Result<Vec<DbObject>> {
        let sql = format!("SELECT name, engine, comment, total_rows FROM system.tables WHERE database = {} AND NOT is_temporary ORDER BY name", lit(schema));
        Ok(self
            .0
            .rows(&sql, false)
            .await?
            .into_iter()
            .map(|r| DbObject {
                schema: schema.to_string(),
                name: r.first().cloned().flatten().unwrap_or_default(),
                kind: object_kind(r.get(1).cloned().flatten().as_deref().unwrap_or("")),
                comment: opt_text(&r.get(2).cloned().flatten()),
                row_estimate: r.get(3).cloned().flatten().and_then(|n| n.parse().ok()),
            })
            .collect())
    }

    async fn describe(&self, schema: &str, name: &str) -> Result<ObjectDetail> {
        let t = self
            .0
            .rows(
                &format!("SELECT engine, comment, total_rows, create_table_query FROM system.tables WHERE database = {} AND name = {}", lit(schema), lit(name)),
                false,
            )
            .await?;
        let Some(t) = t.into_iter().next() else {
            return Err(ConnectorError::query(format!("{schema}.{name} not found")));
        };
        let cols = self.0.rows(&format!("{COLUMNS} WHERE database = {} AND table = {} ORDER BY position", lit(schema), lit(name)), false).await?;
        Ok(ObjectDetail {
            object: DbObject {
                schema: schema.to_string(),
                name: name.to_string(),
                kind: object_kind(t.first().cloned().flatten().as_deref().unwrap_or("")),
                comment: opt_text(&t.get(1).cloned().flatten()),
                row_estimate: t.get(2).cloned().flatten().and_then(|n| n.parse().ok()),
            },
            columns: cols.iter().map(|r| column_info(r)).collect(),
            ddl: opt_text(&t.get(3).cloned().flatten()),
            foreign_keys: vec![],
        })
    }

    async fn schema_columns(&self, schema: &str) -> Result<Vec<TableColumns>> {
        let sql = format!(
            "SELECT name, type, default_kind, default_expression, comment, is_in_primary_key, table FROM system.columns WHERE database = {} ORDER BY table, position",
            lit(schema)
        );
        let mut out: Vec<TableColumns> = Vec::new();
        for r in self.0.rows(&sql, false).await? {
            let table = r.get(6).cloned().flatten().unwrap_or_default();
            let col = column_info(&r);
            match out.last_mut() {
                Some(t) if t.table == table => t.columns.push(col),
                _ => out.push(TableColumns { table, columns: vec![col], foreign_keys: vec![] }),
            }
        }
        Ok(out)
    }

    /// Primary key, partition key, sorting key and data skipping indexes (MergeTree).
    async fn table_layout(&self, schema: &str, name: &str) -> Result<TableLayout> {
        let (s, n) = (lit(schema), lit(name));
        let t = self
            .0
            .rows(
                &format!(
                    "SELECT primary_key, partition_key, sorting_key, total_rows, engine, total_bytes FROM system.tables WHERE database = {s} AND name = {n}"
                ),
                false,
            )
            .await?;
        let Some(t) = t.into_iter().next() else { return Ok(TableLayout::default()) };
        let get = |i: usize| t.get(i).cloned().flatten().unwrap_or_default();
        let mut indexes = Vec::new();
        let pk = split_key_list(&get(0));
        if !pk.is_empty() {
            indexes.push(IndexInfo { name: "PRIMARY KEY".into(), columns: pk, unique: false, primary: true, method: None });
        }
        // Older servers have no data_skipping_indices table.
        if let Ok(rows) = self.0.rows(&format!("SELECT name, expr, type FROM system.data_skipping_indices WHERE database = {s} AND table = {n}"), false).await {
            for r in rows {
                let g = |i: usize| r.get(i).cloned().flatten().unwrap_or_default();
                indexes.push(IndexInfo {
                    name: g(0),
                    columns: split_key_list(&g(1)),
                    unique: false,
                    primary: false,
                    method: Some(g(2)).filter(|m| !m.is_empty()),
                });
            }
        }
        let engine = get(4);
        Ok(TableLayout {
            indexes,
            partition_by: split_key_list(&get(1)),
            partition_kind: None,
            cluster_by: split_key_list(&get(2)),
            requires_partition_filter: false,
            row_estimate: get(3).parse().ok(),
            size_bytes: get(5).parse().ok(),
            notes: if engine.is_empty() { vec![] } else { vec![format!("ENGINE = {engine}")] },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ClickhouseConnector;
    use databrain_auth::AuthMethod;
    use databrain_connector_core::ConnectionConfig;
    use databrain_connector_core::Connector;
    use databrain_connector_core::ErrorKind;
    use std::time::Duration;

    /// A fake server: answers the sign-in check, then streams rows and fails
    /// the way ClickHouse 24.11+ does once the result has started: an
    /// exception block, then the connection closed without the last chunk.
    fn broken_stream_server(with_error: bool) -> u16 {
        use std::io::{Read, Write};
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for mut c in l.incoming().flatten() {
                // The whole request: headers, then Content-Length bytes of SQL.
                let mut req = Vec::new();
                let mut b = [0u8; 4096];
                loop {
                    let n = c.read(&mut b).unwrap();
                    req.extend_from_slice(&b[..n]);
                    let text = String::from_utf8_lossy(&req).to_lowercase();
                    let Some(end) = text.find("\r\n\r\n") else { continue };
                    let len = text.lines().find_map(|l| l.strip_prefix("content-length:")).map_or(0, |v| v.trim().parse::<usize>().unwrap());
                    if n == 0 || req.len() >= end + 4 + len {
                        break;
                    }
                }
                let req = String::from_utf8_lossy(&req);
                if req.contains("currentDatabase()") {
                    let body = "currentDatabase()\nString\ndefault\n";
                    let _ = write!(c, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                    continue;
                }
                let tag = "abcdefghijklmnop";
                let chunk = |c: &mut std::net::TcpStream, s: &str| write!(c, "{:x}\r\n{s}\r\n", s.len()).unwrap();
                write!(c, "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\nX-ClickHouse-Exception-Tag: {tag}\r\n\r\n").unwrap();
                chunk(&mut c, "x\nUInt8\n0\n0\n");
                let msg = "Code: 395. DB::Exception: Value passed to 'throwIf' function is non-zero: boom. (FUNCTION_THROW_IF_VALUE_IS_NON_ZERO) (version 25.8.1.1)\n";
                if with_error {
                    chunk(&mut c, &format!("\r\n__exception__\r\n{tag}\r\n{msg}{} {tag}\r\n__exception__\r\n", msg.len()));
                }
                c.flush().unwrap();
                // No terminating chunk: the response is broken on purpose.
            }
        });
        port
    }

    #[tokio::test]
    async fn reports_errors_sent_after_the_result_started() {
        use databrain_auth::InlineCredentialSource;
        for with_error in [true, false] {
            let port = broken_stream_server(with_error);
            let mut cfg = ConnectionConfig::new(ConnectorKind::Clickhouse, AuthMethod::Password { user: "default".into() });
            cfg.host = Some("127.0.0.1".into());
            cfg.port = Some(port);
            let creds = Arc::new(InlineCredentialSource::new(AuthMethod::Password { user: "default".into() }, Some(secrecy::SecretString::from("pw"))));
            let s = ClickhouseConnector.connect(&cfg, creds).await.unwrap();
            let e = s.execute("SELECT throwIf(number = 100)", ExecOptions::default()).await.unwrap().collect().await.unwrap_err();
            if with_error {
                assert!(e.message.contains("boom") && !e.message.contains("abcdefghijklmnop"), "{e:?}");
            } else {
                // Without the message: says what happened and where to find it.
                assert!(e.message.contains("after 2 rows") && e.message.contains("system.query_log (query_id = '"), "{e:?}");
            }
        }
    }

    /// Live test: set DATABRAIN_CLICKHOUSE_HOST (+ _PORT, _USER, _PASSWORD).
    #[ignore = "live: needs DATABRAIN_CLICKHOUSE_HOST (cargo test -- --ignored)"]
    #[tokio::test]
    async fn live_roundtrip() {
        let Ok(host) = std::env::var("DATABRAIN_CLICKHOUSE_HOST") else {
            panic!("live test: DATABRAIN_CLICKHOUSE_HOST not set (run with --ignored)");
        };
        use databrain_auth::InlineCredentialSource;
        let user = std::env::var("DATABRAIN_CLICKHOUSE_USER").unwrap_or_else(|_| "default".into());
        let mut cfg = ConnectionConfig::new(ConnectorKind::Clickhouse, AuthMethod::Password { user: user.clone() });
        cfg.host = Some(host);
        cfg.port = std::env::var("DATABRAIN_CLICKHOUSE_PORT").ok().and_then(|p| p.parse().ok());
        let creds = Arc::new(InlineCredentialSource::new(
            AuthMethod::Password { user },
            std::env::var("DATABRAIN_CLICKHOUSE_PASSWORD").ok().map(secrecy::SecretString::from),
        ));
        let s = ClickhouseConnector.connect(&cfg, creds).await.unwrap();
        assert!(s.server_version().await.unwrap().starts_with("ClickHouse "));
        let s = &s;
        let run = |sql: &'static str| async move { s.execute(sql, ExecOptions::default()).await.unwrap().collect().await };
        run("CREATE DATABASE IF NOT EXISTS databrain_t").await.unwrap();
        run("USE databrain_t").await.unwrap();
        run("CREATE OR REPLACE TABLE t (id UInt64, name Nullable(String), d Date, ts DateTime64(3), amt Decimal(18, 4)) ENGINE = MergeTree PARTITION BY toYYYYMM(d) ORDER BY id")
            .await
            .unwrap();
        let ins =
            run("INSERT INTO t VALUES (1, 'a\\tb', '2024-01-02', '2024-01-02 03:04:05.678', 1.5), (18446744073709551615, NULL, '2024-02-01', now64(), 2)")
                .await
                .unwrap();
        assert_eq!(ins.summary.rows_affected, Some(2));
        let r = run("SELECT * FROM t ORDER BY id").await.unwrap();
        assert_eq!(r.num_rows(), 2);
        let t = databrain_connector_core::query_text(s.as_ref(), "SELECT id, name, d, amt FROM t ORDER BY id").await.unwrap();
        assert_eq!((t[0][1].as_deref(), t[0][2].as_deref(), t[0][3].as_deref()), (Some("a\tb"), Some("2024-01-02"), Some("1.5")));
        assert_eq!((t[1][0].as_deref(), t[1][1].as_deref()), (Some("18446744073709551615"), None));
        // An error after the first rows were sent (HTTP 200 already).
        let e = run("SELECT throwIf(number = 50000, 'boom') FROM numbers(100000) SETTINGS max_block_size = 100").await.unwrap_err();
        // ClickHouse 25.8 may break the response off without the message.
        assert!(e.message.contains("boom") || e.message.contains("system.query_log"), "{e:?}");
        // Session state: a temporary table survives to the next statement.
        run("CREATE TEMPORARY TABLE tmp (x Int32)").await.unwrap();
        run("INSERT INTO tmp VALUES (7)").await.unwrap();
        assert_eq!(run("SELECT x FROM tmp").await.unwrap().num_rows(), 1);
        let e = run("SELECT nope FROM t").await.unwrap_err();
        assert!(e.message.contains("nope"), "{e:?}");
        let objs = s.list_objects("databrain_t").await.unwrap();
        assert_eq!(objs[0].name, "t");
        let d = s.describe("databrain_t", "t").await.unwrap();
        assert!(d.columns[0].is_primary_key && d.columns[1].nullable && d.ddl.unwrap().contains("MergeTree"));
        let l = s.table_layout("databrain_t", "t").await.unwrap();
        assert_eq!((l.indexes[0].columns.clone(), l.partition_by.clone()), (vec!["id".to_string()], vec!["toYYYYMM(d)".to_string()]));
        assert_eq!(l.row_estimate, Some(2), "{l:?}");
        assert!(l.size_bytes.is_some_and(|b| b > 0), "{l:?}");
        assert_eq!(s.schema_columns("databrain_t").await.unwrap()[0].columns.len(), 5);
        // A read-only connection: the server refuses writes.
        let mut ro = cfg.clone();
        ro.read_only = true;
        let creds = Arc::new(InlineCredentialSource::new(
            AuthMethod::Password { user: ro.auth.user().unwrap_or("default").to_string() },
            std::env::var("DATABRAIN_CLICKHOUSE_PASSWORD").ok().map(secrecy::SecretString::from),
        ));
        let r = ClickhouseConnector.connect(&ro, creds).await.unwrap();
        assert!(r.execute("INSERT INTO databrain_t.t (id) VALUES (3)", ExecOptions::default()).await.unwrap().collect().await.is_err());
        assert_eq!(r.execute("SELECT count() FROM databrain_t.t", ExecOptions::default()).await.unwrap().collect().await.unwrap().num_rows(), 1);
        // Cancel a long query.
        let cancel = databrain_connector_core::CancellationToken::new();
        let stream = s
            .execute("SELECT sleepEachRow(1) FROM numbers(30) SETTINGS max_block_size = 1", ExecOptions { cancel: cancel.clone(), ..Default::default() })
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;
        cancel.cancel();
        assert_eq!(stream.collect().await.unwrap_err().kind, ErrorKind::Cancelled);
        run("DROP DATABASE databrain_t").await.unwrap();
    }
}
