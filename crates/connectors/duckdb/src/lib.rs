//! DuckDB connector: an in-process analytical engine for querying local
//! files (CSV/TSV, Parquet, JSON/NDJSON, Excel, Delta Lake, Iceberg) and
//! DuckDB database files.
//!
//! Results come out of DuckDB as Arrow (duckdb-rs uses arrow 58); batches are
//! moved into the workspace's Arrow version through Arrow IPC, which is
//! lossless and keeps the connector independent of arrow version bumps.
//!
//! "Attached files" (`files` option, one path per line) are exposed as views
//! in the `files` schema, so they show up in the explorer and in AI knowledge.

use std::io::Cursor;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use databrain_auth::{AuthMethodKind, CredentialSource};
use databrain_connector_core::arrow::ipc::reader::StreamReader;
use databrain_connector_core::{
    Capabilities, ColumnInfo, ConnectionConfig, Connector, ConnectorError, ConnectorInfo, ConnectorKind, DbObject,
    ErrorKind, ExecOptions, ExecSummary, FieldSpec, ObjectDetail, ObjectKind, QueryStream, Result, SchemaInfo, Session,
    StreamEvent, StreamSender, TableColumns, quote_ident, quote_literal,
};

#[derive(Debug, Default)]
pub struct DuckdbConnector;

impl DuckdbConnector {
    pub fn new() -> Self {
        Self
    }
}

/// How a file is read, from its extension (or directory layout).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileFormat {
    Csv,
    Parquet,
    Json,
    Excel,
    Delta,
    Iceberg,
}

pub fn detect_format(path: &str) -> Option<FileFormat> {
    let p = std::path::Path::new(path.trim_end_matches(['/', '\\']));
    if p.is_dir() {
        if p.join("_delta_log").is_dir() {
            return Some(FileFormat::Delta);
        }
        if p.join("metadata").is_dir() {
            return Some(FileFormat::Iceberg);
        }
        // A directory of parquet files.
        return Some(FileFormat::Parquet);
    }
    let lower = path.to_ascii_lowercase();
    let lower = lower.trim_end_matches(".gz").trim_end_matches(".zst");
    let ext = lower.rsplit('.').next()?;
    Some(match ext {
        "csv" | "tsv" | "txt" | "psv" => FileFormat::Csv,
        "parquet" | "pq" => FileFormat::Parquet,
        "json" | "ndjson" | "jsonl" => FileFormat::Json,
        "xlsx" => FileFormat::Excel,
        _ => return None,
    })
}

/// SQL expression that reads the file (used in views and suggestions).
pub fn scan_expr(path: &str, format: FileFormat) -> String {
    let lit = quote_literal(path);
    let dir_glob = || {
        let base = path.trim_end_matches(['/', '\\']);
        quote_literal(&format!("{base}/**/*.parquet"))
    };
    match format {
        FileFormat::Csv => format!("read_csv({lit})"),
        FileFormat::Parquet if std::path::Path::new(path).is_dir() => format!("read_parquet({}, hive_partitioning = true)", dir_glob()),
        FileFormat::Parquet => format!("read_parquet({lit})"),
        FileFormat::Json => format!("read_json_auto({lit})"),
        FileFormat::Excel => format!("read_xlsx({lit})"),
        FileFormat::Delta => format!("delta_scan({lit})"),
        FileFormat::Iceberg => format!("iceberg_scan({lit}, allow_moved_paths = true)"),
    }
}

fn required_extension(f: FileFormat) -> Option<&'static str> {
    match f {
        FileFormat::Excel => Some("excel"),
        FileFormat::Delta => Some("delta"),
        FileFormat::Iceberg => Some("iceberg"),
        _ => None,
    }
}

/// View name for an attached file: file stem, sanitized, de-duplicated.
pub fn view_name(path: &str, taken: &[String]) -> String {
    let stem = std::path::Path::new(path.trim_end_matches(['/', '\\']))
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "file".into());
    let stem = stem.split('.').next().unwrap_or(&stem).to_string();
    let mut base: String = stem.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '_' }).collect();
    if base.is_empty() || base.starts_with(|c: char| c.is_ascii_digit()) {
        base = format!("f_{base}");
    }
    let mut name = base.clone();
    let mut i = 2;
    while taken.contains(&name) {
        name = format!("{base}_{i}");
        i += 1;
    }
    name
}

fn map_err(e: duckdb::Error) -> ConnectorError {
    let msg = e.to_string();
    if msg.contains("INTERRUPT") || msg.contains("Interrupted") {
        return ConnectorError::cancelled();
    }
    ConnectorError::query(msg)
}

fn files_of(cfg: &ConnectionConfig) -> Vec<String> {
    cfg.opt("files").map(|f| f.lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_string).collect()).unwrap_or_default()
}

#[async_trait]
impl Connector for DuckdbConnector {
    fn info(&self) -> ConnectorInfo {
        ConnectorInfo {
            kind: ConnectorKind::Duckdb,
            display_name: "DuckDB (local files)",
            default_port: None,
            uses_file: true,
            auth_methods: vec![AuthMethodKind::None],
            capabilities: Capabilities { transactions: true, cancel: true, schemas: true, read_only_sessions: true, ssh: false },
            fields: vec![
                FieldSpec::new("file_path", "Database file").placeholder(":memory: (or /path/to/db.duckdb)"),
                FieldSpec::new("files", "Attached files").placeholder("One path per line: .csv .parquet .json .xlsx, Delta/Iceberg folders")
                    .help("Each file becomes a view in the `files` schema"),
            ],
            note: Some("Query any file directly, e.g. SELECT * FROM 'data/*.parquet'. Delta, Iceberg and Excel download a DuckDB extension on first use."),
        }
    }

    async fn connect(&self, cfg: &ConnectionConfig, _creds: Arc<dyn CredentialSource>) -> Result<Box<dyn Session>> {
        let path = cfg.file_path.clone().filter(|p| !p.trim().is_empty()).unwrap_or_else(|| ":memory:".into());
        let files = files_of(cfg);
        let read_only = cfg.read_only && path != ":memory:";
        let conn = tokio::task::spawn_blocking(move || -> Result<duckdb::Connection> {
            let conn = if path == ":memory:" {
                duckdb::Connection::open_in_memory()
            } else {
                let c = duckdb::Config::default()
                    .access_mode(if read_only { duckdb::AccessMode::ReadOnly } else { duckdb::AccessMode::ReadWrite })
                    .map_err(map_err)?;
                duckdb::Connection::open_with_flags(&path, c)
            }
            .map_err(|e| ConnectorError::connection(e.to_string()))?;
            // Let DuckDB fetch known extensions (delta, iceberg, excel, httpfs) on demand.
            let _ = conn.execute_batch("SET autoinstall_known_extensions = true; SET autoload_known_extensions = true;");
            attach_files(&conn, &files)?;
            Ok(conn)
        })
        .await
        .map_err(|e| ConnectorError::internal(e.to_string()))??;
        Ok(Box::new(DuckSession { interrupt: conn.interrupt_handle(), conn: Arc::new(Mutex::new(conn)) }))
    }
}

/// Create `files.<name>` views over attached files. Unreadable files are
/// skipped with a comment on the view list rather than failing the connection.
fn attach_files(conn: &duckdb::Connection, files: &[String]) -> Result<()> {
    if files.is_empty() {
        return Ok(());
    }
    conn.execute_batch("CREATE SCHEMA IF NOT EXISTS files").map_err(map_err)?;
    let mut taken = Vec::new();
    for f in files {
        let Some(fmt) = detect_format(f) else { continue };
        if let Some(ext) = required_extension(fmt) {
            let _ = conn.execute_batch(&format!("INSTALL {ext}; LOAD {ext};"));
        }
        let name = view_name(f, &taken);
        taken.push(name.clone());
        let sql = format!(
            "CREATE OR REPLACE VIEW files.{} AS SELECT * FROM {}",
            quote_ident(ConnectorKind::Duckdb, &name),
            scan_expr(f, fmt)
        );
        if let Err(e) = conn.execute_batch(&sql) {
            return Err(ConnectorError::new(ErrorKind::Config, format!("cannot read {f}: {e}")));
        }
        let _ = conn.execute_batch(&format!(
            "COMMENT ON VIEW files.{} IS {}",
            quote_ident(ConnectorKind::Duckdb, &name),
            quote_literal(&format!("File: {f}"))
        ));
    }
    Ok(())
}

pub struct DuckSession {
    conn: Arc<Mutex<duckdb::Connection>>,
    interrupt: Arc<duckdb::InterruptHandle>,
}

/// Convert an arrow-58 batch to the workspace's Arrow via IPC.
fn convert_batch(b: &arrow58::record_batch::RecordBatch) -> Result<databrain_connector_core::arrow::array::RecordBatch> {
    let mut buf = Vec::new();
    {
        let mut w = arrow58::ipc::writer::StreamWriter::try_new(&mut buf, &b.schema()).map_err(|e| ConnectorError::internal(e.to_string()))?;
        w.write(b).map_err(|e| ConnectorError::internal(e.to_string()))?;
        w.finish().map_err(|e| ConnectorError::internal(e.to_string()))?;
    }
    let mut r = StreamReader::try_new(Cursor::new(buf), None)?;
    r.next().ok_or_else(|| ConnectorError::internal("empty IPC stream"))?.map_err(Into::into)
}

fn run(conn: &duckdb::Connection, sql: &str, opts: &ExecOptions, tx: &StreamSender) -> Result<()> {
    // Auto-load the extension a query needs (INSTALL is a no-op once cached).
    let lower = sql.to_ascii_lowercase();
    for (f, ext) in [("delta_scan", "delta"), ("iceberg_", "iceberg"), ("read_xlsx", "excel")] {
        if lower.contains(f) {
            let _ = conn.execute_batch(&format!("INSTALL {ext}; LOAD {ext};"));
        }
    }
    let mut stmt = conn.prepare(sql).map_err(map_err)?;
    let kw = databrain_connector_core::sql::leading_keyword(sql);
    let returns_rows = matches!(
        kw.as_str(),
        "SELECT" | "WITH" | "FROM" | "VALUES" | "TABLE" | "SHOW" | "DESCRIBE" | "DESC" | "EXPLAIN" | "SUMMARIZE" | "PRAGMA" | "PIVOT" | "UNPIVOT" | "CALL"
    ) || lower.contains(" returning ");
    if !returns_rows {
        let n = stmt.execute([]).map_err(map_err)?;
        tx.blocking_send(Ok(StreamEvent::Done(ExecSummary { rows_affected: Some(n as u64) })));
        return Ok(());
    }
    let arrow = stmt.query_arrow([]).map_err(map_err)?;
    let schema = arrow.get_schema();
    // Schema first (even for empty results).
    let empty = arrow58::record_batch::RecordBatch::new_empty(schema);
    let first = convert_batch(&empty)?;
    if !tx.blocking_send(Ok(StreamEvent::Schema(first.schema()))) {
        return Ok(());
    }
    for b in arrow {
        if opts.cancel.is_cancelled() {
            return Err(ConnectorError::cancelled());
        }
        // Re-chunk to the requested batch size.
        let size = opts.batch_size.max(1);
        let mut off = 0;
        while off < b.num_rows() {
            let len = size.min(b.num_rows() - off);
            if !tx.blocking_send(Ok(StreamEvent::Batch(convert_batch(&b.slice(off, len))?))) {
                return Ok(());
            }
            off += len;
        }
    }
    tx.blocking_send(Ok(StreamEvent::Done(ExecSummary::default())));
    Ok(())
}

impl DuckSession {
    async fn with_conn<T: Send + 'static>(&self, f: impl FnOnce(&duckdb::Connection) -> Result<T> + Send + 'static) -> Result<T> {
        let c = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let g = c.lock().map_err(|_| ConnectorError::internal("lock poisoned"))?;
            f(&g)
        })
        .await
        .map_err(|e| ConnectorError::internal(e.to_string()))?
    }

    async fn strings(&self, sql: String, params: Vec<String>) -> Result<Vec<Vec<Option<String>>>> {
        self.with_conn(move |c| {
            let mut stmt = c.prepare(&sql).map_err(map_err)?;
            let p: Vec<&dyn duckdb::ToSql> = params.iter().map(|s| s as &dyn duckdb::ToSql).collect();
            let mut rows = stmt.query(p.as_slice()).map_err(map_err)?;
            let n = rows.as_ref().map(|s| s.column_count()).unwrap_or(0);
            let mut out = Vec::new();
            while let Some(r) = rows.next().map_err(map_err)? {
                let mut row = Vec::with_capacity(n);
                for i in 0..n {
                    let v: duckdb::types::Value = r.get(i).map_err(map_err)?;
                    row.push(match v {
                        duckdb::types::Value::Null => None,
                        duckdb::types::Value::Text(s) => Some(s),
                        duckdb::types::Value::Boolean(b) => Some(b.to_string()),
                        duckdb::types::Value::BigInt(i) => Some(i.to_string()),
                        duckdb::types::Value::Int(i) => Some(i.to_string()),
                        duckdb::types::Value::HugeInt(i) => Some(i.to_string()),
                        duckdb::types::Value::UBigInt(i) => Some(i.to_string()),
                        other => Some(format!("{other:?}")),
                    });
                }
                out.push(row);
            }
            Ok(out)
        })
        .await
    }
}

#[async_trait]
impl Session for DuckSession {
    fn kind(&self) -> ConnectorKind {
        ConnectorKind::Duckdb
    }

    async fn server_version(&self) -> Result<String> {
        let r = self.strings("select version()".into(), vec![]).await?;
        Ok(format!("DuckDB {}", r.first().and_then(|r| r[0].clone()).unwrap_or_default()))
    }

    async fn ping(&self) -> Result<()> {
        self.strings("select 1".into(), vec![]).await.map(|_| ())
    }

    async fn execute(&self, sql: &str, opts: ExecOptions) -> Result<QueryStream> {
        let (tx, stream) = QueryStream::channel(4);
        let conn = self.conn.clone();
        let sql = sql.to_string();
        let (cancel, interrupt) = (opts.cancel.clone(), self.interrupt.clone());
        let done = tokio_util::sync::CancellationToken::new();
        let done_guard = done.clone();
        tokio::spawn(async move {
            tokio::select! {
                _ = cancel.cancelled() => interrupt.interrupt(),
                _ = done.cancelled() => {}
            }
        });
        tokio::task::spawn_blocking(move || {
            let _done = done_guard.drop_guard();
            let Ok(g) = conn.lock() else {
                tx.blocking_send(Err(ConnectorError::internal("lock poisoned")));
                return;
            };
            if let Err(e) = run(&g, &sql, &opts, &tx) {
                let e = if opts.cancel.is_cancelled() { ConnectorError::cancelled() } else { e };
                tx.blocking_send(Err(e));
            }
        });
        Ok(stream)
    }

    async fn list_schemas(&self) -> Result<Vec<SchemaInfo>> {
        let rows = self
            .strings(
                "select database_name || '.' || schema_name, (database_name = current_database() and schema_name = current_schema())::varchar \
                 from duckdb_schemas() where not internal and schema_name <> 'pg_catalog' \
                 order by database_name = current_database() desc, schema_name = 'files' desc, 1"
                    .into(),
                vec![],
            )
            .await?;
        Ok(rows
            .into_iter()
            .filter_map(|r| Some(SchemaInfo { name: r[0].clone()?, is_default: r[1].as_deref() == Some("true") }))
            .collect())
    }

    async fn list_objects(&self, schema: &str) -> Result<Vec<DbObject>> {
        let (db, sch) = schema.split_once('.').ok_or_else(|| ConnectorError::query("expected database.schema"))?;
        let rows = self
            .strings(
                "select table_name, 'table', comment, estimated_size::varchar from duckdb_tables() where database_name = ? and schema_name = ? \
                 union all select view_name, 'view', comment, null from duckdb_views() where database_name = ? and schema_name = ? and not internal \
                 order by 2, 1"
                    .into(),
                vec![db.into(), sch.into(), db.into(), sch.into()],
            )
            .await?;
        Ok(rows
            .into_iter()
            .filter_map(|r| {
                Some(DbObject {
                    schema: schema.to_string(),
                    name: r[0].clone()?,
                    kind: if r[1].as_deref() == Some("view") { ObjectKind::View } else { ObjectKind::Table },
                    comment: r[2].clone().filter(|c| !c.is_empty()),
                    row_estimate: r[3].as_deref().and_then(|n| n.parse().ok()),
                })
            })
            .collect())
    }

    async fn describe(&self, schema: &str, name: &str) -> Result<ObjectDetail> {
        let object = self
            .list_objects(schema)
            .await?
            .into_iter()
            .find(|o| o.name == name)
            .ok_or_else(|| ConnectorError::query(format!("object not found: {schema}.{name}")))?;
        let columns = self.columns(schema, Some(name)).await?.into_iter().next().map(|t| t.columns).unwrap_or_default();
        let (db, sch) = schema.split_once('.').unwrap_or((schema, "main"));
        let ddl = self
            .strings(
                "select sql from duckdb_tables() where database_name = ? and schema_name = ? and table_name = ? \
                 union all select sql from duckdb_views() where database_name = ? and schema_name = ? and view_name = ?"
                    .into(),
                vec![db.into(), sch.into(), name.into(), db.into(), sch.into(), name.into()],
            )
            .await?
            .into_iter()
            .next()
            .and_then(|r| r[0].clone());
        Ok(ObjectDetail { object, columns, ddl, foreign_keys: vec![] })
    }

    async fn schema_columns(&self, schema: &str) -> Result<Vec<TableColumns>> {
        self.columns(schema, None).await
    }
}

impl DuckSession {
    async fn columns(&self, schema: &str, table: Option<&str>) -> Result<Vec<TableColumns>> {
        let (db, sch) = schema.split_once('.').ok_or_else(|| ConnectorError::query("expected database.schema"))?;
        let rows = self
            .strings(
                "select table_name, column_name, data_type, is_nullable::varchar, comment from duckdb_columns() \
                 where database_name = ? and schema_name = ? and (? = '' or table_name = ?) order by table_name, column_index"
                    .into(),
                vec![db.into(), sch.into(), table.unwrap_or("").into(), table.unwrap_or("").into()],
            )
            .await?;
        let mut out: Vec<TableColumns> = Vec::new();
        for r in rows {
            let t = r[0].clone().unwrap_or_default();
            let col = ColumnInfo {
                name: r[1].clone().unwrap_or_default(),
                data_type: r[2].clone().unwrap_or_default(),
                nullable: r[3].as_deref() != Some("false"),
                is_primary_key: false,
                default: None,
                comment: r[4].clone().filter(|c| !c.is_empty()),
            };
            match out.last_mut() {
                Some(x) if x.table == t => x.columns.push(col),
                _ => out.push(TableColumns { table: t, columns: vec![col], foreign_keys: vec![] }),
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use databrain_auth::{AuthMethod, InlineCredentialSource};
    use databrain_connector_core::arrow::array::AsArray;
    use databrain_connector_core::arrow::datatypes::Int64Type;

    async fn session(cfg: ConnectionConfig) -> Box<dyn Session> {
        DuckdbConnector.connect(&cfg, Arc::new(InlineCredentialSource::new(AuthMethod::None, None))).await.unwrap()
    }

    async fn q(s: &dyn Session, sql: &str) -> databrain_connector_core::Collected {
        s.execute(sql, ExecOptions::default()).await.unwrap().collect().await.unwrap()
    }

    #[test]
    fn detects_formats_and_names() {
        assert_eq!(detect_format("/x/sales.CSV"), Some(FileFormat::Csv));
        assert_eq!(detect_format("/x/a.jsonl.gz"), Some(FileFormat::Json));
        assert_eq!(detect_format("/x/a.xlsx"), Some(FileFormat::Excel));
        assert_eq!(detect_format("/x/a.bin"), None);
        assert_eq!(view_name("/x/2024 Sales.csv", &[]), "f_2024_sales");
        assert_eq!(view_name("/x/sales.csv", &["sales".into()]), "sales_2");
        assert_eq!(scan_expr("/x/it's.csv", FileFormat::Csv), "read_csv('/x/it''s.csv')");
    }

    #[tokio::test]
    async fn queries_csv_parquet_json_files() {
        let dir = tempfile::tempdir().unwrap();
        let csv = dir.path().join("sales.csv");
        std::fs::write(&csv, "region,amount,day\nEU,10,2024-01-02\nUS,5,2024-01-03\nEU,2.5,2024-01-04\n").unwrap();
        let json = dir.path().join("events.ndjson");
        std::fs::write(&json, "{\"id\":1,\"kind\":\"a\"}\n{\"id\":2,\"kind\":\"b\"}\n").unwrap();
        let parquet = dir.path().join("out.parquet");

        let mut cfg = ConnectionConfig::new(ConnectorKind::Duckdb, AuthMethod::None);
        cfg.options.insert("files".into(), format!("{}\n{}", csv.display(), json.display()));
        let s = session(cfg).await;

        // Attached files appear in the explorer as views.
        let schemas = s.list_schemas().await.unwrap();
        let files_schema = schemas.iter().find(|x| x.name.ends_with(".files")).unwrap().name.clone();
        let objs = s.list_objects(&files_schema).await.unwrap();
        assert_eq!(objs.iter().map(|o| o.name.as_str()).collect::<Vec<_>>(), vec!["events", "sales"]);
        let d = s.describe(&files_schema, "sales").await.unwrap();
        assert_eq!(d.columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), vec!["region", "amount", "day"]);
        assert!(d.columns[2].data_type.contains("DATE"));
        assert!(s.schema_columns(&files_schema).await.unwrap().len() == 2);

        let r = q(s.as_ref(), "select region, sum(amount) s from files.sales group by 1 order by 1").await;
        assert_eq!(r.num_rows(), 2);
        // Direct file query + write Parquet + read it back.
        q(s.as_ref(), &format!("copy (select * from '{}') to '{}' (format parquet)", csv.display(), parquet.display())).await;
        let r = q(s.as_ref(), &format!("select count(*) n from read_parquet('{}')", parquet.display())).await;
        assert_eq!(r.batches[0].column(0).as_primitive::<Int64Type>().value(0), 3);
        let r = q(s.as_ref(), "select * from files.events").await;
        assert_eq!(r.num_rows(), 2);
        // DDL/DML report affected rows.
        let r = q(s.as_ref(), "create table t as select * from range(5)").await;
        assert!(r.schema.is_none());
        let r = q(s.as_ref(), "delete from t where range < 2").await;
        assert_eq!(r.summary.rows_affected, Some(2));
        // Empty result still has a schema.
        let r = q(s.as_ref(), "select * from t where false").await;
        assert_eq!(r.schema.unwrap().fields().len(), 1);
    }

    #[tokio::test]
    async fn batches_and_cancel() {
        let s = session(ConnectionConfig::new(ConnectorKind::Duckdb, AuthMethod::None)).await;
        let r = s
            .execute("select * from range(5000)", ExecOptions { batch_size: 1000, cancel: Default::default() })
            .await
            .unwrap()
            .collect()
            .await
            .unwrap();
        assert_eq!(r.num_rows(), 5000);
        assert!(r.batches.iter().all(|b| b.num_rows() <= 1000));
        let cancel = databrain_connector_core::CancellationToken::new();
        let st = s
            .execute("select count(*) from range(10000000000) a, range(1000) b", ExecOptions { batch_size: 10, cancel: cancel.clone() })
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        cancel.cancel();
        let e = tokio::time::timeout(std::time::Duration::from_secs(10), st.collect()).await.unwrap().unwrap_err();
        assert!(e.is_cancelled(), "{e:?}");
        assert_eq!(q(s.as_ref(), "select 1").await.num_rows(), 1);
    }

    #[tokio::test]
    async fn bad_attached_file_is_config_error() {
        let mut cfg = ConnectionConfig::new(ConnectorKind::Duckdb, AuthMethod::None);
        cfg.options.insert("files".into(), "/definitely/missing.csv".into());
        let e = DuckdbConnector.connect(&cfg, Arc::new(InlineCredentialSource::new(AuthMethod::None, None))).await.err().unwrap();
        assert_eq!(e.kind, ErrorKind::Config);
        assert!(e.message.contains("missing.csv"));
    }
}
