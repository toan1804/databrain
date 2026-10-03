use std::collections::BTreeMap;

use databrain_auth::{AuthMethod, AuthMethodKind};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectorKind {
    Sqlite,
    Postgres,
    Mysql,
    Mssql,
    Oracle,
    Snowflake,
    Databricks,
    Bigquery,
    Duckdb,
}

impl ConnectorKind {
    pub const ALL: [ConnectorKind; 9] = [
        ConnectorKind::Sqlite,
        ConnectorKind::Postgres,
        ConnectorKind::Mysql,
        ConnectorKind::Mssql,
        ConnectorKind::Oracle,
        ConnectorKind::Snowflake,
        ConnectorKind::Databricks,
        ConnectorKind::Bigquery,
        ConnectorKind::Duckdb,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            ConnectorKind::Sqlite => "sqlite",
            ConnectorKind::Postgres => "postgres",
            ConnectorKind::Mysql => "mysql",
            ConnectorKind::Mssql => "mssql",
            ConnectorKind::Oracle => "oracle",
            ConnectorKind::Snowflake => "snowflake",
            ConnectorKind::Databricks => "databricks",
            ConnectorKind::Bigquery => "bigquery",
            ConnectorKind::Duckdb => "duckdb",
        }
    }

    /// Human name of the SQL dialect (used in AI prompts).
    pub fn dialect_name(self) -> &'static str {
        match self {
            ConnectorKind::Sqlite => "SQLite",
            ConnectorKind::Postgres => "PostgreSQL",
            ConnectorKind::Mysql => "MySQL/MariaDB",
            ConnectorKind::Mssql => "Microsoft SQL Server (T-SQL)",
            ConnectorKind::Oracle => "Oracle SQL / PL/SQL",
            ConnectorKind::Snowflake => "Snowflake SQL",
            ConnectorKind::Databricks => "Databricks SQL (Spark SQL)",
            ConnectorKind::Bigquery => "BigQuery GoogleSQL",
            ConnectorKind::Duckdb => "DuckDB SQL (read local files with read_csv/read_parquet/read_json/delta_scan/iceberg_scan)",
        }
    }

    /// Connectors reached over HTTPS APIs (no raw TCP, so no SSH tunnel).
    pub fn is_cloud(self) -> bool {
        matches!(
            self,
            ConnectorKind::Snowflake | ConnectorKind::Databricks | ConnectorKind::Bigquery
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "method", rename_all = "snake_case")]
pub enum SshAuth {
    /// Password in the keychain slot `ssh`.
    Password,
    /// Private key file; optional passphrase in the keychain slot `ssh`.
    Key { path: String },
    /// Keys from the running SSH agent (`SSH_AUTH_SOCK`).
    Agent,
}

/// SSH tunnel (bastion) settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SshConfig {
    pub host: String,
    #[serde(default = "default_ssh_port")]
    pub port: u16,
    pub user: String,
    pub auth: SshAuth,
    /// Expected server key fingerprint (`SHA256:...`). Filled on first
    /// connect (trust on first use) and verified afterwards.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_key_fingerprint: Option<String>,
}

fn default_ssh_port() -> u16 {
    22
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SslMode {
    Disable,
    #[default]
    Prefer,
    Require,
    /// Require TLS and verify the certificate chain and host name.
    VerifyFull,
}

/// Non-secret connection settings, persisted in the workspace database.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConnectionConfig {
    pub kind: ConnectorKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub database: Option<String>,
    /// Database file path (SQLite).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_path: Option<String>,
    pub auth: AuthMethod,
    #[serde(default)]
    pub ssl_mode: SslMode,
    /// Open sessions in read-only mode where the engine supports it.
    #[serde(default)]
    pub read_only: bool,
    /// Connector-specific options, e.g. Snowflake `account`, `warehouse`,
    /// `role`, `schema`; Databricks `http_path`, `catalog`, `schema`;
    /// BigQuery `project`, `location`, `dataset`; SQL Server `instance`,
    /// `trust_cert`, `tenant`; Oracle `service_name`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub options: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh: Option<SshConfig>,
}

impl ConnectionConfig {
    pub fn host_or_default(&self) -> &str {
        self.host
            .as_deref()
            .filter(|h| !h.is_empty())
            .unwrap_or("localhost")
    }

    /// Non-empty option value.
    pub fn opt(&self, key: &str) -> Option<&str> {
        self.options
            .get(key)
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
    }

    /// Minimal config for tests and examples.
    pub fn new(kind: ConnectorKind, auth: AuthMethod) -> Self {
        Self {
            kind,
            host: None,
            port: None,
            database: None,
            file_path: None,
            auth,
            ssl_mode: SslMode::default(),
            read_only: false,
            options: BTreeMap::new(),
            ssh: None,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Capabilities {
    pub transactions: bool,
    pub cancel: bool,
    pub schemas: bool,
    pub read_only_sessions: bool,
    /// Supports an SSH tunnel.
    pub ssh: bool,
}

/// Static description of a connector; drives the connection form in the UI.
#[derive(Debug, Clone, Serialize)]
pub struct ConnectorInfo {
    pub kind: ConnectorKind,
    pub display_name: &'static str,
    pub default_port: Option<u16>,
    pub uses_file: bool,
    /// Supported auth methods, recommended first.
    pub auth_methods: Vec<AuthMethodKind>,
    pub capabilities: Capabilities,
    /// Connection form fields (beyond auth) the UI should show.
    pub fields: Vec<FieldSpec>,
    /// Runtime requirement shown in the UI (e.g. Oracle Instant Client).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<&'static str>,
}

/// A connection form field. `key` is `host`, `port`, `database`, `file_path`
/// or an `options` key.
#[derive(Debug, Clone, Serialize)]
pub struct FieldSpec {
    pub key: &'static str,
    pub label: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub placeholder: Option<&'static str>,
    pub required: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub help: Option<&'static str>,
}

impl FieldSpec {
    pub const fn new(key: &'static str, label: &'static str) -> Self {
        Self {
            key,
            label,
            placeholder: None,
            required: false,
            help: None,
        }
    }
    pub const fn required(mut self) -> Self {
        self.required = true;
        self
    }
    pub const fn placeholder(mut self, p: &'static str) -> Self {
        self.placeholder = Some(p);
        self
    }
    pub const fn help(mut self, h: &'static str) -> Self {
        self.help = Some(h);
        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SchemaInfo {
    /// Fully qualified schema id passed back to `list_objects` (for
    /// three-level engines this is `catalog.schema`).
    pub name: String,
    /// Schema the session resolves unqualified names against.
    pub is_default: bool,
    /// Top level (Databricks catalog, Snowflake database, BigQuery project,
    /// DuckDB database) for engines with three-level names.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catalog: Option<String>,
}

impl SchemaInfo {
    pub fn new(name: impl Into<String>, is_default: bool) -> Self {
        Self { name: name.into(), is_default, catalog: None }
    }

    /// `catalog.schema` entry; `name` becomes the qualified id.
    pub fn in_catalog(catalog: impl Into<String>, schema: &str, is_default: bool) -> Self {
        let catalog = catalog.into();
        Self { name: format!("{catalog}.{schema}"), is_default, catalog: Some(catalog) }
    }
}

/// Matching for catalog search: case-insensitive substring of the object
/// name, or of `schema.name` when the query is qualified (`sales.ord`).
pub fn object_matches(query: &str, schema: &str, name: &str) -> bool {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return false;
    }
    let n = name.to_lowercase();
    if !q.contains('.') {
        return n.contains(&q);
    }
    format!("{}.{n}", schema.to_lowercase()).contains(&q)
}

/// The part of a search query that applies to the object name (after the
/// last dot), for engines that filter server-side.
pub fn search_name_term(query: &str) -> String {
    query.trim().rsplit('.').next().unwrap_or_default().to_string()
}

/// Lower-cased name term for server-side filtering, reduced to characters
/// that are safe inside any dialect's string literal (letters, digits, `_`,
/// `-`, `$`, space). Results are re-checked with [`object_matches`].
pub fn search_sql_term(query: &str) -> String {
    search_name_term(query)
        .to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | '$' | ' '))
        .collect()
}

/// Rank search hits: exact name, then prefix, then shorter names first.
pub fn rank_matches(query: &str, hits: &mut Vec<DbObject>, limit: usize) {
    let term = search_name_term(query).to_lowercase();
    hits.sort_by_cached_key(|o| {
        let n = o.name.to_lowercase();
        let tier = if n == term { 0 } else if n.starts_with(&term) { 1 } else { 2 };
        (tier, n.len(), o.schema.clone(), o.name.clone())
    });
    hits.dedup_by(|a, b| a.schema == b.schema && a.name == b.name);
    hits.truncate(limit);
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ObjectKind {
    Table,
    View,
    MaterializedView,
    ForeignTable,
    Function,
    Procedure,
    Sequence,
    Other,
}

impl ObjectKind {
    /// Tables and views (things with columns), as opposed to routines and sequences.
    pub fn is_relation(self) -> bool {
        matches!(self, ObjectKind::Table | ObjectKind::View | ObjectKind::MaterializedView | ObjectKind::ForeignTable)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DbObject {
    pub schema: String,
    pub name: String,
    pub kind: ObjectKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub row_estimate: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ColumnInfo {
    pub name: String,
    pub data_type: String,
    pub nullable: bool,
    pub is_primary_key: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ForeignKey {
    pub columns: Vec<String>,
    pub ref_schema: String,
    pub ref_table: String,
    pub ref_columns: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ObjectDetail {
    pub object: DbObject,
    pub columns: Vec<ColumnInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ddl: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub foreign_keys: Vec<ForeignKey>,
}

/// An index (or primary/unique key) on a table.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct IndexInfo {
    pub name: String,
    /// Key columns in order (expressions as written by the server).
    pub columns: Vec<String>,
    #[serde(default)]
    pub unique: bool,
    #[serde(default)]
    pub primary: bool,
    /// Access method when not the engine default, e.g. `hash`, `gin`, `clustered`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
}

/// How a table is physically organised: what makes filters and joins fast.
/// Used for query hints in the editor.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct TableLayout {
    #[serde(default)]
    pub indexes: Vec<IndexInfo>,
    /// Partition columns (or expressions), e.g. `order_date`.
    #[serde(default)]
    pub partition_by: Vec<String>,
    /// e.g. `range`, `list`, `hash`, `day`, `hive`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub partition_kind: Option<String>,
    /// Clustering / sort keys (Snowflake, BigQuery, Databricks liquid clustering and Z-order).
    #[serde(default)]
    pub cluster_by: Vec<String>,
    /// The server rejects queries without a partition filter (BigQuery `require_partition_filter`).
    #[serde(default)]
    pub requires_partition_filter: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub row_estimate: Option<i64>,
    /// Extra facts worth showing (e.g. "ZORDER BY (a, b)", "heap: no clustered index").
    #[serde(default)]
    pub notes: Vec<String>,
}

impl TableLayout {
    /// Layout with only the primary key, from column metadata.
    pub fn from_columns(columns: &[ColumnInfo]) -> Self {
        let pk: Vec<String> = columns.iter().filter(|c| c.is_primary_key).map(|c| c.name.clone()).collect();
        let mut l = TableLayout::default();
        if !pk.is_empty() {
            l.indexes.push(IndexInfo { name: "PRIMARY KEY".into(), columns: pk, unique: true, primary: true, method: None });
        }
        l
    }

    pub fn is_empty(&self) -> bool {
        self.indexes.is_empty() && self.partition_by.is_empty() && self.cluster_by.is_empty() && self.notes.is_empty()
    }
}

/// Split a key list such as `a, date_trunc('day', b), "c,d"` at top-level commas.
pub fn split_key_list(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let (mut depth, mut cur, mut quote) = (0i32, String::new(), None::<char>);
    for ch in s.chars() {
        match (quote, ch) {
            (Some(q), c) if c == q => {
                quote = None;
                cur.push(c);
            }
            (Some(_), c) => cur.push(c),
            (None, '\'' | '"' | '`') => {
                quote = Some(ch);
                cur.push(ch);
            }
            (None, '(' | '[') => {
                depth += 1;
                cur.push(ch);
            }
            (None, ')' | ']') => {
                depth -= 1;
                cur.push(ch);
            }
            (None, ',') if depth == 0 => {
                if !cur.trim().is_empty() {
                    out.push(cur.trim().to_string());
                }
                cur.clear();
            }
            (None, c) => cur.push(c),
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_string());
    }
    out
}

/// Strip one level of identifier quotes (`"a"`, `` `a` ``, `[a]`).
pub fn unquote_ident(s: &str) -> String {
    let t = s.trim();
    let b = t.as_bytes();
    if b.len() >= 2 && ((b[0] == b'"' && b[b.len() - 1] == b'"') || (b[0] == b'`' && b[b.len() - 1] == b'`') || (b[0] == b'[' && b[b.len() - 1] == b']')) {
        return t[1..t.len() - 1].to_string();
    }
    t.to_string()
}

/// One schema's metadata from [`crate::Session::bulk_metadata`].
#[derive(Debug, Clone, PartialEq)]
pub struct SchemaMetadata {
    pub schema: String,
    pub objects: Vec<DbObject>,
    pub columns: Vec<TableColumns>,
    pub error: Option<String>,
}

/// Column metadata for a whole schema (bulk introspection for knowledge).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TableColumns {
    pub table: String,
    pub columns: Vec<ColumnInfo>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub foreign_keys: Vec<ForeignKey>,
}

#[cfg(test)]
mod layout_tests {
    use super::*;

    #[test]
    fn splits_key_lists() {
        assert_eq!(split_key_list("a, date_trunc('day', b) , \"c,d\""), vec!["a", "date_trunc('day', b)", "\"c,d\""]);
        assert_eq!(split_key_list("  "), Vec::<String>::new());
        assert_eq!(unquote_ident("`x`"), "x");
        assert_eq!(unquote_ident("[x y]"), "x y");
        assert_eq!(crate::parse_list_text("[a, \"b c\"]"), vec!["a", "b c"]);
        assert_eq!(crate::parse_list_text("[]"), Vec::<String>::new());
        assert_eq!(crate::parse_call_list("RANGE (order_date)"), ("range".to_string(), vec!["order_date".to_string()]));
        assert_eq!(crate::parse_call_list("LINEAR(a, \"B\")").1, vec!["a", "B"]);
        let ix = crate::group_indexes([
            ("i".to_string(), "a".to_string(), false, false, None),
            ("pk".into(), "id".into(), false, true, None),
            ("i".into(), "b".into(), false, false, None),
        ]);
        assert_eq!(ix[0].name, "pk");
        assert!(ix[0].unique);
        assert_eq!(ix[1].columns, vec!["a", "b"]);
    }

    #[test]
    fn primary_key_from_columns() {
        let c = |n: &str, pk| ColumnInfo { name: n.into(), data_type: "int".into(), nullable: false, is_primary_key: pk, default: None, comment: None };
        let l = TableLayout::from_columns(&[c("a", true), c("b", false), c("c", true)]);
        assert_eq!(l.indexes[0].columns, vec!["a", "c"]);
        assert!(TableLayout::from_columns(&[c("b", false)]).is_empty());
    }
}

#[cfg(test)]
mod search_tests {
    use super::*;

    fn obj(schema: &str, name: &str) -> DbObject {
        DbObject { schema: schema.into(), name: name.into(), kind: ObjectKind::Table, comment: None, row_estimate: None }
    }

    #[test]
    fn matches_and_ranks() {
        assert!(object_matches("ORD", "s", "orders"));
        assert!(object_matches("main.sales.ord", "main.sales", "orders"));
        assert!(!object_matches("crm.ord", "main.sales", "orders"));
        assert!(!object_matches("  ", "s", "orders"));
        assert_eq!(search_sql_term("cat.sch.Ord'x"), "ordx");
        let mut v = vec![obj("a", "big_orders"), obj("a", "orders_2024"), obj("b", "orders"), obj("a", "orders"), obj("a", "orders")];
        rank_matches("orders", &mut v, 3);
        let got: Vec<_> = v.iter().map(|o| format!("{}.{}", o.schema, o.name)).collect();
        assert_eq!(got, vec!["a.orders", "b.orders", "a.orders_2024"]);
        let s = SchemaInfo::in_catalog("main", "sales", true);
        assert_eq!((s.name.as_str(), s.catalog.as_deref()), ("main.sales", Some("main")));
    }
}
