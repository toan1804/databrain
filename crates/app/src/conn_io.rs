//! Sharing and importing connections.
//!
//! - Export writes a `.databrain-connections.json` file: names, folders,
//!   environment tags, settings and AI rules, never passwords or keys
//!   (secrets live in the keychain; option values that carry one, such as a
//!   `mongodb://user:pass@…` string, are cleaned).
//! - Import reads such a file, DBeaver's `data-sources.json` (no passwords:
//!   DBeaver keeps them encrypted elsewhere) or `~/.pgpass` (with its
//!   passwords, saved to the keychain). A preview lists what would be added
//!   and marks connections that already exist; only the picked ones are saved.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use databrain_auth::AuthMethod;
use databrain_connector_core::{ConnectionConfig, ConnectorKind, SshAuth, SshConfig};
use databrain_query_engine::{EngineError, Result};
use databrain_workspace::{AiPolicy, ConnectionProfile, EnvTag, Folder, FolderKind};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::api::{self, AppState};

pub const FORMAT: &str = "databrain-connections";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectionsFile {
    pub format: String,
    pub version: u32,
    pub connections: Vec<SharedConnection>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SharedConnection {
    pub name: String,
    /// Folder path from the root (`["Prod", "EU"]`).
    #[serde(default)]
    pub folder: Vec<String>,
    #[serde(default)]
    pub env: EnvTag,
    #[serde(default)]
    pub color: Option<String>,
    pub config: ConnectionConfig,
    #[serde(default)]
    pub ai_policy: AiPolicy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportSource {
    /// A DataBrain export.
    File,
    Dbeaver,
    Pgpass,
}

#[derive(Debug, Clone, Serialize)]
pub struct ImportCandidate {
    pub index: usize,
    pub name: String,
    pub folder: Vec<String>,
    pub kind: ConnectorKind,
    /// `host:port/database` or the file.
    pub target: String,
    pub user: Option<String>,
    pub env: EnvTag,
    /// The name of an existing connection to the same place.
    pub existing: Option<String>,
    /// A password comes with it (pgpass).
    pub has_password: bool,
    /// Why it may need attention after import.
    pub note: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ImportPreview {
    pub path: String,
    pub candidates: Vec<ImportCandidate>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ImportReport {
    pub imported: usize,
    /// "name: reason" for those that could not be saved.
    pub errors: Vec<String>,
}

struct Parsed {
    conn: SharedConnection,
    secret: Option<String>,
    note: Option<String>,
}

fn bad(m: impl Into<String>) -> EngineError {
    EngineError::new("invalid", m)
}

/// Where the other tools keep their connections on this computer.
pub fn default_path(source: ImportSource) -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    match source {
        ImportSource::File => None,
        ImportSource::Pgpass => Some(std::env::var_os("PGPASSFILE").map(PathBuf::from).unwrap_or_else(|| {
            if cfg!(windows) {
                dirs::data_dir().unwrap_or(home.clone()).join("postgresql").join("pgpass.conf")
            } else {
                home.join(".pgpass")
            }
        })),
        ImportSource::Dbeaver => {
            let base = if cfg!(target_os = "macos") {
                home.join("Library").join("DBeaverData")
            } else if cfg!(windows) {
                dirs::data_dir()?.join("DBeaverData")
            } else {
                dirs::data_local_dir().unwrap_or(home.join(".local/share")).join("DBeaverData")
            };
            Some(base.join("workspace6").join("General").join(".dbeaver").join("data-sources.json"))
        }
    }
}

// ------------------------------------------------------------------ export

/// Option keys that hold secrets (never exported).
fn secret_key(k: &str) -> bool {
    let k = k.to_ascii_lowercase();
    ["password", "passwd", "secret", "token", "credential"].iter().any(|s| k.contains(s))
}

/// `scheme://user:pass@host` → `scheme://user@host`.
fn strip_url_password(v: &str) -> String {
    let Some(start) = v.find("://").map(|i| i + 3) else { return v.to_string() };
    let rest = &v[start..];
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let Some(at) = rest[..end].rfind('@') else { return v.to_string() };
    match rest[..at].find(':') {
        Some(colon) => format!("{}{}{}", &v[..start], &rest[..colon], &rest[at..]),
        None => v.to_string(),
    }
}

pub fn shareable(mut c: ConnectionConfig) -> ConnectionConfig {
    c.options.retain(|k, _| !secret_key(k));
    for v in c.options.values_mut() {
        *v = strip_url_password(v);
    }
    c
}

fn folder_paths(state: &AppState) -> Result<HashMap<String, Vec<String>>> {
    let folders = state.workspace.list_folders(FolderKind::Connections)?;
    let by_id: HashMap<&str, &Folder> = folders.iter().map(|f| (f.id.as_str(), f)).collect();
    let mut out = HashMap::new();
    for f in &folders {
        let mut path = vec![f.name.clone()];
        let mut cur = f.parent_id.as_deref();
        while let Some(p) = cur.and_then(|id| by_id.get(id)) {
            if path.len() > 32 {
                break;
            }
            path.insert(0, p.name.clone());
            cur = p.parent_id.as_deref();
        }
        out.insert(f.id.clone(), path);
    }
    Ok(out)
}

/// Write the connections (`ids`; all when `None`) to `path`, without secrets.
pub fn export_connections(state: &AppState, ids: Option<Vec<String>>, path: &str) -> Result<usize> {
    let paths = folder_paths(state)?;
    let connections: Vec<SharedConnection> = state
        .workspace
        .list_connections()?
        .into_iter()
        .filter(|c| ids.as_ref().is_none_or(|ids| ids.contains(&c.id)))
        // The built-in Results (DuckDB) connection is not the user's.
        .filter(|c| !c.config.options.contains_key(databrain_query_engine::RESULTS_MARKER))
        .map(|c| SharedConnection {
            folder: c.folder_id.as_ref().and_then(|f| paths.get(f).cloned()).unwrap_or_default(),
            name: c.name,
            env: c.env,
            color: c.color,
            config: shareable(c.config),
            ai_policy: c.ai_policy,
        })
        .collect();
    let n = connections.len();
    let file = ConnectionsFile { format: FORMAT.into(), version: 1, connections };
    let text = serde_json::to_string_pretty(&file).map_err(|e| bad(e.to_string()))?;
    std::fs::write(path, text).map_err(|e| bad(format!("{path}: {e}")))?;
    Ok(n)
}

// ------------------------------------------------------------------ import

fn parse(source: ImportSource, path: &Path) -> Result<Vec<Parsed>> {
    let text = std::fs::read_to_string(path).map_err(|e| bad(format!("{}: {e}", path.display())))?;
    match source {
        ImportSource::File => {
            let f: ConnectionsFile = serde_json::from_str(&text).map_err(|e| bad(format!("Not a DataBrain connections file: {e}")))?;
            if f.format != FORMAT {
                return Err(bad("Not a DataBrain connections file"));
            }
            Ok(f.connections.into_iter().map(|c| Parsed { conn: SharedConnection { config: shareable(c.config), ..c }, secret: None, note: None }).collect())
        }
        ImportSource::Pgpass => Ok(parse_pgpass(&text)),
        ImportSource::Dbeaver => parse_dbeaver(&text),
    }
}

/// `host:port:database:user:password` lines (`\:` and `\\` escaped).
fn parse_pgpass(text: &str) -> Vec<Parsed> {
    let mut out = Vec::new();
    for line in text.lines() {
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        let mut fields = vec![String::new()];
        let mut esc = false;
        for ch in line.chars() {
            match (esc, ch) {
                (true, c) => {
                    fields.last_mut().unwrap().push(c);
                    esc = false;
                }
                (false, '\\') => esc = true,
                (false, ':') if fields.len() < 5 => fields.push(String::new()),
                (false, c) => fields.last_mut().unwrap().push(c),
            }
        }
        let [host, port, db, user, password] = <[String; 5]>::try_from(fields).unwrap_or_default();
        // A wildcard host or user can't be connected to.
        if host.is_empty() || host == "*" || user.is_empty() || user == "*" {
            continue;
        }
        let mut c = ConnectionConfig::new(ConnectorKind::Postgres, AuthMethod::Password { user: user.clone() });
        c.host = Some(host.clone());
        c.port = port.parse().ok();
        c.database = (db != "*" && !db.is_empty()).then(|| db.clone());
        let name = format!("{user}@{host}{}", c.database.as_ref().map(|d| format!("/{d}")).unwrap_or_default());
        out.push(Parsed {
            conn: SharedConnection { name, folder: vec![], env: EnvTag::None, color: None, config: c, ai_policy: Default::default() },
            secret: (!password.is_empty()).then_some(password),
            note: (db == "*").then(|| "Any database: the server's default is used".into()),
        });
    }
    out
}

fn dbeaver_kind(provider: &str, driver: &str) -> Option<ConnectorKind> {
    let p = provider.to_ascii_lowercase();
    let d = driver.to_ascii_lowercase();
    let has = |s: &str| p.contains(s) || d.contains(s);
    Some(if has("postgres") || has("redshift") || has("greenplum") || has("cockroach") {
        ConnectorKind::Postgres
    } else if has("mysql") || has("mariadb") {
        ConnectorKind::Mysql
    } else if has("sqlserver") || has("mssql") || has("jtds") || has("azure") {
        ConnectorKind::Mssql
    } else if has("oracle") {
        ConnectorKind::Oracle
    } else if has("sqlite") {
        ConnectorKind::Sqlite
    } else if has("duckdb") {
        ConnectorKind::Duckdb
    } else if has("snowflake") {
        ConnectorKind::Snowflake
    } else if has("databricks") {
        ConnectorKind::Databricks
    } else if has("bigquery") {
        ConnectorKind::Bigquery
    } else if has("mongo") {
        ConnectorKind::Mongodb
    } else if has("neo4j") {
        ConnectorKind::Neo4j
    } else {
        return None;
    })
}

fn parse_dbeaver(text: &str) -> Result<Vec<Parsed>> {
    let v: Value = serde_json::from_str(text).map_err(|e| bad(format!("Not a DBeaver data-sources.json: {e}")))?;
    let Some(conns) = v.get("connections").and_then(Value::as_object) else {
        return Err(bad("No connections in this DBeaver file"));
    };
    let s = |o: &Value, k: &str| o.get(k).and_then(|x| match x {
        Value::String(s) if !s.trim().is_empty() => Some(s.trim().to_string()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    });
    let mut out = Vec::new();
    for c in conns.values() {
        let conf = c.get("configuration").cloned().unwrap_or(Value::Null);
        let name = s(c, "name").unwrap_or_else(|| "DBeaver connection".into());
        let Some(kind) = dbeaver_kind(&s(c, "provider").unwrap_or_default(), &s(c, "driver").unwrap_or_default()) else {
            continue;
        };
        let user = s(&conf, "user");
        let auth = match (kind, &user) {
            (ConnectorKind::Sqlite | ConnectorKind::Duckdb, _) => AuthMethod::None,
            (_, Some(u)) => AuthMethod::Password { user: u.clone() },
            (_, None) => AuthMethod::Password { user: String::new() },
        };
        let mut cfg = ConnectionConfig::new(kind, auth);
        let url = s(&conf, "url").unwrap_or_default();
        match kind {
            ConnectorKind::Sqlite | ConnectorKind::Duckdb => {
                let file = s(&conf, "database").or_else(|| url.split_once(':').and_then(|(_, r)| r.split_once(':')).map(|(_, p)| p.to_string()));
                cfg.file_path = file;
            }
            _ => {
                cfg.host = s(&conf, "host");
                cfg.port = s(&conf, "port").and_then(|p| p.parse().ok());
                cfg.database = s(&conf, "database");
            }
        }
        if kind == ConnectorKind::Snowflake {
            if let Some(acct) = cfg.host.as_deref().and_then(|h| h.strip_suffix(".snowflakecomputing.com")) {
                cfg.options.insert("account".into(), acct.to_string());
            }
            if let Some(w) = conf.get("provider-properties").and_then(|p| s(p, "@dbeaver-warehouse@")) {
                cfg.options.insert("warehouse".into(), w);
            }
        }
        if kind == ConnectorKind::Oracle {
            let service = conf.get("provider-properties").and_then(|p| s(p, "@dbeaver-sid-service@")).is_some_and(|t| t.eq_ignore_ascii_case("SERVICE"));
            if service {
                if let Some(db) = cfg.database.take() {
                    cfg.options.insert("service_name".into(), db);
                }
            }
        }
        if let Some(ssh) = conf.get("handlers").and_then(|h| h.get("ssh_tunnel")).filter(|h| h.get("enabled").and_then(Value::as_bool) == Some(true)) {
            let p = ssh.get("properties").cloned().unwrap_or(Value::Null);
            if let Some(host) = s(&p, "host") {
                let key = s(&p, "keyPath");
                let auth = match s(&p, "authType").as_deref() {
                    Some("PUBLIC_KEY") if key.is_some() => SshAuth::Key { path: key.unwrap() },
                    Some("AGENT") => SshAuth::Agent,
                    _ => SshAuth::Password,
                };
                cfg.ssh = Some(SshConfig { host, port: s(&p, "port").and_then(|x| x.parse().ok()).unwrap_or(22), user: s(&p, "user").unwrap_or_default(), auth, host_key_fingerprint: None });
            }
        }
        let env = match s(&conf, "type").as_deref() {
            Some("prod") => EnvTag::Prod,
            Some("test") => EnvTag::Staging,
            Some("dev") => EnvTag::Dev,
            _ => EnvTag::None,
        };
        let folder = s(c, "folder").map(|f| f.split('/').map(str::to_string).filter(|x| !x.is_empty()).collect()).unwrap_or_default();
        let needs_secret = !matches!(kind, ConnectorKind::Sqlite | ConnectorKind::Duckdb);
        let note = match (needs_secret, user.is_none()) {
            (true, true) => Some("Enter the user and password after import".to_string()),
            (true, false) => Some("Enter the password after import".to_string()),
            _ => None,
        };
        out.push(Parsed { conn: SharedConnection { name, folder, env, color: None, config: cfg, ai_policy: Default::default() }, secret: None, note });
    }
    Ok(out)
}

fn user_of(c: &ConnectionConfig) -> Option<String> {
    match &c.auth {
        AuthMethod::Password { user } | AuthMethod::KeyPair { user } | AuthMethod::ExternalBrowser { user } => Some(user.clone()).filter(|u| !u.is_empty()),
        AuthMethod::ApiToken { user } => user.clone(),
        _ => None,
    }
}

/// Two configs reach the same place as the same user.
fn same_place(a: &ConnectionConfig, b: &ConnectionConfig) -> bool {
    let eq = |x: &Option<String>, y: &Option<String>| x.as_deref().map(str::to_ascii_lowercase) == y.as_deref().map(str::to_ascii_lowercase);
    a.kind == b.kind
        && if a.file_path.is_some() || b.file_path.is_some() {
            a.file_path == b.file_path
        } else {
            eq(&a.host, &b.host) && (a.port.is_none() || b.port.is_none() || a.port == b.port) && eq(&a.database, &b.database) && user_of(a) == user_of(b) && a.options.get("account") == b.options.get("account")
        }
}

fn target_of(c: &ConnectionConfig) -> String {
    if let Some(f) = &c.file_path {
        return f.clone();
    }
    let mut t = c.host.clone().or_else(|| c.options.get("account").cloned()).unwrap_or_default();
    if let Some(p) = c.port {
        t.push_str(&format!(":{p}"));
    }
    if let Some(d) = &c.database {
        t.push_str(&format!("/{d}"));
    }
    t
}

fn resolve(source: ImportSource, path: Option<String>) -> Result<PathBuf> {
    match path.filter(|p| !p.trim().is_empty()) {
        Some(p) => Ok(PathBuf::from(p)),
        None => default_path(source).ok_or_else(|| bad("Choose the file to import")),
    }
}

pub fn import_preview(state: &AppState, source: ImportSource, path: Option<String>) -> Result<ImportPreview> {
    let path = resolve(source, path)?;
    if !path.exists() {
        return Err(EngineError::new("not_found", format!("{} not found", path.display())));
    }
    let parsed = parse(source, &path)?;
    let existing = state.workspace.list_connections()?;
    let candidates = parsed
        .iter()
        .enumerate()
        .map(|(index, p)| ImportCandidate {
            index,
            name: p.conn.name.clone(),
            folder: p.conn.folder.clone(),
            kind: p.conn.config.kind,
            target: target_of(&p.conn.config),
            user: user_of(&p.conn.config),
            env: p.conn.env,
            existing: existing.iter().find(|e| same_place(&e.config, &p.conn.config)).map(|e| e.name.clone()),
            has_password: p.secret.is_some(),
            note: p.note.clone(),
        })
        .collect();
    Ok(ImportPreview { path: path.to_string_lossy().into_owned(), candidates })
}

/// Save the picked candidates (indexes of the preview), creating their folders.
pub fn import_apply(state: &AppState, source: ImportSource, path: Option<String>, picks: &[usize]) -> Result<ImportReport> {
    let path = resolve(source, path)?;
    let parsed = parse(source, &path)?;
    let mut folders: HashMap<Vec<String>, String> = folder_paths(state)?.into_iter().map(|(id, p)| (p, id)).collect();
    let mut report = ImportReport::default();
    for &i in picks {
        let Some(p) = parsed.get(i) else { continue };
        let mut folder_id = None;
        for depth in 1..=p.conn.folder.len() {
            let key = p.conn.folder[..depth].to_vec();
            let id = match folders.get(&key) {
                Some(id) => id.clone(),
                None => {
                    let f = state.workspace.save_folder(Folder { id: String::new(), parent_id: folder_id.clone(), name: key[depth - 1].clone(), kind: FolderKind::Connections })?;
                    folders.insert(key, f.id.clone());
                    f.id
                }
            };
            folder_id = Some(id);
        }
        let profile = ConnectionProfile {
            id: String::new(),
            name: p.conn.name.clone(),
            config: p.conn.config.clone(),
            color: p.conn.color.clone(),
            env: p.conn.env,
            folder_id,
            has_secret: false,
            ai_policy: p.conn.ai_policy.clone(),
            created_at: 0,
            updated_at: 0,
        };
        match api::save_connection(state, api::SaveConnectionArgs { profile, secret: p.secret.clone(), clear_secret: false, extra_secrets: Default::default() }) {
            Ok(_) => report.imported += 1,
            Err(e) => report.errors.push(format!("{}: {}", p.conn.name, e.message)),
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleans_secrets_from_shared_settings() {
        assert_eq!(strip_url_password("mongodb+srv://ann:s3cr%40t@c0.x.net/db?x=1"), "mongodb+srv://ann@c0.x.net/db?x=1");
        assert_eq!(strip_url_password("neo4j://host:7687"), "neo4j://host:7687");
        let mut c = ConnectionConfig::new(ConnectorKind::Mongodb, AuthMethod::None);
        c.options.insert("uri".into(), "mongodb://u:p@h/".into());
        c.options.insert("client_secret".into(), "x".into());
        c.options.insert("warehouse".into(), "wh".into());
        let s = shareable(c);
        assert_eq!(s.options.get("uri").map(String::as_str), Some("mongodb://u@h/"));
        assert!(!s.options.contains_key("client_secret"));
        assert!(s.options.contains_key("warehouse"));
    }

    #[test]
    fn reads_pgpass() {
        let p = parse_pgpass("# comment\nlocalhost:5432:shop:ann:pa\\:ss\n*:*:*:bob:x\ndb.example.com:*:*:carl:\n");
        assert_eq!(p.len(), 2, "wildcard host skipped");
        assert_eq!((p[0].conn.name.as_str(), p[0].conn.config.port, p[0].secret.as_deref()), ("ann@localhost/shop", Some(5432), Some("pa:ss")));
        assert_eq!((p[1].conn.config.database.clone(), p[1].secret.clone()), (None, None));
    }

    #[test]
    fn reads_dbeaver() {
        let text = r#"{"folders": {"Prod": {}}, "connections": {
            "postgres-1": {"provider": "postgresql", "driver": "postgres-jdbc", "name": "Orders DB", "folder": "Prod",
                "configuration": {"host": "pg.internal", "port": "5433", "database": "orders", "user": "app", "type": "prod",
                  "handlers": {"ssh_tunnel": {"enabled": true, "properties": {"host": "bastion", "port": "2222", "user": "ec2", "authType": "PUBLIC_KEY", "keyPath": "/k/id_rsa"}}}}},
            "sqlite-1": {"provider": "generic", "driver": "sqlite_jdbc", "name": "Local", "configuration": {"url": "jdbc:sqlite:/tmp/a.db"}},
            "x-1": {"provider": "cassandra", "driver": "x", "name": "Unsupported", "configuration": {}}}}"#;
        let p = parse_dbeaver(text).unwrap();
        assert_eq!(p.len(), 2);
        let pg = p.iter().find(|x| x.conn.name == "Orders DB").unwrap();
        assert_eq!((pg.conn.config.kind, pg.conn.config.port, pg.conn.env, pg.conn.folder.clone()), (ConnectorKind::Postgres, Some(5433), EnvTag::Prod, vec!["Prod".to_string()]));
        let ssh = pg.conn.config.ssh.as_ref().unwrap();
        assert_eq!((ssh.host.as_str(), ssh.port, ssh.auth.clone()), ("bastion", 2222, SshAuth::Key { path: "/k/id_rsa".into() }));
        let lite = p.iter().find(|x| x.conn.name == "Local").unwrap();
        assert_eq!(lite.conn.config.file_path.as_deref(), Some("/tmp/a.db"));
    }
}
