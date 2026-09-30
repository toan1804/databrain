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
    pub name: String,
    /// Schema the session resolves unqualified names against.
    pub is_default: bool,
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

/// Column metadata for a whole schema (bulk introspection for knowledge).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TableColumns {
    pub table: String,
    pub columns: Vec<ColumnInfo>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub foreign_keys: Vec<ForeignKey>,
}
