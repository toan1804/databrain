//! Sharing and importing connections.
//!
//! - Export writes a `.databrain-connections.json` file: names, folders,
//!   environment tags, settings and AI rules, never passwords or keys
//!   (secrets live in the keychain; option values that carry one, such as a
//!   `mongodb://user:pass@…` string, are cleaned).
//! - Import reads such a file, DBeaver's `data-sources.json` (with the users
//!   and passwords of the `credentials-config.json` next to it, when DBeaver
//!   has no master password), DataGrip's `dataSources.xml` (users from
//!   `dataSources.local.xml`; passwords stay in DataGrip's keychain) or
//!   `~/.pgpass` (with its passwords). Passwords go to the keychain. A preview
//!   lists what would be added and marks connections that already exist; only
//!   the picked ones are saved. Settings an import lacks (a user, a project)
//!   don't stop it: the preview says what to fill in after.

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
    Datagrip,
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
        // The newest DataGrip's global data sources.
        ImportSource::Datagrip => {
            let base = if cfg!(target_os = "macos") { home.join("Library").join("Application Support").join("JetBrains") } else { dirs::config_dir()?.join("JetBrains") };
            let mut dirs: Vec<PathBuf> = std::fs::read_dir(&base)
                .ok()?
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("DataGrip")))
                .collect();
            dirs.sort();
            dirs.into_iter().rev().map(|d| d.join("options").join("dataSources.xml")).find(|p| p.exists())
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
        ImportSource::Dbeaver => parse_dbeaver(&text, &dbeaver_credentials(&path.with_file_name("credentials-config.json"))),
        ImportSource::Datagrip => {
            let local = std::fs::read_to_string(path.with_file_name("dataSources.local.xml")).unwrap_or_default();
            parse_datagrip(&text, &local)
        }
    }
}

/// DBeaver's default key for `credentials-config.json` (used when no
/// master password is set; documented by DBeaver).
const DBEAVER_KEY: [u8; 16] = [0xba, 0xbb, 0x4a, 0x9f, 0x77, 0x4a, 0xb8, 0x53, 0xc9, 0x6c, 0x2d, 0x65, 0x3d, 0xfe, 0x54, 0x4a];

/// Connection id → (user, password) from DBeaver's `credentials-config.json`
/// (AES-128-CBC, the IV first). Empty when it is missing or can't be read.
fn dbeaver_credentials(path: &Path) -> HashMap<String, (Option<String>, Option<String>)> {
    use aes::cipher::{BlockDecryptMut, KeyIvInit, block_padding::Pkcs7};
    let mut out = HashMap::new();
    let Ok(bytes) = std::fs::read(path) else { return out };
    if bytes.len() < 32 {
        return out;
    }
    let (iv, data) = bytes.split_at(16);
    let Ok(plain) = cbc::Decryptor::<aes::Aes128>::new(&DBEAVER_KEY.into(), iv.into()).decrypt_padded_vec_mut::<Pkcs7>(data) else { return out };
    let Ok(Value::Object(m)) = serde_json::from_slice::<Value>(&plain) else { return out };
    for (id, v) in m {
        let c = v.get("#connection");
        let get = |k: &str| c.and_then(|c| c.get(k)).and_then(Value::as_str).map(str::to_string).filter(|s| !s.is_empty());
        out.insert(id, (get("user"), get("password")));
    }
    out
}

/// Settings from a JDBC (or native) URL: `jdbc:postgresql://h:5432/db`,
/// `jdbc:sqlserver://h:1433;databaseName=db`, `jdbc:oracle:thin:@//h:1521/svc`,
/// `jdbc:snowflake://acct.snowflakecomputing.com/?db=..&warehouse=..`,
/// `jdbc:databricks://h:443/default;httpPath=..`, `jdbc:bigquery://…;ProjectId=..`,
/// `jdbc:sqlite:/path`, `mongodb://…`. The user named in it too, if any.
pub fn config_from_url(url: &str) -> Option<(ConnectionConfig, Option<String>)> {
    use ConnectorKind as K;
    let u = url.trim();
    let rest = u.strip_prefix("jdbc:").unwrap_or(u);
    let (scheme, after) = rest.split_once(':')?;
    let kind = match scheme.to_ascii_lowercase().as_str() {
        "postgresql" | "postgres" | "redshift" | "cockroachdb" | "pgsql" => K::Postgres,
        "mysql" | "mariadb" => K::Mysql,
        "sqlserver" | "jtds" => K::Mssql,
        "oracle" => K::Oracle,
        "sqlite" => K::Sqlite,
        "duckdb" => K::Duckdb,
        "snowflake" => K::Snowflake,
        "databricks" | "spark" => K::Databricks,
        "bigquery" => K::Bigquery,
        "mongodb" | "mongodb+srv" => K::Mongodb,
        "neo4j" | "bolt" | "neo4j+s" => K::Neo4j,
        _ => return None,
    };
    let mut c = ConnectionConfig::new(kind, AuthMethod::None);
    let mut user = None;
    // `host[:port]` (the first of several hosts; `[v6]:port`).
    let host_port = |s: &str| -> (Option<String>, Option<u16>) {
        let s = s.split(',').next().unwrap_or("").rsplit('@').next().unwrap_or("");
        let (h, p) = match s.strip_prefix('[').and_then(|x| x.split_once(']')) {
            Some((h, r)) => (h.to_string(), r.strip_prefix(':').and_then(|p| p.parse().ok())),
            None => match s.rsplit_once(':') {
                Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) && !p.is_empty() => (h.to_string(), p.parse().ok()),
                _ => (s.to_string(), None),
            },
        };
        ((!h.is_empty()).then_some(h), p)
    };
    // `k=v` pairs split by `sep`, keys lower-cased.
    let props = |s: &str, sep: char| -> HashMap<String, String> {
        s.split(sep).filter_map(|kv| kv.split_once('=')).map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string())).filter(|(_, v)| !v.is_empty()).collect()
    };
    match kind {
        K::Sqlite | K::Duckdb => {
            let p = after.trim_start_matches("file:").split('?').next().unwrap_or("");
            if !p.is_empty() && p != ":memory:" && p != "memory" {
                c.file_path = Some(p.to_string());
            }
        }
        K::Mongodb => {
            c.options.insert("uri".into(), strip_url_password(rest));
        }
        K::Mssql => {
            let a = after.strip_prefix("sqlserver:").unwrap_or(after).trim_start_matches("//");
            let mut segs = a.split(';');
            let head = segs.next().unwrap_or("");
            let p = props(&segs.collect::<Vec<_>>().join(";"), ';');
            let (hp, db) = head.split_once('/').map(|(h, d)| (h, Some(d.to_string()))).unwrap_or((head, None));
            // host\\instance:port
            (c.host, c.port) = host_port(hp);
            let inst = c.host.clone().and_then(|h| h.split_once('\\').map(|(h, i)| (h.to_string(), i.to_string()))).map(|(h, i)| {
                c.host = Some(h);
                i
            });
            c.database = p.get("databasename").or(p.get("database")).cloned().or(db).filter(|d| !d.is_empty());
            if let Some(i) = inst.or(p.get("instancename").cloned()) {
                c.options.insert("instance".into(), i);
            }
            user = p.get("user").cloned();
        }
        K::Oracle => {
            let a = after.split_once('@').map(|(_, r)| r).unwrap_or(after);
            if a.starts_with('(') {
                c.options.insert("connect_string".into(), a.to_string());
            } else {
                let a = a.trim_start_matches("//");
                match a.split_once('/') {
                    Some((hp, svc)) => {
                        (c.host, c.port) = host_port(hp);
                        c.options.insert("service_name".into(), svc.split('?').next().unwrap_or(svc).to_string());
                    }
                    None => {
                        // host:port:SID
                        let mut it = a.splitn(3, ':');
                        c.host = it.next().filter(|h| !h.is_empty()).map(str::to_string);
                        c.port = it.next().and_then(|p| p.parse().ok());
                        c.database = it.next().map(str::to_string).filter(|d| !d.is_empty());
                    }
                }
            }
        }
        K::Snowflake => {
            let a = after.trim_start_matches("//");
            let (hp, q) = a.split_once('?').unwrap_or((a, ""));
            let (h, _) = host_port(hp.trim_end_matches('/'));
            if let Some(acct) = h.as_deref().map(|h| h.strip_suffix(".snowflakecomputing.com").unwrap_or(h).to_string()) {
                c.options.insert("account".into(), acct);
            }
            let p = props(q, '&');
            c.database = p.get("db").or(p.get("database")).cloned();
            for (k, o) in [("warehouse", "warehouse"), ("role", "role"), ("schema", "schema")] {
                if let Some(v) = p.get(k) {
                    c.options.insert(o.into(), v.clone());
                }
            }
            user = p.get("user").cloned();
        }
        K::Databricks => {
            let a = after.strip_prefix("hive2:").unwrap_or(after).trim_start_matches("//");
            let mut segs = a.split(';');
            let head = segs.next().unwrap_or("");
            let p = props(&segs.collect::<Vec<_>>().join(";"), ';');
            let hp = head.split('/').next().unwrap_or(head);
            (c.host, c.port) = host_port(hp);
            if let Some(v) = p.get("httppath") {
                c.options.insert("http_path".into(), v.clone());
            }
            if let Some(v) = p.get("conncatalog") {
                c.options.insert("catalog".into(), v.clone());
            }
            if let Some(v) = p.get("connschema") {
                c.options.insert("schema".into(), v.clone());
            }
        }
        K::Bigquery => {
            let p = props(after, ';');
            if let Some(v) = p.get("projectid") {
                c.options.insert("project".into(), v.clone());
            }
            if let Some(v) = p.get("defaultdataset") {
                c.options.insert("dataset".into(), v.clone());
            }
        }
        _ => {
            // //host:port/db?k=v (Postgres, MySQL, Neo4j `bolt://`).
            let a = after.split_once("//").map(|(_, r)| r).unwrap_or(after);
            let (a, q) = a.split_once('?').unwrap_or((a, ""));
            let (hp, db) = a.split_once('/').map(|(h, d)| (h, Some(d.to_string()))).unwrap_or((a, None));
            if let Some((cred, _)) = hp.rsplit_once('@') {
                user = cred.split(':').next().map(str::to_string).filter(|u| !u.is_empty());
            }
            (c.host, c.port) = host_port(hp);
            c.database = db.filter(|d| !d.is_empty());
            let p = props(q, '&');
            user = user.or(p.get("user").cloned());
        }
    }
    Some((c, user))
}

/// One `<data-source>` of DataGrip's XML: attributes and child texts.
#[derive(Default, Debug)]
struct XmlSource {
    attrs: HashMap<String, String>,
    fields: HashMap<String, String>,
}

fn attrs_of(e: &quick_xml::events::BytesStart) -> HashMap<String, String> {
    e.attributes().flatten().filter_map(|a| Some((String::from_utf8_lossy(a.key.as_ref()).into_owned(), a.unescape_value().ok()?.into_owned()))).collect()
}

/// `<data-source …>` elements of a JetBrains data sources file, with the
/// text of their (nested) children by element name.
fn xml_sources(text: &str) -> std::result::Result<Vec<XmlSource>, String> {
    use quick_xml::events::Event;
    let mut r = quick_xml::Reader::from_str(text);
    r.config_mut().trim_text(true);
    let mut out = Vec::new();
    let mut cur: Option<XmlSource> = None;
    let mut path: Vec<String> = Vec::new();
    let text_of = |cur: &mut Option<XmlSource>, path: &[String], t: &str| {
        if let (Some(src), Some(name)) = (cur.as_mut(), path.last()) {
            let key = if path.len() >= 2 && path[path.len() - 2] != "data-source" { format!("{}/{name}", path[path.len() - 2]) } else { name.clone() };
            src.fields.entry(key).or_default().push_str(t);
        }
    };
    loop {
        match r.read_event().map_err(|e| e.to_string())? {
            Event::Start(e) if e.name().as_ref() == b"data-source" => {
                cur = Some(XmlSource { attrs: attrs_of(&e), ..Default::default() });
                path.clear();
            }
            // `<data-source … />`: no children.
            Event::Empty(e) if e.name().as_ref() == b"data-source" => out.push(XmlSource { attrs: attrs_of(&e), ..Default::default() }),
            Event::Start(e) if cur.is_some() => path.push(String::from_utf8_lossy(e.name().as_ref()).into_owned()),
            Event::End(e) if e.name().as_ref() == b"data-source" => {
                if let Some(src) = cur.take() {
                    out.push(src);
                }
            }
            Event::End(_) if cur.is_some() => {
                path.pop();
            }
            Event::Text(t) => {
                if let Ok(v) = t.decode() {
                    text_of(&mut cur, &path, &v);
                }
            }
            Event::GeneralRef(g) => {
                let v = match g.resolve_char_ref() {
                    Ok(Some(ch)) => ch.to_string(),
                    _ => match g.decode().as_deref() {
                        Ok("amp") => "&".into(),
                        Ok("lt") => "<".into(),
                        Ok("gt") => ">".into(),
                        Ok("quot") => "\"".into(),
                        Ok("apos") => "'".into(),
                        _ => String::new(),
                    },
                };
                text_of(&mut cur, &path, &v);
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(out)
}

/// DataGrip's `dataSources.xml` (project `.idea/` or the global `options/`)
/// and `dataSources.local.xml` (user names, per data source uuid).
fn parse_datagrip(text: &str, local: &str) -> Result<Vec<Parsed>> {
    let sources = xml_sources(text).map_err(|e| bad(format!("Not a DataGrip dataSources.xml: {e}")))?;
    if sources.is_empty() && !text.contains("DataSourceManager") {
        return Err(bad("Not a DataGrip dataSources.xml (no data sources)"));
    }
    let users: HashMap<String, String> = xml_sources(local)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|s| Some((s.attrs.get("uuid")?.clone(), s.fields.get("user-name")?.trim().to_string())))
        .filter(|(_, u)| !u.is_empty())
        .collect();
    let mut out = Vec::new();
    for src in sources {
        let name = src.attrs.get("name").cloned().unwrap_or_else(|| "DataGrip data source".into());
        let url = src.fields.get("jdbc-url").map(|u| u.trim().to_string()).unwrap_or_default();
        let driver = src.fields.get("driver-ref").cloned().unwrap_or_default();
        let Some((mut cfg, url_user)) = config_from_url(&url).or_else(|| dbeaver_kind(&driver, "").map(|k| (ConnectionConfig::new(k, AuthMethod::None), None))) else {
            continue;
        };
        let kind = cfg.kind;
        let user = src.attrs.get("uuid").and_then(|u| users.get(u)).cloned().or(url_user);
        let (auth, need) = auth_for(kind, user.clone());
        cfg.auth = auth;
        cfg.read_only = src.attrs.get("read-only").is_some_and(|v| v == "true");
        let ssh = src.fields.get("ssh-properties/enabled").is_some_and(|v| v.trim() == "true");
        let folder = src.attrs.get("group").map(|g| g.split('/').map(str::to_string).filter(|x| !x.is_empty()).collect()).unwrap_or_default();
        let mut notes: Vec<String> = need.into_iter().collect();
        if ssh {
            notes.push("Its SSH tunnel is not imported: set it in the connection".into());
        }
        out.push(Parsed { conn: SharedConnection { name, folder, env: EnvTag::None, color: None, config: cfg, ai_policy: Default::default() }, secret: None, note: (!notes.is_empty()).then(|| notes.join("; ")) });
    }
    Ok(out)
}

/// The auth for an imported connection with `user` (when known), and what
/// to enter after import.
fn auth_for(kind: ConnectorKind, user: Option<String>) -> (AuthMethod, Option<String>) {
    match kind {
        ConnectorKind::Sqlite | ConnectorKind::Duckdb => (AuthMethod::None, None),
        ConnectorKind::Bigquery | ConnectorKind::Databricks => (AuthMethod::None, Some("Choose how to sign in after import".into())),
        ConnectorKind::Mongodb if user.is_none() => (AuthMethod::None, None),
        _ => match user {
            Some(u) => (AuthMethod::Password { user: u }, Some("Enter the password after import".into())),
            None => (AuthMethod::Password { user: String::new() }, Some("Enter the user and password after import".into())),
        },
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

fn parse_dbeaver(text: &str, creds: &HashMap<String, (Option<String>, Option<String>)>) -> Result<Vec<Parsed>> {
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
    for (id, c) in conns {
        let conf = c.get("configuration").cloned().unwrap_or(Value::Null);
        let name = s(c, "name").unwrap_or_else(|| "DBeaver connection".into());
        let Some(kind) = dbeaver_kind(&s(c, "provider").unwrap_or_default(), &s(c, "driver").unwrap_or_default()) else {
            continue;
        };
        let (cred_user, password) = creds.get(id).cloned().unwrap_or_default();
        let from_url = s(&conf, "url").and_then(|u| config_from_url(&u)).filter(|(c, _)| c.kind == kind);
        let user = s(&conf, "user").or(cred_user).or_else(|| from_url.as_ref().and_then(|(_, u)| u.clone()));
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
                // URL-mode connections: what the URL says.
                if let Some((u, _)) = &from_url {
                    cfg.host = cfg.host.clone().or(u.host.clone());
                    cfg.port = cfg.port.or(u.port);
                    cfg.database = cfg.database.clone().or(u.database.clone());
                    for (k, v) in &u.options {
                        cfg.options.entry(k.clone()).or_insert(v.clone());
                    }
                }
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
        let note = match (needs_secret, user.is_none(), password.is_some()) {
            (true, true, _) => Some("Enter the user and password after import".to_string()),
            (true, false, false) => Some("Enter the password after import".to_string()),
            _ => None,
        };
        out.push(Parsed { conn: SharedConnection { name, folder, env, color: None, config: cfg, ai_policy: Default::default() }, secret: password.filter(|_| needs_secret), note });
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
            // Settings it lacks are filled in after import (it is saved anyway).
            note: match (api::validate_config(state, &p.conn.config), &p.note) {
                (Err(e), None) => Some(format!("Complete after import: {}", e.message)),
                (Err(e), Some(n)) if !n.to_lowercase().contains("user") => Some(format!("{n}; complete after import: {}", e.message)),
                (_, n) => n.clone(),
            },
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
        // Saved even when incomplete (no user yet…): finished in its form.
        match api::store_connection(state, api::SaveConnectionArgs { profile, secret: p.secret.clone(), clear_secret: false, extra_secrets: Default::default() }) {
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
        let p = parse_dbeaver(text, &HashMap::new()).unwrap();
        assert_eq!(p.len(), 2);
        let pg = p.iter().find(|x| x.conn.name == "Orders DB").unwrap();
        assert_eq!((pg.conn.config.kind, pg.conn.config.port, pg.conn.env, pg.conn.folder.clone()), (ConnectorKind::Postgres, Some(5433), EnvTag::Prod, vec!["Prod".to_string()]));
        let ssh = pg.conn.config.ssh.as_ref().unwrap();
        assert_eq!((ssh.host.as_str(), ssh.port, ssh.auth.clone()), ("bastion", 2222, SshAuth::Key { path: "/k/id_rsa".into() }));
        let lite = p.iter().find(|x| x.conn.name == "Local").unwrap();
        assert_eq!(lite.conn.config.file_path.as_deref(), Some("/tmp/a.db"));
    }

    #[test]
    fn reads_jdbc_urls() {
        let c = |u: &str| config_from_url(u).unwrap();
        let (pg, u) = c("jdbc:postgresql://db.local:5433/shop?user=ann&ssl=true");
        assert_eq!((pg.kind, pg.host.as_deref(), pg.port, pg.database.as_deref(), u.as_deref()), (ConnectorKind::Postgres, Some("db.local"), Some(5433), Some("shop"), Some("ann")));
        let (ms, _) = c("jdbc:sqlserver://sql1\\inst:1433;databaseName=Sales;encrypt=true");
        assert_eq!((ms.kind, ms.host.as_deref(), ms.port, ms.database.as_deref(), ms.opt("instance")), (ConnectorKind::Mssql, Some("sql1"), Some(1433), Some("Sales"), Some("inst")));
        let (ora, _) = c("jdbc:oracle:thin:@//ora.local:1521/ORCLPDB");
        assert_eq!((ora.host.as_deref(), ora.port, ora.opt("service_name")), (Some("ora.local"), Some(1521), Some("ORCLPDB")));
        let (sid, _) = c("jdbc:oracle:thin:@ora:1521:XE");
        assert_eq!((sid.host.as_deref(), sid.database.as_deref()), (Some("ora"), Some("XE")));
        let (sf, u) = c("jdbc:snowflake://xy12345.eu-west-1.snowflakecomputing.com/?db=ANALYTICS&warehouse=WH&user=BOB");
        assert_eq!((sf.opt("account"), sf.database.as_deref(), sf.opt("warehouse"), u.as_deref()), (Some("xy12345.eu-west-1"), Some("ANALYTICS"), Some("WH"), Some("BOB")));
        let (db, _) = c("jdbc:databricks://adb-1.azuredatabricks.net:443/default;transportMode=http;ssl=1;httpPath=/sql/1.0/warehouses/abc;AuthMech=3");
        assert_eq!((db.host.as_deref(), db.opt("http_path")), (Some("adb-1.azuredatabricks.net"), Some("/sql/1.0/warehouses/abc")));
        let (bq, _) = c("jdbc:bigquery://https://www.googleapis.com/bigquery/v2:443;ProjectId=my-proj;OAuthType=3");
        assert_eq!(bq.opt("project"), Some("my-proj"));
        let (lite, _) = c("jdbc:sqlite:/tmp/a.db");
        assert_eq!(lite.file_path.as_deref(), Some("/tmp/a.db"));
        let (my, u) = c("jdbc:mysql://root@mysql.local/app");
        assert_eq!((my.kind, my.host.as_deref(), my.database.as_deref(), u.as_deref()), (ConnectorKind::Mysql, Some("mysql.local"), Some("app"), Some("root")));
        let (mg, _) = c("mongodb://u:secret@m1:27017/db");
        assert_eq!(mg.opt("uri"), Some("mongodb://u@m1:27017/db"), "no password kept");
        assert!(config_from_url("jdbc:cassandra://x").is_none());
    }

    #[test]
    fn reads_datagrip() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<project version="4">
  <component name="DataSourceManagerImpl" format="xml" multifile-model="true">
    <data-source source="LOCAL" name="shop@localhost" uuid="u-1" group="Prod/EU" read-only="true">
      <driver-ref>postgresql</driver-ref>
      <synchronize>true</synchronize>
      <jdbc-driver>org.postgresql.Driver</jdbc-driver>
      <jdbc-url>jdbc:postgresql://localhost:5432/shop</jdbc-url>
      <ssh-properties><enabled>true</enabled><ssh-config-id>x</ssh-config-id></ssh-properties>
    </data-source>
    <data-source source="LOCAL" name="SF" uuid="u-2">
      <driver-ref>snowflake</driver-ref>
      <jdbc-url>jdbc:snowflake://acct.snowflakecomputing.com/?db=A&amp;warehouse=W</jdbc-url>
    </data-source>
    <data-source source="LOCAL" name="file" uuid="u-3">
      <jdbc-url>jdbc:sqlite:$PROJECT_DIR$/x.db</jdbc-url>
    </data-source>
    <data-source source="LOCAL" name="odd" uuid="u-4"><jdbc-url>jdbc:clickhouse://h</jdbc-url></data-source>
  </component>
</project>"#;
        let local = r#"<project version="4"><component name="dataSourceStorageLocal">
            <data-source name="shop@localhost" uuid="u-1"><secret-storage>master_key</secret-storage><user-name>ann</user-name></data-source>
          </component></project>"#;
        let p = parse_datagrip(xml, local).unwrap();
        assert_eq!(p.len(), 3, "unsupported drivers are skipped");
        let pg = &p[0];
        assert_eq!((pg.conn.name.as_str(), pg.conn.config.host.as_deref(), pg.conn.config.database.as_deref(), pg.conn.folder.clone()), ("shop@localhost", Some("localhost"), Some("shop"), vec!["Prod".to_string(), "EU".to_string()]));
        assert_eq!(user_of(&pg.conn.config).as_deref(), Some("ann"), "user from dataSources.local.xml");
        assert!(pg.conn.config.read_only);
        assert!(pg.note.as_deref().unwrap().contains("SSH"), "{:?}", pg.note);
        assert_eq!((p[1].conn.config.opt("account"), p[1].conn.config.opt("warehouse")), (Some("acct"), Some("W")), "&amp; in the URL");
        assert!(p[1].note.as_deref().unwrap().contains("user and password"));
        assert!(parse_datagrip("<x/>", "").is_err());
    }

    #[test]
    fn reads_dbeaver_credentials() {
        use aes::cipher::{BlockEncryptMut, KeyIvInit, block_padding::Pkcs7};
        let dir = tempfile::tempdir().unwrap();
        let plain = br##"{"postgres-1":{"#connection":{"user":"app","password":"s3cret"}}}"##;
        let iv = [7u8; 16];
        let enc = cbc::Encryptor::<aes::Aes128>::new(&DBEAVER_KEY.into(), &iv.into()).encrypt_padded_vec_mut::<Pkcs7>(plain);
        let f = dir.path().join("credentials-config.json");
        std::fs::write(&f, [iv.as_slice(), &enc].concat()).unwrap();
        let creds = dbeaver_credentials(&f);
        assert_eq!(creds["postgres-1"], (Some("app".into()), Some("s3cret".into())));
        let text = r#"{"connections": {"postgres-1": {"provider": "postgresql", "driver": "postgres-jdbc", "name": "No user in file",
            "configuration": {"host": "pg", "port": "5432", "database": "d"}},
          "mysql-2": {"provider": "mysql", "driver": "mysql8", "name": "URL mode", "configuration": {"url": "jdbc:mysql://my.host:3307/app"}}}}"#;
        let p = parse_dbeaver(text, &creds).unwrap();
        let pg = p.iter().find(|x| x.conn.name == "No user in file").unwrap();
        assert_eq!((user_of(&pg.conn.config).as_deref(), pg.secret.as_deref(), pg.note.as_deref()), (Some("app"), Some("s3cret"), None));
        let my = p.iter().find(|x| x.conn.name == "URL mode").unwrap();
        assert_eq!((my.conn.config.host.as_deref(), my.conn.config.port, my.conn.config.database.as_deref()), (Some("my.host"), Some(3307), Some("app")));
        assert!(dbeaver_credentials(&dir.path().join("missing.json")).is_empty());
    }
}
