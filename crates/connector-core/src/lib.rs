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

    /// A cheap fingerprint of each schema's table/column definitions (schema
    /// id → opaque text), computed in one catalog query. When a schema's
    /// fingerprint is unchanged since the last index run its metadata is not
    /// fetched again. `None` = not supported (every schema is re-read).
    async fn schema_fingerprints(&self) -> Result<Option<HashMap<String, String>>> {
        Ok(None)
    }

    /// Number of tables/views per schema id, when the engine can count them
    /// in one cheap query (used to size the knowledge index before running it).
    async fn schema_object_counts(&self) -> Result<Option<HashMap<String, usize>>> {
        Ok(None)
    }

    /// Indexes, partitioning and clustering of a table (editor query hints).
    /// The default reports the primary key from [`Session::describe`].
    async fn table_layout(&self, schema: &str, name: &str) -> Result<TableLayout> {
        Ok(TableLayout::from_columns(&self.describe(schema, name).await?.columns))
    }

    //// Columns of every table/view in a schema (bulk, for the AI knowledge
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

/// Run a small metadata query and return every cell as text (NULL → None;
/// lists as `[a, b]`). For catalog queries in `table_layout` implementations.
pub async fn query_text<S: Session + ?Sized>(s: &S, sql: &str) -> Result<Vec<Vec<Option<String>>>> {
    Ok(query_table(s, sql).await?.1)
}

/// [`query_text`] plus the column names.
#[allow(clippy::type_complexity)]
pub async fn query_table<S: Session + ?Sized>(s: &S, sql: &str) -> Result<(Vec<String>, Vec<Vec<Option<String>>>)> {
    use arrow::array::Array;
    use arrow::util::display::{ArrayFormatter, FormatOptions};
    let c = s.execute(sql, ExecOptions::default()).await?.collect().await?;
    let opts = FormatOptions::default();
    let mut rows = Vec::new();
    for b in &c.batches {
        let fmts: Vec<ArrayFormatter> = b
            .columns()
            .iter()
            .map(|a| ArrayFormatter::try_new(a.as_ref(), &opts).map_err(|e| ConnectorError::internal(e.to_string())))
            .collect::<Result<_>>()?;
        for i in 0..b.num_rows() {
            rows.push(b.columns().iter().zip(&fmts).map(|(a, f)| (!a.is_null(i)).then(|| f.value(i).to_string())).collect());
        }
    }
    let names = c.schema.map(|sc| sc.fields().iter().map(|f| f.name().clone()).collect()).unwrap_or_default();
    Ok((names, rows))
}

/// Group one-row-per-key-column index rows `(index, column, unique, primary,
/// method)` into indexes, keeping the first-seen order.
pub fn group_indexes(rows: impl IntoIterator<Item = (String, String, bool, bool, Option<String>)>) -> Vec<IndexInfo> {
    let mut out: Vec<IndexInfo> = Vec::new();
    for (name, col, unique, primary, method) in rows {
        match out.iter_mut().find(|i| i.name == name) {
            Some(i) => i.columns.push(col),
            None => out.push(IndexInfo { name, columns: vec![col], unique: unique || primary, primary, method }),
        }
    }
    out.sort_by_key(|i| !i.primary);
    out
}

/// `[a, b]` / `["a","b"]` / `a, b` → items (Databricks/DuckDB list text).
pub fn parse_list_text(s: &str) -> Vec<String> {
    let t = s.trim();
    let t = t.strip_prefix('[').and_then(|x| x.strip_suffix(']')).unwrap_or(t);
    split_key_list(t)
        .into_iter()
        .map(|x| {
            let x = x.trim();
            let x = x.strip_prefix('\'').and_then(|y| y.strip_suffix('\'')).unwrap_or(x);
            unquote_ident(x)
        })
        .filter(|x| !x.is_empty())
        .collect()
}

/// `RANGE (a, b)` / `LINEAR(a, b)` → ("range", ["a", "b"]); no parens → ("", [text]).
pub fn parse_call_list(s: &str) -> (String, Vec<String>) {
    let t = s.trim();
    match (t.find('('), t.rfind(')')) {
        (Some(o), Some(c)) if c > o => {
            let items = split_key_list(&t[o + 1..c]).into_iter().map(|x| unquote_ident(&x)).collect();
            (t[..o].trim().to_ascii_lowercase(), items)
        }
        _ if t.is_empty() => (String::new(), vec![]),
        _ => (String::new(), vec![unquote_ident(t)]),
    }
}

/// Truthy cell text from catalog queries (`t`, `true`, `1`, `YES`, `UNIQUE`).
pub fn truthy(v: &Option<String>) -> bool {
    matches!(v.as_deref().map(|x| x.trim().to_ascii_lowercase()).as_deref(), Some("t" | "true" | "1" | "yes" | "y" | "unique"))
}

/// Per-schema metadata (the default for [`Session::bulk_metadata`]).
/// Group flat catalog rows (from one multi-schema query each) into
/// [`SchemaMetadata`] per requested schema. Linear time, whatever the size.
pub fn assemble_metadata(
    schemas: &[String],
    objects: Vec<DbObject>,
    columns: Vec<(String, String, ColumnInfo)>,
    foreign_keys: Vec<(String, String, ForeignKey)>,
) -> Vec<SchemaMetadata> {
    let mut out: Vec<SchemaMetadata> = schemas.iter().map(|s| SchemaMetadata { schema: s.clone(), objects: vec![], columns: vec![], error: None }).collect();
    let pos: HashMap<&str, usize> = schemas.iter().enumerate().map(|(i, s)| (s.as_str(), i)).collect();
    let mut tables: HashMap<(usize, String), usize> = HashMap::new();
    for o in objects {
        if let Some(&i) = pos.get(o.schema.as_str()) {
            out[i].objects.push(o);
        }
    }
    let mut cols: Vec<Vec<TableColumns>> = vec![Vec::new(); schemas.len()];
    for (schema, table, c) in columns {
        let Some(&i) = pos.get(schema.as_str()) else { continue };
        let k = (i, table);
        match tables.get(&k) {
            Some(&j) => cols[i][j].columns.push(c),
            None => {
                tables.insert(k.clone(), cols[i].len());
                cols[i].push(TableColumns { table: k.1, columns: vec![c], foreign_keys: vec![] });
            }
        }
    }
    for (schema, table, fk) in foreign_keys {
        let Some(&i) = pos.get(schema.as_str()) else { continue };
        if let Some(&j) = tables.get(&(i, table)) {
            cols[i][j].foreign_keys.push(fk);
        }
    }
    for (m, c) in out.iter_mut().zip(cols) {
        m.columns = c;
    }
    out
}

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
/// [`Session::search_schema`] by listing the schema and filtering.
pub async fn search_schema_by_listing<S: Session + ?Sized>(s: &S, schema: &str, query: &str, limit: usize) -> Result<Vec<DbObject>> {
    let q = query.trim().to_lowercase();
    let mut hits: Vec<DbObject> = s
        .list_objects(schema)
        .await?
        .into_iter()
        .filter(|o| o.kind.is_relation() && (q.is_empty() || o.name.to_lowercase().contains(&q)))
        .collect();
    rank_matches(query, &mut hits, limit);
    Ok(hits)
}

/// [`Session::search_routines`] by listing one schema (the given one, else the default).
pub async fn search_routines_by_listing<S: Session + ?Sized>(s: &S, schema: Option<&str>, query: &str, limit: usize) -> Result<Vec<DbObject>> {
    let schema = match schema {
        Some(sc) => sc.to_string(),
        None => {
            let all = s.list_schemas().await?;
            match all.iter().find(|sc| sc.is_default).or(all.first()) {
                Some(sc) => sc.name.clone(),
                None => return Ok(vec![]),
            }
        }
    };
    let q = query.trim().to_lowercase();
    let mut hits: Vec<DbObject> = s
        .list_objects(&schema)
        .await?
        .into_iter()
        .filter(|o| o.kind.is_routine() && (q.is_empty() || o.name.to_lowercase().contains(&q)))
        .collect();
    rank_matches(query, &mut hits, limit);
    Ok(hits)
}

pub async fn search_by_listing<S: Session + ?Sized>(s: &S, query: &str, limit: usize) -> Result<Vec<DbObject>> {
    let mut hits = Vec::new();
    for sc in s.list_schemas().await? {
        let Ok(objs) = s.list_objects(&sc.name).await else { continue };
        hits.extend(objs.into_iter().filter(|o| {
            !matches!(o.kind, ObjectKind::Function | ObjectKind::Procedure | ObjectKind::Package | ObjectKind::Sequence)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assembles_flat_rows_per_schema() {
        let col = |n: &str| ColumnInfo { name: n.into(), data_type: "int".into(), nullable: true, is_primary_key: false, default: None, comment: None };
        let obj = |s: &str, n: &str| DbObject { schema: s.into(), name: n.into(), kind: ObjectKind::Table, comment: None, row_estimate: None };
        let fk = ForeignKey { columns: vec!["a_id".into()], ref_schema: "a".into(), ref_table: "t".into(), ref_columns: vec!["id".into()] };
        let m = assemble_metadata(
            &["a".into(), "b".into(), "empty".into()],
            vec![obj("a", "t"), obj("b", "u"), obj("zz", "ignored")],
            vec![("a".into(), "t".into(), col("id")), ("b".into(), "u".into(), col("x")), ("a".into(), "t".into(), col("name")), ("b".into(), "u".into(), col("a_id"))],
            vec![("b".into(), "u".into(), fk.clone()), ("b".into(), "missing".into(), fk.clone())],
        );
        assert_eq!(m.iter().map(|s| s.schema.as_str()).collect::<Vec<_>>(), vec!["a", "b", "empty"]);
        assert_eq!(m[0].columns[0].columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), vec!["id", "name"], "non-adjacent rows still grouped, in order");
        assert_eq!(m[1].columns[0].foreign_keys, vec![fk]);
        assert!(m[2].objects.is_empty() && m[2].columns.is_empty() && m[2].error.is_none());
    }
}
