//! Core abstractions shared by every DataBrain connector.
//!
//! Each connector turns native driver results into Arrow record batches sent
//! over a [`QueryStream`], so the grid, filtering and export are engine-agnostic.

pub mod error;
pub mod external;
pub mod sql;
pub mod stream;
pub mod types;
pub mod value;

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use databrain_auth::{AuthContext, CredentialSource};

pub use arrow;
pub use error::{ConnectorError, ErrorKind, Result};
pub use stream::{Collected, ExecOptions, ExecSummary, QueryStream, StreamEvent, StreamSender};
pub use tokio_util::sync::CancellationToken;
pub use types::*;
pub use value::{BatchBuilder, ColType, Column, Value};

#[async_trait]
pub trait Connector: Send + Sync {
    fn info(&self) -> ConnectorInfo;

    fn kind(&self) -> ConnectorKind {
        self.info().kind
    }

    /// OAuth endpoints / CLI hints used to build the credential source.
    fn auth_context(&self, _cfg: &ConnectionConfig) -> AuthContext {
        AuthContext::default()
    }

    /// Open a new session. Credentials are fetched from `creds` right before
    /// connecting and are not retained by the connector.
    async fn connect(
        &self,
        cfg: &ConnectionConfig,
        creds: Arc<dyn CredentialSource>,
    ) -> Result<Box<dyn Session>>;
}

/// A live database session. Sessions are shared behind `Arc`, so methods take
/// `&self`; implementations serialize access internally where the driver
/// requires it.
#[async_trait]
pub trait Session: Send + Sync {
    fn kind(&self) -> ConnectorKind;

    async fn server_version(&self) -> Result<String>;

    async fn ping(&self) -> Result<()>;

    /// Execute a single statement and stream its result.
    async fn execute(&self, sql: &str, opts: ExecOptions) -> Result<QueryStream>;

    async fn list_schemas(&self) -> Result<Vec<SchemaInfo>>;

    async fn list_objects(&self, schema: &str) -> Result<Vec<DbObject>>;

    async fn describe(&self, schema: &str, name: &str) -> Result<ObjectDetail>;

    /// Find tables and views by name across every schema (catalog search).
    /// The default walks schemas one by one; engines with a global catalog
    /// view override it with a single query.
    async fn search_objects(&self, query: &str, limit: usize) -> Result<Vec<DbObject>> {
        search_by_listing(self, query, limit).await
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

    /// Columns of every table/view in a schema (bulk, for the AI knowledge
    /// index). The default describes objects one by one.
    async fn schema_columns(&self, schema: &str) -> Result<Vec<TableColumns>> {
        let mut out = Vec::new();
        for o in self.list_objects(schema).await? {
            if matches!(
                o.kind,
                ObjectKind::Function | ObjectKind::Procedure | ObjectKind::Sequence
            ) {
                continue;
            }
            if let Ok(d) = self.describe(schema, &o.name).await {
                out.push(TableColumns {
                    table: o.name,
                    columns: d.columns,
                    foreign_keys: d.foreign_keys,
                });
            }
        }
        Ok(out)
    }
}

/// Per-schema metadata (the default for [`Session::bulk_metadata`]).
pub async fn default_bulk_metadata<S: Session + ?Sized>(s: &S, schemas: &[String]) -> Result<Vec<SchemaMetadata>> {
    let mut out = Vec::with_capacity(schemas.len());
    for sc in schemas {
        out.push(match s.list_objects(sc).await {
            Ok(objects) => {
                let (columns, error) = match s.schema_columns(sc).await {
                    Ok(c) => (c, None),
                    Err(e) => (vec![], Some(e.message)),
                };
                SchemaMetadata { schema: sc.clone(), objects, columns, error }
            }
            Err(e) => SchemaMetadata { schema: sc.clone(), objects: vec![], columns: vec![], error: Some(e.message) },
        });
    }
    Ok(out)
}

/// Catalog search by listing every schema (the default for
/// [`Session::search_objects`]); stops early once plenty of hits are found.
pub async fn search_by_listing<S: Session + ?Sized>(s: &S, query: &str, limit: usize) -> Result<Vec<DbObject>> {
    let mut hits = Vec::new();
    for sc in s.list_schemas().await? {
        let Ok(objs) = s.list_objects(&sc.name).await else { continue };
        hits.extend(objs.into_iter().filter(|o| {
            !matches!(o.kind, ObjectKind::Function | ObjectKind::Procedure | ObjectKind::Sequence)
                && object_matches(query, &o.schema, &o.name)
        }));
        if hits.len() >= limit * 4 {
            break;
        }
    }
    rank_matches(query, &mut hits, limit);
    Ok(hits)
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
        self.map.get(&kind).cloned().ok_or_else(|| {
            ConnectorError::unsupported(format!(
                "connector '{}' is not enabled in this build",
                kind.as_str()
            ))
        })
    }

    pub fn infos(&self) -> Vec<ConnectorInfo> {
        let mut v: Vec<_> = self.map.values().map(|c| c.info()).collect();
        v.sort_by_key(|i| i.display_name);
        v
    }
}

/// Quote an identifier for the dialect.
pub fn quote_ident(kind: ConnectorKind, ident: &str) -> String {
    match kind {
        ConnectorKind::Mysql | ConnectorKind::Databricks => {
            format!("`{}`", ident.replace('`', "``"))
        }
        ConnectorKind::Bigquery => format!("`{}`", ident.replace('`', "\\`")),
        ConnectorKind::Mssql => format!("[{}]", ident.replace(']', "]]")),
        _ => format!("\"{}\"", ident.replace('"', "\"\"")),
    }
}

/// Quote a possibly qualified name (`catalog.schema` stored as one string).
pub fn quote_path(kind: ConnectorKind, path: &str) -> String {
    path.split('.')
        .map(|p| quote_ident(kind, p))
        .collect::<Vec<_>>()
        .join(".")
}

/// Quote a string literal with single quotes.
pub fn quote_literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}
