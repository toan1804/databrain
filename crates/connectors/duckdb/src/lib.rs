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

mod database;
mod engine;
mod error;
mod excel;
mod exec;
mod files;
mod outputs;
mod sandbox;
mod session;
#[cfg(test)]
mod test_util;
pub mod xlsx;

pub use engine::{EngineStatus, engine_status, extension_dir, load_engine, set_extension_dir, version_and_platform};
pub use excel::{ExcelSource, excel_scan, excel_sheet_source, excel_sources, file_scan_expr};
pub use files::{FileFormat, detect_format, scan_expr, view_name};
pub use outputs::RESULTS_SCHEMA_ID;
pub use session::DuckSession;

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use databrain_auth::{AuthMethodKind, CredentialSource};
use databrain_connector_core::external::ExternalTablesSlot;
use databrain_connector_core::{
    Capabilities, ConnectionConfig, Connector, ConnectorError, ConnectorInfo, ConnectorKind, FieldSpec, Result, SANDBOX_OPTION, Session, quote_literal,
};

use database::{FileConn, FileDb, connect_file};
use engine::engine;
use files::{attach_files, files_of};
use sandbox::seal;

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

#[async_trait]
impl Connector for DuckdbConnector {
    fn info(&self) -> ConnectorInfo {
        ConnectorInfo {
            kind: ConnectorKind::Duckdb,
            traits: ConnectorKind::Duckdb.traits(),
            display_name: "DuckDB (local files)",
            default_port: None,
            uses_file: true,
            auth_methods: vec![AuthMethodKind::None],
            capabilities: Capabilities { ssh: false },
            fields: vec![
                FieldSpec::new("file_path", "Database file").placeholder(":memory: (or /path/to/db.duckdb)"),
                FieldSpec::new("files", "Attached files")
                    .placeholder("One path per line: .csv .parquet .json .xlsx, Delta/Iceberg folders")
                    .help("Each file becomes a view in the `files` schema"),
                // "first" (default) or "all": one view per sheet, <file>_<sheet>.
                FieldSpec::new("excel_sheets", "Excel sheets").placeholder("first"),
            ],
            note: Some("Query any file directly, e.g. SELECT * FROM 'data/*.parquet'. Excel, Delta and Iceberg support is included."),
        }
    }

    async fn connect(&self, cfg: &ConnectionConfig, _creds: Arc<dyn CredentialSource>) -> Result<Box<dyn Session>> {
        engine()?;
        let path = cfg.file_path.clone().filter(|p| !p.trim().is_empty()).unwrap_or_else(|| ":memory:".into());
        let files = files_of(cfg);
        let all_sheets = cfg.opt("excel_sheets") == Some("all");
        let read_only = cfg.read_only && path != ":memory:";
        let sandbox = cfg.opt(SANDBOX_OPTION) == Some("1");
        let (conn, db) = tokio::task::spawn_blocking(move || -> Result<(duckdb::Connection, Option<Arc<FileDb>>)> {
            // Settings and file views, then (sandboxed) the lock: once
            // configuration is locked nothing can be changed or read from disk.
            let setup = |conn: &duckdb::Connection| -> Result<()> {
                // Bundled extensions (excel, delta, iceberg, httpfs, icu…) live in
                // the app's extension directory; unknown ones still download on demand.
                if let Some(dir) = extension_dir() {
                    let _ = conn.execute_batch(&format!("SET extension_directory = {}", quote_literal(&dir)));
                }
                let _ = conn.execute_batch("SET autoinstall_known_extensions = true; SET autoload_known_extensions = true;");
                attach_files(conn, &files, all_sheets)?;
                if sandbox {
                    seal(conn, &files)?;
                }
                Ok(())
            };
            if path == ":memory:" {
                let conn = duckdb::Connection::open_in_memory().map_err(|e| ConnectorError::connection(e.to_string()))?;
                setup(&conn)?;
                return Ok((conn, None));
            }
            if sandbox {
                // Set up once, on the instance, before any session uses it.
                let (c, db) = connect_file(&path, read_only, true, &setup)?;
                return Ok((c, Some(db)));
            }
            let (c, db) = connect_file(&path, read_only, false, &|_| Ok(()))?;
            setup(&c)?;
            Ok((c, Some(db)))
        })
        .await
        .map_err(|e| ConnectorError::internal(e.to_string()))??;
        Ok(Box::new(DuckSession {
            interrupt: conn.interrupt_handle(),
            shared: db.is_some(),
            outputs: self.outputs.clone(),
            loaded: db.as_ref().map(|d| d.loaded.clone()).unwrap_or_default(),
            sandboxed: sandbox,
            conn: Arc::new(Mutex::new(FileConn { conn, _db: db })),
            state: Default::default(),
        }))
    }
}
