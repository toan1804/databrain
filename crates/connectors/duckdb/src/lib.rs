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
use databrain_connector_core::external::{ExternalTablesSlot, RESULTS_SCHEMA, referenced_results};
use databrain_connector_core::{
    Capabilities, ColumnInfo, ConnectionConfig, Connector, ConnectorError, ConnectorInfo, ConnectorKind, DbObject,
    ErrorKind, ExecOptions, ExecSummary, FieldSpec, ObjectDetail, ObjectKind, QueryStream, Result, SchemaInfo, Session,
    StreamEvent, StreamSender, TableColumns, quote_ident, quote_literal,
};

#[derive(Default)]
pub struct DuckdbConnector {
    outputs: Option<Arc<ExternalTablesSlot>>,
}

impl DuckdbConnector {
    pub fn new() -> Self {
        Self::default()
    }

    /// Let sessions query DataBrain outputs as `results.<name>`.
    pub fn with_outputs(slot: Arc<ExternalTablesSlot>) -> Self {
        Self { outputs: Some(slot) }
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

/// Like [`scan_expr`], but Excel column types are checked against every row
/// (see [`excel_scan`]). Opens a private in-memory DuckDB for the check.
pub fn file_scan_expr(path: &str, format: FileFormat) -> String {
    if format != FileFormat::Excel {
        return scan_expr(path, format);
    }
    let Ok(conn) = duckdb::Connection::open_in_memory() else { return scan_expr(path, format) };
    if let Some(dir) = extension_dir() {
        let _ = conn.execute_batch(&format!("SET extension_directory = {}", quote_literal(&dir)));
    }
    load_extension(&conn, "excel");
    excel_scan(&conn, path)
}

/// Read an .xlsx file with column types that fit every row.
///
/// DuckDB's `read_xlsx` takes each column's type from the first data row, so
/// a column that starts with a number (or an empty cell) becomes DOUBLE and
/// fails later on text such as `A-12` (or turns `0042` into 42). This checks
/// each typed column over the whole sheet and reads the columns that do not
/// fit as text. When every column fits, plain `read_xlsx(path)` is returned.
pub fn excel_scan(conn: &duckdb::Connection, path: &str) -> String {
    let lit = quote_literal(path);
    let plain = format!("read_xlsx({lit})");
    let text = format!("read_xlsx({lit}, all_varchar = true)");
    let describe = |src: &str| -> Option<Vec<(String, String)>> {
        let mut st = conn.prepare(&format!("DESCRIBE SELECT * FROM {src}")).ok()?;
        let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))).ok()?;
        rows.collect::<std::result::Result<Vec<_>, _>>().ok()
    };
    let Some(typed) = describe(&plain) else { return plain };
    let Some(texts) = describe(&text) else { return plain };
    if typed.iter().map(|c| &c.0).ne(texts.iter().map(|c| &c.0)) {
        // Header detection differed: text for everything is the safe choice.
        return text;
    }
    let qi = |n: &str| quote_ident(ConnectorKind::Duckdb, n);
    // Cells (as text) that do not fit the inferred type. Number, date and
    // time cells come through as numbers (dates as Excel serial numbers).
    let misfit = |name: &str, ty: &str| -> Option<String> {
        let v = qi(name);
        let present = format!("{v} IS NOT NULL AND trim({v}) <> ''");
        match ty {
            "VARCHAR" => None,
            "BOOLEAN" => Some(format!("{present} AND lower(trim({v})) NOT IN ('true', 'false', '1', '0')")),
            // Leading zeros (`0042`) mean an identifier stored as text.
            _ => Some(format!("{present} AND (TRY_CAST({v} AS DOUBLE) IS NULL OR regexp_matches(trim({v}), '^[+-]?0[0-9]'))")),
        }
    };
    let checks: Vec<(usize, String)> = typed.iter().enumerate().filter_map(|(i, (n, t))| misfit(n, t).map(|w| (i, w))).collect();
    if checks.is_empty() {
        return plain;
    }
    let agg = checks.iter().map(|(_, w)| format!("count(*) FILTER (WHERE {w})")).collect::<Vec<_>>().join(", ");
    let bad: Vec<bool> = match conn.prepare(&format!("SELECT {agg} FROM {text}")).and_then(|mut st| {
        st.query_row([], |r| (0..checks.len()).map(|i| r.get::<_, i64>(i).map(|n| n > 0)).collect::<std::result::Result<Vec<_>, _>>())
    }) {
        Ok(b) => b,
        Err(_) => return plain,
    };
    if !bad.contains(&true) {
        return plain;
    }
    let mut to_text = vec![false; typed.len()];
    for ((i, _), b) in checks.iter().zip(bad) {
        to_text[*i] = b;
    }
    // Read everything as text once and convert the columns that fit.
    let cols = typed
        .iter()
        .zip(to_text)
        .map(|((n, t), as_text)| {
            let v = qi(n);
            let num = format!("TRY_CAST({v} AS DOUBLE)");
            // Excel serial numbers count days from 1899-12-30.
            let expr = match t.as_str() {
                // Mixed date column: show date cells as dates, keep the text.
                "DATE" if as_text => format!("coalesce(strftime(DATE '1899-12-30' + CAST(floor({num}) AS INTEGER), '%Y-%m-%d'), {v})"),
                "TIMESTAMP" if as_text => format!(
                    "coalesce(strftime(TIMESTAMP '1899-12-30' + to_microseconds(CAST(round({num} * 86400000000) AS BIGINT)), '%Y-%m-%d %H:%M:%S'), {v})"
                ),
                _ if as_text => return v,
                "VARCHAR" => return v,
                "BOOLEAN" => format!("CASE lower(trim({v})) WHEN 'true' THEN true WHEN '1' THEN true WHEN 'false' THEN false WHEN '0' THEN false END"),
                "DATE" => format!("(DATE '1899-12-30' + CAST(floor({num}) AS INTEGER))"),
                "TIMESTAMP" => format!("(TIMESTAMP '1899-12-30' + to_microseconds(CAST(round({num} * 86400000000) AS BIGINT)))"),
                "TIME" => format!("CAST(TIMESTAMP '1899-12-30' + to_microseconds(CAST(round(({num} - floor({num})) * 86400000000) AS BIGINT)) AS TIME)"),
                "DOUBLE" => num,
                other => format!("TRY_CAST({v} AS {other})"),
            };
            format!("{expr} AS {v}")
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!("(SELECT {cols} FROM {text})")
}

/// Folder with DuckDB extensions shipped in the app (`<dir>/v1.x.y/<platform>/*.duckdb_extension`).
static EXTENSION_DIR: std::sync::RwLock<Option<String>> = std::sync::RwLock::new(None);

/// Use `dir` as DuckDB's extension directory for new sessions (the app
/// seeds it with the bundled extensions, so nothing is downloaded).
pub fn set_extension_dir(dir: Option<String>) {
    if let Ok(mut g) = EXTENSION_DIR.write() {
        *g = dir;
    }
}

pub fn extension_dir() -> Option<String> {
    EXTENSION_DIR.read().ok().and_then(|g| g.clone())
}

/// DuckDB version and platform of this build, e.g. ("v1.5.6", "osx_arm64").
pub fn version_and_platform() -> Result<(String, String)> {
    let c = duckdb::Connection::open_in_memory().map_err(map_err)?;
    let v: String = c.query_row("select version()", [], |r| r.get(0)).map_err(map_err)?;
    let p: String = c.query_row("select platform from pragma_platform()", [], |r| r.get(0)).map_err(map_err)?;
    Ok((v, p))
}

/// Load an extension: from the extension directory first (bundled, works
/// offline), downloading it only when it is not there.
fn load_extension(conn: &duckdb::Connection, ext: &str) {
    if conn.execute_batch(&format!("LOAD {ext};")).is_err() {
        let _ = conn.execute_batch(&format!("INSTALL {ext}; LOAD {ext};"));
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
            note: Some("Query any file directly, e.g. SELECT * FROM 'data/*.parquet'. Excel, Delta and Iceberg support is included."),
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
            // Bundled extensions (excel, delta, iceberg, httpfs, icu…) live in
            // the app's extension directory; unknown ones still download on demand.
            if let Some(dir) = extension_dir() {
                let _ = conn.execute_batch(&format!("SET extension_directory = {}", quote_literal(&dir)));
            }
            let _ = conn.execute_batch("SET autoinstall_known_extensions = true; SET autoload_known_extensions = true;");
            attach_files(&conn, &files)?;
            Ok(conn)
        })
        .await
        .map_err(|e| ConnectorError::internal(e.to_string()))??;
        Ok(Box::new(DuckSession {
            interrupt: conn.interrupt_handle(),
            conn: Arc::new(Mutex::new(conn)),
            outputs: self.outputs.clone(),
            loaded: Arc::default(),
        }))
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
            load_extension(conn, ext);
        }
        let name = view_name(f, &taken);
        taken.push(name.clone());
        let src = if fmt == FileFormat::Excel { excel_scan(conn, f) } else { scan_expr(f, fmt) };
        let sql = format!("CREATE OR REPLACE VIEW files.{} AS SELECT * FROM {src}", quote_ident(ConnectorKind::Duckdb, &name));
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
    outputs: Option<Arc<ExternalTablesSlot>>,
    /// Output tables loaded in this session: lowercase name → version key.
    loaded: Arc<Mutex<std::collections::HashMap<String, String>>>,
}

type Loaded = Mutex<std::collections::HashMap<String, String>>;

/// Explorer schema id for DataBrain outputs (`results.<name>` in SQL).
pub const RESULTS_SCHEMA_ID: &str = "results.main";

impl DuckSession {
    /// Outputs as explorer objects (no data loaded).
    fn output_catalog(&self) -> Vec<databrain_connector_core::external::ExternalInfo> {
        self.outputs.as_ref().and_then(|s| s.get()).map(|t| t.catalog()).unwrap_or_default()
    }

    fn output_objects(&self) -> Vec<DbObject> {
        self.output_catalog()
            .into_iter()
            .map(|i| DbObject { schema: RESULTS_SCHEMA_ID.into(), name: i.name, kind: ObjectKind::Table, comment: i.comment, row_estimate: i.rows })
            .collect()
    }

    fn output_columns(&self) -> Vec<TableColumns> {
        self.output_catalog()
            .into_iter()
            .map(|i| TableColumns {
                table: i.name,
                columns: i
                    .columns
                    .into_iter()
                    .map(|(name, data_type)| ColumnInfo { name, data_type, nullable: true, is_primary_key: false, default: None, comment: None })
                    .collect(),
                foreign_keys: vec![],
            })
            .collect()
    }
}

/// Load (or refresh) the outputs `sql` references into the in-memory
/// `results` catalog of this session. Returns notices to show the user.
fn load_outputs(conn: &duckdb::Connection, sql: &str, slot: Option<&ExternalTablesSlot>, loaded: &Loaded) -> Result<Vec<String>> {
    let names = referenced_results(sql);
    if names.is_empty() {
        return Ok(vec![]);
    }
    let Some(tables) = slot.and_then(|s| s.get()) else {
        return Err(ConnectorError::query("results.* refers to DataBrain outputs, which are not available in this session"));
    };
    let mut notices = Vec::new();
    for name in names {
        let t = match tables.resolve(&name) {
            Ok(t) => t,
            Err(e) => {
                let known = tables.names();
                let hint = if known.is_empty() {
                    "No outputs yet — run a query first.".to_string()
                } else {
                    format!("Available: {}", known.iter().take(15).map(|n| format!("results.{n}")).collect::<Vec<_>>().join(", "))
                };
                return Err(ConnectorError::query(format!("results.{name}: {e}. {hint}")));
            }
        };
        if let Some(n) = &t.notice {
            notices.push(n.clone());
        }
        let key = name.to_ascii_lowercase();
        let current = loaded.lock().map_err(|_| ConnectorError::internal("lock poisoned"))?.get(&key).cloned();
        if current.as_deref() == Some(t.version_key.as_str()) {
            continue;
        }
        let attached: bool = conn
            .query_row("SELECT count(*) > 0 FROM duckdb_databases() WHERE database_name = 'results'", [], |r| r.get(0))
            .map_err(map_err)?;
        if !attached {
            conn.execute_batch(&format!("ATTACH ':memory:' AS {RESULTS_SCHEMA}")).map_err(map_err)?;
        }
        let batches: Vec<_> = t.batches.iter().map(normalize_for_duckdb).collect::<Result<_>>()?;
        let schema = batches.first().map(|b| b.schema()).unwrap_or_else(|| normalize_schema(&t.schema));
        let cols = schema
            .fields()
            .iter()
            .map(|f| format!("{} {}", quote_ident(ConnectorKind::Duckdb, f.name()), duck_type(f.data_type())))
            .collect::<Vec<_>>()
            .join(", ");
        let table = quote_ident(ConnectorKind::Duckdb, &name);
        conn.execute_batch(&format!("CREATE OR REPLACE TABLE {RESULTS_SCHEMA}.main.{table} ({cols})")).map_err(map_err)?;
        {
            let mut app = conn.appender_to_catalog_and_db(&name, RESULTS_SCHEMA, "main").map_err(map_err)?;
            for b in &batches {
                if b.num_rows() > 0 {
                    app.append_record_batch(to_arrow58(b)?).map_err(map_err)?;
                }
            }
            app.flush().map_err(map_err)?;
        }
        loaded.lock().map_err(|_| ConnectorError::internal("lock poisoned"))?.insert(key, t.version_key);
    }
    Ok(notices)
}

use databrain_connector_core::arrow::array::RecordBatch as Batch59;
use databrain_connector_core::arrow::datatypes::{DataType as Dt, Field as Field59, Schema as Schema59, SchemaRef as SchemaRef59, TimeUnit};

/// Column type DuckDB's appender handles natively, or `None` → cast to text.
fn native(dt: &Dt) -> Option<Dt> {
    Some(match dt {
        Dt::Boolean | Dt::Int8 | Dt::Int16 | Dt::Int32 | Dt::Int64 | Dt::UInt8 | Dt::UInt16 | Dt::UInt32 | Dt::UInt64 | Dt::Float32 | Dt::Float64 | Dt::Utf8 | Dt::Binary | Dt::Date32 => dt.clone(),
        Dt::Float16 => Dt::Float32,
        Dt::LargeUtf8 | Dt::Utf8View => Dt::Utf8,
        Dt::LargeBinary | Dt::BinaryView | Dt::FixedSizeBinary(_) => Dt::Binary,
        Dt::Date64 => Dt::Date32,
        Dt::Timestamp(u, None) => Dt::Timestamp(*u, None),
        Dt::Timestamp(_, Some(tz)) => Dt::Timestamp(TimeUnit::Microsecond, Some(tz.clone())),
        Dt::Time32(_) | Dt::Time64(_) => Dt::Time64(TimeUnit::Microsecond),
        Dt::Decimal128(p, _) if *p <= 38 => dt.clone(),
        Dt::Decimal32(p, s) | Dt::Decimal64(p, s) => Dt::Decimal128(*p, *s),
        Dt::Decimal256(..) => Dt::Float64,
        _ => return None,
    })
}

fn normalize_schema(s: &SchemaRef59) -> SchemaRef59 {
    Arc::new(Schema59::new(s.fields().iter().map(|f| Field59::new(f.name(), native(f.data_type()).unwrap_or(Dt::Utf8), true)).collect::<Vec<_>>()))
}

fn normalize_for_duckdb(b: &Batch59) -> Result<Batch59> {
    use databrain_connector_core::arrow::compute::cast;
    let schema = normalize_schema(&b.schema());
    let cols = b
        .columns()
        .iter()
        .zip(schema.fields())
        .map(|(c, f)| if c.data_type() == f.data_type() { Ok(c.clone()) } else { cast(c, f.data_type()) })
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|e| ConnectorError::internal(e.to_string()))?;
    Batch59::try_new(schema, cols).map_err(|e| ConnectorError::internal(e.to_string()))
}

fn duck_type(dt: &Dt) -> String {
    match dt {
        Dt::Boolean => "BOOLEAN".into(),
        Dt::Int8 => "TINYINT".into(),
        Dt::Int16 => "SMALLINT".into(),
        Dt::Int32 => "INTEGER".into(),
        Dt::Int64 => "BIGINT".into(),
        Dt::UInt8 => "UTINYINT".into(),
        Dt::UInt16 => "USMALLINT".into(),
        Dt::UInt32 => "UINTEGER".into(),
        Dt::UInt64 => "UBIGINT".into(),
        Dt::Float32 => "FLOAT".into(),
        Dt::Float64 => "DOUBLE".into(),
        Dt::Binary => "BLOB".into(),
        Dt::Date32 => "DATE".into(),
        Dt::Timestamp(TimeUnit::Second, None) => "TIMESTAMP_S".into(),
        Dt::Timestamp(TimeUnit::Millisecond, None) => "TIMESTAMP_MS".into(),
        Dt::Timestamp(TimeUnit::Nanosecond, None) => "TIMESTAMP_NS".into(),
        Dt::Timestamp(_, None) => "TIMESTAMP".into(),
        Dt::Timestamp(_, Some(_)) => "TIMESTAMPTZ".into(),
        Dt::Time64(_) => "TIME".into(),
        Dt::Decimal128(p, s) => format!("DECIMAL({p},{s})"),
        _ => "VARCHAR".into(),
    }
}

/// Workspace Arrow (59) → DuckDB's Arrow (58), via IPC.
fn to_arrow58(b: &Batch59) -> Result<arrow58::record_batch::RecordBatch> {
    let mut buf = Vec::new();
    {
        let mut w = databrain_connector_core::arrow::ipc::writer::StreamWriter::try_new(&mut buf, &b.schema()).map_err(|e| ConnectorError::internal(e.to_string()))?;
        w.write(b).map_err(|e| ConnectorError::internal(e.to_string()))?;
        w.finish().map_err(|e| ConnectorError::internal(e.to_string()))?;
    }
    let mut r = arrow58::ipc::reader::StreamReader::try_new(Cursor::new(buf), None).map_err(|e| ConnectorError::internal(e.to_string()))?;
    r.next().ok_or_else(|| ConnectorError::internal("empty IPC stream"))?.map_err(|e| ConnectorError::internal(e.to_string()))
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

fn run(conn: &duckdb::Connection, sql: &str, opts: &ExecOptions, tx: &StreamSender, outputs: Option<&ExternalTablesSlot>, loaded: &Loaded) -> Result<()> {
    for n in load_outputs(conn, sql, outputs, loaded)? {
        tx.blocking_send(Ok(StreamEvent::Notice(n)));
    }
    // Load the extension a query needs (bundled copy first).
    let lower = sql.to_ascii_lowercase();
    for (f, ext) in [("delta_scan", "delta"), ("iceberg_", "iceberg"), ("read_xlsx", "excel")] {
        if lower.contains(f) {
            load_extension(conn, ext);
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
        let (outputs, loaded) = (self.outputs.clone(), self.loaded.clone());
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
            if let Err(e) = run(&g, &sql, &opts, &tx, outputs.as_deref(), &loaded) {
                let e = if opts.cancel.is_cancelled() { ConnectorError::cancelled() } else { e };
                tx.blocking_send(Err(e));
            }
        });
        Ok(stream)
    }

    async fn list_schemas(&self) -> Result<Vec<SchemaInfo>> {
        let rows = self
            .strings(
                "select database_name, schema_name, (database_name = current_database() and schema_name = current_schema())::varchar \
                 from duckdb_schemas() where not internal and schema_name <> 'pg_catalog' and database_name <> 'results' \
                 order by database_name = current_database() desc, schema_name = 'files' desc, 1, 2"
                    .into(),
                vec![],
            )
            .await?;
        Ok(rows
            .into_iter()
            .filter_map(|r| Some(SchemaInfo::in_catalog(r[0].clone()?, r[1].as_deref()?, r[2].as_deref() == Some("true"))))
            .chain((!self.output_catalog().is_empty()).then(|| SchemaInfo::in_catalog("results", "main", false)))
            .collect())
    }

    async fn schema_object_counts(&self) -> Result<Option<std::collections::HashMap<String, usize>>> {
        let rows = self
            .strings(
                "select s, count(*)::varchar from (select database_name || '.' || schema_name s from duckdb_tables() where not internal \
                 union all select database_name || '.' || schema_name from duckdb_views() where not internal) group by 1"
                    .into(),
                vec![],
            )
            .await?;
        let mut counts: std::collections::HashMap<String, usize> = rows
            .into_iter()
            .filter_map(|r| Some((r[0].clone()?, r[1].as_deref()?.parse().ok()?)))
            .filter(|(s, _): &(String, usize)| !s.starts_with("results."))
            .collect();
        let n = self.output_catalog().len();
        if n > 0 {
            counts.insert(RESULTS_SCHEMA_ID.into(), n);
        }
        Ok(Some(counts))
    }

    async fn search_objects(&self, query: &str, limit: usize) -> Result<Vec<DbObject>> {
        let term = databrain_connector_core::search_sql_term(query);
        let rows = self
            .strings(
                "select database_name || '.' || schema_name, table_name, 'table', comment, estimated_size::varchar from duckdb_tables() \
                 where not internal and database_name <> 'results' and contains(lower(table_name), ?) \
                 union all select database_name || '.' || schema_name, view_name, 'view', comment, null from duckdb_views() \
                 where not internal and database_name <> 'results' and contains(lower(view_name), ?) limit 5000"
                    .into(),
                vec![term.clone(), term],
            )
            .await?;
        let mut hits: Vec<DbObject> = rows
            .into_iter()
            .filter_map(|r| {
                Some(DbObject {
                    schema: r[0].clone()?,
                    name: r[1].clone()?,
                    kind: if r[2].as_deref() == Some("view") { ObjectKind::View } else { ObjectKind::Table },
                    comment: r[3].clone().filter(|c| !c.is_empty()),
                    row_estimate: r[4].as_deref().and_then(|n| n.parse().ok()),
                })
            })
            .chain(self.output_objects())
            .filter(|o| databrain_connector_core::object_matches(query, &o.schema, &o.name))
            .collect();
        databrain_connector_core::rank_matches(query, &mut hits, limit);
        Ok(hits)
    }

    async fn list_objects(&self, schema: &str) -> Result<Vec<DbObject>> {
        if schema == RESULTS_SCHEMA_ID {
            return Ok(self.output_objects());
        }
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
        if schema == RESULTS_SCHEMA_ID {
            let object = self.output_objects().into_iter().find(|o| o.name == name).ok_or_else(|| ConnectorError::query(format!("output {name} not found")))?;
            let columns = self.output_columns().into_iter().find(|t| t.table == name).map(|t| t.columns).unwrap_or_default();
            return Ok(ObjectDetail { object, columns, ddl: None, foreign_keys: vec![] });
        }
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
        if schema == RESULTS_SCHEMA_ID {
            return Ok(self.output_columns());
        }
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
        DuckdbConnector::new().connect(&cfg, Arc::new(InlineCredentialSource::new(AuthMethod::None, None))).await.unwrap()
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
        let files = schemas.iter().find(|x| x.name.ends_with(".files")).unwrap();
        let files_schema = files.name.clone();
        // Three-level names: database (catalog) → schema.
        assert_eq!(files.name, format!("{}.files", files.catalog.as_deref().unwrap()));
        let hits = s.search_objects("ale", 10).await.unwrap();
        assert_eq!(hits.iter().map(|o| (o.schema.as_str(), o.name.as_str())).collect::<Vec<_>>(), vec![(files_schema.as_str(), "sales")]);
        assert_eq!(s.search_objects("files.ev", 10).await.unwrap()[0].name, "events");
        assert!(s.search_objects("x'; drop table t; --", 10).await.unwrap().is_empty());
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

    struct FakeOutputs(std::sync::Mutex<(String, i64)>);
    impl databrain_connector_core::external::ExternalTables for FakeOutputs {
        fn resolve(&self, name: &str) -> std::result::Result<databrain_connector_core::external::ExternalTable, String> {
            use databrain_connector_core::arrow::array::{Int64Array, StringArray};
            use databrain_connector_core::arrow::datatypes::{DataType, Field, Schema};
            if name != "sales" {
                return Err("unknown output".into());
            }
            let (version, n) = self.0.lock().unwrap().clone();
            let schema = Arc::new(Schema::new(vec![Field::new("region", DataType::Utf8, true), Field::new("amount", DataType::Int64, true)]));
            let regions: Vec<Option<&str>> = (0..n).map(|i| Some(if i % 2 == 0 { "EU" } else { "US" })).collect();
            let batch = databrain_connector_core::arrow::array::RecordBatch::try_new(
                schema.clone(),
                vec![Arc::new(StringArray::from(regions)), Arc::new(Int64Array::from((0..n).collect::<Vec<_>>()))],
            )
            .unwrap();
            Ok(databrain_connector_core::external::ExternalTable {
                name: name.into(),
                version_key: version,
                schema,
                batches: vec![batch],
                notice: Some("capped".into()),
            })
        }
        fn names(&self) -> Vec<String> {
            vec!["sales".into()]
        }
        fn catalog(&self) -> Vec<databrain_connector_core::external::ExternalInfo> {
            vec![databrain_connector_core::external::ExternalInfo {
                name: "sales".into(),
                columns: vec![("region".into(), "VARCHAR".into()), ("amount".into(), "BIGINT".into())],
                rows: Some(4),
                comment: Some("output r1".into()),
            }]
        }
    }

    #[tokio::test]
    async fn queries_outputs_as_results_tables() {
        let slot = ExternalTablesSlot::new();
        let fake = Arc::new(FakeOutputs(std::sync::Mutex::new(("v1".into(), 4))));
        slot.set(fake.clone());
        let s = DuckdbConnector::with_outputs(slot)
            .connect(&ConnectionConfig::new(ConnectorKind::Duckdb, AuthMethod::None), Arc::new(InlineCredentialSource::new(AuthMethod::None, None)))
            .await
            .unwrap();
        let r = q(s.as_ref(), "select region, sum(amount) from results.sales group by 1 order by 1").await;
        assert_eq!(r.num_rows(), 2);
        assert_eq!(r.notices, vec!["capped".to_string()]);
        // New version is reloaded; joins between outputs work.
        *fake.0.lock().unwrap() = ("v2".into(), 10);
        let r = q(s.as_ref(), "select count(*) from results.sales a join results.sales b using (amount)").await;
        assert_eq!(r.batches[0].column(0).as_primitive::<Int64Type>().value(0), 10);
        let e = s.execute("select * from results.nope", ExecOptions::default()).await.unwrap().collect().await.unwrap_err();
        assert!(e.message.contains("results.nope") && e.message.contains("results.sales"), "{}", e.message);
        // Explorer: outputs are listed (with columns) under results.main,
        // also after `results` was attached by the queries above.
        let schemas = s.list_schemas().await.unwrap();
        assert_eq!(schemas.iter().filter(|x| x.catalog.as_deref() == Some("results")).map(|x| x.name.as_str()).collect::<Vec<_>>(), vec![RESULTS_SCHEMA_ID]);
        let objs = s.list_objects(RESULTS_SCHEMA_ID).await.unwrap();
        assert_eq!(objs.iter().map(|o| o.name.as_str()).collect::<Vec<_>>(), vec!["sales"]);
        let d = s.describe(RESULTS_SCHEMA_ID, "sales").await.unwrap();
        assert_eq!(d.columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), vec!["region", "amount"]);
        assert_eq!(s.search_objects("sal", 5).await.unwrap()[0].schema, RESULTS_SCHEMA_ID);
        assert_eq!(s.schema_object_counts().await.unwrap().unwrap().get(RESULTS_SCHEMA_ID), Some(&1));
        // Without a provider, results.* is a clear error.
        let plain = session(ConnectionConfig::new(ConnectorKind::Duckdb, AuthMethod::None)).await;
        let e = plain.execute("select * from results.sales", ExecOptions::default()).await.unwrap().collect().await.unwrap_err();
        assert!(e.message.contains("not available"), "{}", e.message);
    }

    #[tokio::test]
    async fn bad_attached_file_is_config_error() {
        let mut cfg = ConnectionConfig::new(ConnectorKind::Duckdb, AuthMethod::None);
        cfg.options.insert("files".into(), "/definitely/missing.csv".into());
        let e = DuckdbConnector::new().connect(&cfg, Arc::new(InlineCredentialSource::new(AuthMethod::None, None))).await.err().unwrap();
        assert_eq!(e.kind, ErrorKind::Config);
        assert!(e.message.contains("missing.csv"));
    }
}

#[cfg(test)]
mod bundled_extensions {
    use super::*;

    /// With the app's extension folder set, Excel loads without any download.
    /// Runs when `crates/app/resources/duckdb-extensions` was fetched
    /// (`node scripts/fetch-duckdb-extensions.mjs`).
    #[test]
    fn loads_bundled_excel_offline() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../app/resources/duckdb-extensions");
        let (v, p) = version_and_platform().unwrap();
        if !dir.join(&v).join(&p).join("excel.duckdb_extension").is_file() {
            eprintln!("skipped: bundled extensions not fetched");
            return;
        }
        let c = duckdb::Connection::open_in_memory().unwrap();
        c.execute_batch(&format!("SET extension_directory = {}; SET autoinstall_known_extensions = false;", quote_literal(&dir.to_string_lossy()))).unwrap();
        load_extension(&c, "excel");
        let path: String = c
            .query_row("select install_path from duckdb_extensions() where extension_name = 'excel' and loaded", [], |r| r.get(0))
            .unwrap();
        assert!(path.starts_with(&*dir.to_string_lossy()), "{path}");
    }
}

#[cfg(test)]
mod excel_types {
    use super::*;
    use rust_xlsxwriter::{ExcelDateTime, Format, Workbook};

    /// Sheet whose first data row suggests numbers/dates for columns that
    /// hold text further down.
    fn mixed_sheet(path: &std::path::Path) {
        let mut wb = Workbook::new();
        let ws = wb.add_worksheet();
        let date = Format::new().set_num_format("yyyy-mm-dd");
        for (c, h) in ["id", "code", "name", "when", "amount", "day"].iter().enumerate() {
            ws.write_string(0, c as u16, *h).unwrap();
        }
        let d = |n: u8| ExcelDateTime::from_ymd(2023, 3, n).unwrap();
        ws.write_number(1, 0, 1).unwrap();
        ws.write_number(1, 1, 100).unwrap();
        ws.write_number(1, 4, 1.5).unwrap();
        ws.write_datetime_with_format(1, 3, d(15), &date).unwrap();
        ws.write_datetime_with_format(1, 5, d(15), &date).unwrap();
        ws.write_number(2, 0, 2).unwrap();
        ws.write_string(2, 1, "A-12").unwrap();
        ws.write_string(2, 2, "Bob").unwrap();
        ws.write_string(2, 3, "n/a").unwrap();
        ws.write_number(2, 4, 2).unwrap();
        ws.write_datetime_with_format(2, 5, d(16), &date).unwrap();
        ws.write_number(3, 0, 3).unwrap();
        ws.write_string(3, 1, "0042").unwrap();
        ws.write_string(3, 2, "Ann").unwrap();
        ws.write_datetime_with_format(3, 3, d(17), &date).unwrap();
        ws.write_number(3, 4, 3.25).unwrap();
        ws.write_datetime_with_format(3, 5, d(17), &date).unwrap();
        wb.save(path).unwrap();
    }

    fn conn() -> Option<duckdb::Connection> {
        let c = duckdb::Connection::open_in_memory().unwrap();
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../app/resources/duckdb-extensions");
        if dir.is_dir() {
            c.execute_batch(&format!("SET extension_directory = {}", quote_literal(&dir.to_string_lossy()))).unwrap();
        }
        load_extension(&c, "excel");
        c.execute_batch("SELECT excel_text(1, '0')").ok().map(|_| c)
    }

    #[test]
    fn mixed_columns_are_read_as_text() {
        let Some(c) = conn() else { return eprintln!("skipped: excel extension unavailable") };
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("mixed.xlsx");
        mixed_sheet(&p);
        let path = p.to_string_lossy().to_string();
        // DuckDB's own inference (first row only) fails on this sheet.
        let plain = c.prepare(&format!("SELECT * FROM read_xlsx({})", quote_literal(&path))).and_then(|mut s| s.query_arrow([]).map(|r| r.count()));
        assert!(plain.is_err(), "expected read_xlsx to fail on mixed columns");
        let src = excel_scan(&c, &path);
        let types: Vec<(String, String)> = c
            .prepare(&format!("DESCRIBE SELECT * FROM {src}"))
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        let ty = |n: &str| types.iter().find(|t| t.0 == n).map(|t| t.1.as_str()).unwrap();
        assert_eq!(ty("id"), "DOUBLE", "{src}");
        assert_eq!(ty("code"), "VARCHAR");
        assert_eq!(ty("name"), "VARCHAR", "empty first cell, text below");
        assert_eq!(ty("when"), "VARCHAR");
        assert_eq!(ty("amount"), "DOUBLE");
        assert_eq!(ty("day"), "DATE", "consistent dates stay dates");
        let rows: Vec<(String, Option<String>, String, String)> = c
            .prepare(&format!("SELECT code, name, \"when\", CAST(day AS VARCHAR) FROM {src}"))
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(rows[0], ("100".into(), None, "2023-03-15".into(), "2023-03-15".into()));
        assert_eq!(rows[1].0, "A-12");
        assert_eq!(rows[1].2, "n/a");
        assert_eq!(rows[2].0, "0042", "leading zeros kept");
    }

    #[test]
    fn consistent_sheet_uses_plain_read_xlsx() {
        let Some(c) = conn() else { return };
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("ok.xlsx");
        c.execute_batch(&format!("COPY (SELECT range AS n, 'x' || range AS s FROM range(5)) TO {} (FORMAT xlsx, HEADER true)", quote_literal(&p.to_string_lossy())))
            .unwrap();
        assert_eq!(excel_scan(&c, &p.to_string_lossy()), format!("read_xlsx({})", quote_literal(&p.to_string_lossy())));
    }
}
