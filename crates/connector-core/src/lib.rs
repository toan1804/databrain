//! Core abstractions shared by every DataBrain connector.
//!
//! Each connector turns native driver results into Arrow record batches sent
//! over a [`QueryStream`], so the grid, filtering and export are engine-agnostic.

pub mod catalog;
pub mod conn_state;
pub mod documents;
pub mod error;
pub mod external;
#[doc(hidden)]
pub mod invariants;
pub mod language;
pub mod quote;
pub mod sql;
pub mod stream;
pub mod tables;
pub mod traits;
pub mod types;
pub mod value;

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use databrain_auth::{AuthContext, CredentialSource};

pub use arrow;
pub use catalog::*;
pub use conn_state::ConnState;
pub use error::{ConnectorError, ErrorKind, Result};
pub use quote::*;
pub use sqlparser;
pub use stream::{Collected, ExecOptions, ExecSummary, QueryStream, StreamEvent, StreamSender, memory_size};
pub use tokio_util::sync::CancellationToken;
pub use traits::{QueryLanguage, SourceTraits};
pub use types::*;
pub use value::{BatchBuilder, ColType, Column, Value};

/// Connection option set by the query engine, never by the user: the
/// session runs SQL from the AI assistant or an MCP client, so a connector
/// that can (DuckDB) denies it file and network access and locks settings.
pub const SANDBOX_OPTION: &str = "databrain_sandbox";

/// Connection option: seconds opening a session may take, overriding the
/// global setting (a VPN database may need more, a local one less).
pub const CONNECT_TIMEOUT_OPTION: &str = "connect_timeout_secs";
/// Connection option: minutes after which its unused sessions close,
/// overriding the global setting (`0` = never).
pub const IDLE_DISCONNECT_OPTION: &str = "idle_disconnect_minutes";
/// Options about DataBrain's own session handling: not sent to the
/// server, and not part of what the connection points at.
pub const SESSION_OPTIONS: [&str; 2] = [CONNECT_TIMEOUT_OPTION, IDLE_DISCONNECT_OPTION];

#[async_trait]
pub trait Connector: Send + Sync {
    fn info(&self) -> ConnectorInfo;

    fn kind(&self) -> ConnectorKind {
        self.info().kind
    }

    /// Check settings before saving or testing: a short message naming what
    /// is missing (`Choose a database file`). Required form fields
    /// ([`FieldSpec::required`]) are checked by the app already.
    fn validate(&self, _cfg: &ConnectionConfig) -> std::result::Result<(), String> {
        Ok(())
    }

    /// OAuth endpoints / CLI hints used to build the credential source.
    fn auth_context(&self, _cfg: &ConnectionConfig) -> AuthContext {
        AuthContext::default()
    }

    /// One-time local setup before the first connect, such as downloading a
    /// driver helper. It is not counted in the connect time limit, which
    /// only covers reaching and signing in to the server.
    async fn prepare(&self, _cfg: &ConnectionConfig) -> Result<()> {
        Ok(())
    }

    /// Open a new session. Credentials are fetched from `creds` right before
    /// connecting and are not retained by the connector.
    async fn connect(&self, cfg: &ConnectionConfig, creds: Arc<dyn CredentialSource>) -> Result<Box<dyn Session>>;
}

/// Whether a session has an open transaction ([`Session::transaction_state`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxState {
    /// None open: the session can be closed or replaced.
    None,
    /// One is open.
    Open,
    /// The connector can't tell (treated as open).
    Unknown,
}

/// A live database session. Sessions are shared behind `Arc`, so methods take
/// `&self`; implementations serialize access internally where the driver
/// requires it.
#[async_trait]
pub trait Session: Send + Sync {
    fn kind(&self) -> ConnectorKind;

    async fn server_version(&self) -> Result<String>;

    async fn ping(&self) -> Result<()>;

    /// Whether a transaction is open on the session. Every connector should
    /// say: [`TxState::None`] where the engine has no transactions spanning
    /// statements, [`TxState::Open`]/[`TxState::None`] from the driver or
    /// from [`conn_state::ConnState`] elsewhere. The default, `Unknown`, is
    /// treated as open (the session is kept, not idle-closed or replaced),
    /// so a connector that doesn't implement this never loses work.
    fn transaction_state(&self) -> TxState {
        TxState::Unknown
    }

    /// A transaction is (or may be) open on the session: statements on
    /// another session would not see its uncommitted changes, and dropping
    /// the session would lose them.
    fn in_transaction(&self) -> bool {
        self.transaction_state() != TxState::None
    }

    /// Execute a single statement and stream its result.
    async fn execute(&self, sql: &str, opts: ExecOptions) -> Result<QueryStream>;

    async fn list_schemas(&self) -> Result<Vec<SchemaInfo>>;

    async fn list_objects(&self, schema: &str) -> Result<Vec<DbObject>>;

    /// Top level of three-level engines (Databricks catalogs, Snowflake
    /// databases, BigQuery projects, DuckDB databases), so the explorer can
    /// show them before any schema is listed. `None` = two-level engine.
    async fn list_catalogs(&self) -> Result<Option<Vec<CatalogInfo>>> {
        Ok(None)
    }

    /// Schemas of one catalog (ids as in [`Session::list_schemas`]). The
    /// default filters the full schema list.
    async fn list_catalog_schemas(&self, catalog: &str) -> Result<Vec<SchemaInfo>> {
        Ok(self.list_schemas().await?.into_iter().filter(|s| s.catalog.as_deref() == Some(catalog)).collect())
    }

    /// Fingerprint per schema of everything the explorer lists (tables,
    /// views, routines, sequences, columns, comments), in one catalog query:
    /// unchanged = the cached listing is still right, and an index run
    /// skips the schema. `None` = not supported (every schema is re-read).
    async fn explorer_fingerprints(&self) -> Result<Option<HashMap<String, String>>> {
        Ok(None)
    }

    /// Opaque version of every object [`Session::list_objects`] returns, keyed
    /// by [`version_key`]; it changes when the object's definition, columns
    /// or comment change. Lets a changed schema be refreshed table by table
    /// (only changed tables are fetched; their cached columns go stale).
    /// Must cover exactly the listed objects. `None` = not supported.
    async fn object_versions(&self, _schema: &str) -> Result<Option<HashMap<String, String>>> {
        Ok(None)
    }

    /// [`Session::list_objects`] limited to `names` (changed objects after a
    /// version diff). The default lists the schema and filters.
    async fn list_objects_named(&self, schema: &str, names: &[String]) -> Result<Vec<DbObject>> {
        let want: std::collections::HashSet<&str> = names.iter().map(String::as_str).collect();
        Ok(self.list_objects(schema).await?.into_iter().filter(|o| want.contains(o.name.as_str())).collect())
    }

    async fn describe(&self, schema: &str, name: &str) -> Result<ObjectDetail>;

    /// Full DDL of an object (explorer "Show DDL"). `None` when the engine
    /// can't produce it. The default uses [`Session::describe`] for tables
    /// and views; engines override it for complete table DDL (constraints,
    /// indexes, comments) and for routines.
    async fn object_ddl(&self, schema: &str, name: &str, kind: ObjectKind) -> Result<Option<String>> {
        if !kind.is_relation() {
            return Ok(None);
        }
        Ok(self.describe(schema, name).await?.ddl)
    }

    /// Find tables and views by name across every schema (catalog search).
    /// The default walks schemas one by one; engines with a global catalog
    /// view override it with a single query.
    async fn search_objects(&self, query: &str, limit: usize) -> Result<Vec<DbObject>> {
        search_by_listing(self, query, limit).await
    }

    /// Tables/views of one schema whose name contains `query` (empty = any),
    /// best matches first, at most `limit` (editor completion). The default
    /// lists the schema; engines with a catalog view filter server-side so
    /// huge schemas are never transferred whole.
    async fn search_schema(&self, schema: &str, query: &str, limit: usize) -> Result<Vec<DbObject>> {
        search_schema_by_listing(self, schema, query, limit).await
    }

    /// Functions, procedures and packages whose name contains `query`, in
    /// `schema` (or the default schema when `None`), best matches first
    /// (editor completion). The default lists the schema; engines with a
    /// routine catalog override it with one filtered query.
    async fn search_routines(&self, schema: Option<&str>, query: &str, limit: usize) -> Result<Vec<DbObject>> {
        search_routines_by_listing(self, schema, query, limit).await
    }

    /// Members (functions/procedures) of a package (Oracle). Empty elsewhere.
    async fn package_members(&self, _schema: &str, _package: &str) -> Result<Vec<DbObject>> {
        Ok(vec![])
    }

    /// Objects and columns of several schemas, for the AI knowledge index.
    /// Engines with a catalog-wide information schema override this to use
    /// a couple of queries per catalog instead of two per schema. Errors for
    /// one schema are reported in its entry.
    async fn bulk_metadata(&self, schemas: &[String]) -> Result<Vec<SchemaMetadata>> {
        default_bulk_metadata(self, schemas).await
    }

    /// Number of tables/views per schema id, when the engine can count them
    /// in one cheap query (used to size the knowledge index before running it).
    async fn schema_object_counts(&self) -> Result<Option<HashMap<String, usize>>> {
        Ok(None)
    }

    /// Check a query with the database without running it (`EXPLAIN`, no
    /// `ANALYZE`): unknown tables/columns and type errors come back as the
    /// error, with `position` (1-based character in `sql`) when the engine
    /// gives one. `Ok(false)` = this engine has no cheap check.
    async fn check_sql(&self, sql: &str) -> Result<bool> {
        let Some(prefix) = self.kind().traits().explain else { return Ok(false) };
        let shift = |mut e: ConnectorError| {
            e.position = e.position.and_then(|p| p.checked_sub(prefix.chars().count() as u32)).filter(|p| *p > 0);
            e
        };
        let stream = self.execute(&format!("{prefix}{sql}"), ExecOptions::default()).await.map_err(shift)?;
        stream.collect().await.map_err(shift)?;
        Ok(true)
    }

    /// Indexes, partitioning and clustering of a table (editor query hints).
    /// The default reports the primary key from [`Session::describe`].
    async fn table_layout(&self, schema: &str, name: &str) -> Result<TableLayout> {
        Ok(TableLayout::from_columns(&self.describe(schema, name).await?.columns))
    }

    /// What a statement would scan, asked of the database without running
    /// it (BigQuery dry run, Snowflake and Databricks `EXPLAIN`): the
    /// editor's cost preview. `Ok(None)` = this engine has no estimate
    /// (`SourceTraits::cost_preview` is false).
    async fn estimate_cost(&self, _sql: &str) -> Result<Option<CostEstimate>> {
        Ok(None)
    }

    //// Columns of every table/view in a schema (bulk, for the AI knowledge
    /// index). The default describes objects one by one.
    async fn schema_columns(&self, schema: &str) -> Result<Vec<TableColumns>> {
        let mut out = Vec::new();
        for o in self.list_objects(schema).await? {
            if matches!(o.kind, ObjectKind::Function | ObjectKind::Procedure | ObjectKind::Sequence) {
                continue;
            }
            if let Ok(d) = self.describe(schema, &o.name).await {
                out.push(TableColumns { table: o.name, columns: d.columns, foreign_keys: d.foreign_keys });
            }
        }
        Ok(out)
    }
}

/// Connectors available in this build (populated by the app based on
/// enabled Cargo features).
#[derive(Default, Clone)]
pub struct ConnectorRegistry {
    map: HashMap<ConnectorKind, Arc<dyn Connector>>,
}

impl ConnectorRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, c: Arc<dyn Connector>) {
        self.map.insert(c.kind(), c);
    }

    pub fn get(&self, kind: ConnectorKind) -> Result<Arc<dyn Connector>> {
        self.map.get(&kind).cloned().ok_or_else(|| ConnectorError::unsupported(format!("connector '{}' is not enabled in this build", kind.as_str())))
    }

    pub fn infos(&self) -> Vec<ConnectorInfo> {
        let mut v: Vec<_> = self.map.values().map(|c| c.info()).collect();
        v.sort_by_key(|i| i.display_name);
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_and_quotes_typed_paths() {
        let pg = ConnectorKind::Postgres;
        assert_eq!(split_path(pg, "sales.orders"), vec!["sales", "orders"]);
        assert_eq!(split_path(pg, r#" "My ""Schema"".x" . t "#), vec![r#"My "Schema".x"#, "t"]);
        assert_eq!(split_path(ConnectorKind::Mssql, "[db].[dbo].[a]]b]"), vec!["db", "dbo", "a]b"]);
        assert_eq!(split_path(ConnectorKind::Bigquery, "`proj.ds.t`"), vec!["proj", "ds", "t"]);
        assert_eq!(split_path(ConnectorKind::Mysql, "`a.b`.c"), vec!["a.b", "c"], "a dot inside quotes (not BigQuery)");
        assert_eq!(split_path(pg, "a..b"), vec!["a", "", "b"]);
        assert!(split_path(pg, "  ").is_empty());
        assert_eq!(split_path(pg, r#"we"ird.t"#), vec![r#"we"ird"#, "t"], "a quote inside a word is a character");
        // Backslash-escaping engines: quotes and backslashes in a name stay inside it.
        for kind in [ConnectorKind::Clickhouse, ConnectorKind::Bigquery] {
            for name in ["a\\", "x\\` ; DROP TABLE t; --", "back`tick", "plain"] {
                let q = quote_ident(kind, name);
                assert!(q.starts_with('`') && q.ends_with('`'), "{q}");
                assert_eq!(split_path(kind, &q), vec![name.to_string()], "{kind:?}: {q}");
            }
            assert_eq!(quote_ident(kind, "a\\"), "`a\\\\`");
            assert_eq!(quote_ident(kind, "a`b"), "`a\\`b`");
        }
        assert_eq!(split_path(ConnectorKind::Clickhouse, "`d\\`b`.`t\\\\`"), vec!["d`b", "t\\"]);
        // Typed quotes are not quoted twice.
        assert_eq!(quote_path(pg, r#""My Schema".t"#), r#""My Schema"."t""#);
        assert_eq!(quote_path(pg, "main.app.t"), r#""main"."app"."t""#);
        // A catalog name is one identifier, dots and quotes included.
        assert_eq!(quote_object(pg, "public", "a.b"), r#""public"."a.b""#);
        assert_eq!(quote_object(pg, "db.sch", r#"x"y"#), r#""db"."sch"."x""y""#);
        assert_eq!(quote_object(pg, "", "t.1"), r#""t.1""#);
    }

    #[test]
    fn assembles_flat_rows_per_schema() {
        let col = |n: &str| ColumnInfo { name: n.into(), data_type: "int".into(), nullable: true, is_primary_key: false, default: None, comment: None };
        let obj = |s: &str, n: &str| DbObject { schema: s.into(), name: n.into(), kind: ObjectKind::Table, comment: None, row_estimate: None };
        let fk = ForeignKey { columns: vec!["a_id".into()], ref_schema: "a".into(), ref_table: "t".into(), ref_columns: vec!["id".into()] };
        let m = assemble_metadata(
            &["a".into(), "b".into(), "empty".into()],
            vec![obj("a", "t"), obj("b", "u"), obj("zz", "ignored")],
            vec![
                ("a".into(), "t".into(), col("id")),
                ("b".into(), "u".into(), col("x")),
                ("a".into(), "t".into(), col("name")),
                ("b".into(), "u".into(), col("a_id")),
            ],
            vec![("b".into(), "u".into(), fk.clone()), ("b".into(), "missing".into(), fk.clone())],
        );
        assert_eq!(m.iter().map(|s| s.schema.as_str()).collect::<Vec<_>>(), vec!["a", "b", "empty"]);
        assert_eq!(
            m[0].columns[0].columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
            vec!["id", "name"],
            "non-adjacent rows still grouped, in order"
        );
        assert_eq!(m[1].columns[0].foreign_keys, vec![fk]);
        assert!(m[2].objects.is_empty() && m[2].columns.is_empty() && m[2].error.is_none());
    }
}

#[cfg(test)]
mod explorer_tests {
    use super::*;

    #[test]
    fn version_keys_match_serde() {
        for k in [ObjectKind::Table, ObjectKind::MaterializedView, ObjectKind::ForeignTable, ObjectKind::Sequence, ObjectKind::Package, ObjectKind::Other] {
            assert_eq!(serde_json::to_value(k).unwrap().as_str().unwrap(), k.as_str());
        }
        assert_eq!(version_key(ObjectKind::View, "v1"), "view:v1");
    }
}
