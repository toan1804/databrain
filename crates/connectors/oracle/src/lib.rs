//! Oracle connector with two drivers:
//!
//! - **Thin** (default): a helper process using go-ora, downloaded on first
//!   use (see [`agent`]). No Oracle software needed.
//! - **Instant Client** (`driver = instant_client`): the `oracle` crate over
//!   ODPI-C, for features go-ora lacks (e.g. OS authentication). Needs
//!   Oracle Instant Client; its API is synchronous, so calls run on
//!   blocking threads.
//!
//! Catalog queries are the same for both drivers.

pub mod agent;
pub mod client;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use databrain_auth::{AuthMethodKind, Credential, CredentialSource};
use databrain_connector_core::value::{Column, MICROS_PER_DAY, days_from_civil};
use databrain_connector_core::{
    BatchBuilder, Capabilities, ColType, ColumnInfo, ConnectionConfig, Connector, ConnectorError,
    ConnectorInfo, ConnectorKind, DbObject, ErrorKind, ExecOptions, ExecSummary, FieldSpec, ForeignKey, ObjectDetail,
    ObjectKind, QueryStream, Result, SchemaInfo, Session, StreamEvent, StreamSender, TableColumns, Value, quote_ident,
};
use oracle::sql_type::{OracleType, Timestamp, ToSql};
use oracle::{Connection, SqlValue};
use secrecy::ExposeSecret;

#[derive(Debug, Default)]
pub struct OracleConnector;

impl OracleConnector {
    pub fn new() -> Self {
        Self
    }
}

fn init_client(lib_dir: Option<&str>) {
    // Retries on every connect until the library loads (e.g. right after
    // installing Instant Client), using the folder found automatically.
    let _ = client::init_client(lib_dir, &[]);
}

fn map_err(e: oracle::Error) -> ConnectorError {
    if let Some(db) = e.db_error() {
        let code = db.code();
        if code == 1013 {
            // ORA-01013: user requested cancel of current operation
            return ConnectorError::cancelled();
        }
        let kind = if matches!(code, 1017 | 12154 | 12514 | 12541 | 12545 | 12170 | 3113 | 3114 | 3135) {
            ErrorKind::Connection
        } else {
            ErrorKind::Query
        };
        let offset = db.offset();
        let mut ce = ConnectorError::new(kind, db.message().to_string()).with_code(format!("ORA-{code:05}"));
        if offset > 0 {
            ce = ce.with_position(Some(offset + 1));
        }
        return ce;
    }
    let msg = e.to_string();
    if msg.contains("DPI-1047") {
        // `oracle_client_missing` lets the UI offer the one-click install.
        return ConnectorError::connection(format!(
            "Oracle Instant Client was not found (DPI-1047). Install it from the connection dialog (“Install Instant Client”), or set the connection's “Instant Client directory”.\n\n{msg}"
        ))
        .with_code("oracle_client_missing");
    }
    ConnectorError::query(msg)
}

fn connect_string(cfg: &ConnectionConfig) -> String {
    if let Some(cs) = cfg.opt("connect_string") {
        return cs.to_string();
    }
    let service = cfg
        .opt("service_name")
        .or(cfg.database.as_deref().filter(|d| !d.is_empty()))
        .unwrap_or("FREEPDB1");
    format!("{}:{}/{}", cfg.host_or_default(), cfg.port.unwrap_or(1521), service)
}

/// Rows per round trip for catalog queries (the driver default is 100).
const METADATA_FETCH: u32 = 2000;

/// `'A', 'B'` for an `in (…)` list of owners.
fn owner_list(owners: &[String]) -> String {
    owners.iter().map(|o| databrain_connector_core::quote_literal(o)).collect::<Vec<_>>().join(", ")
}

#[derive(Default)]
struct CallState {
    /// The caller stopped waiting.
    abandoned: AtomicBool,
    /// The statement holds the connection now.
    running: AtomicBool,
}

struct BreakOnDrop {
    conn: Arc<Connection>,
    state: Arc<CallState>,
    armed: bool,
}

impl Drop for BreakOnDrop {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        self.state.abandoned.store(true, Ordering::SeqCst);
        if self.state.running.load(Ordering::SeqCst) {
            let _ = self.conn.break_execution();
        }
    }
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T> + Send + 'static) -> Result<T> {
    tokio::task::spawn_blocking(f).await.map_err(|e| ConnectorError::internal(e.to_string()))?
}

#[async_trait]
impl Connector for OracleConnector {
    fn info(&self) -> ConnectorInfo {
        ConnectorInfo {
            kind: ConnectorKind::Oracle,
            display_name: "Oracle",
            default_port: Some(1521),
            uses_file: false,
            auth_methods: vec![AuthMethodKind::Password],
            capabilities: Capabilities { transactions: true, cancel: true, schemas: true, read_only_sessions: true, ssh: true },
            fields: vec![
                FieldSpec::new("host", "Host").required().placeholder("localhost"),
                FieldSpec::new("port", "Port").placeholder("1521"),
                FieldSpec::new("service_name", "Service name").required().placeholder("FREEPDB1 / ORCLPDB1"),
                FieldSpec::new("connect_string", "Connect string").placeholder("(optional) TNS alias or full descriptor")
                    .help("Overrides host/port/service"),
                FieldSpec::new("driver", "Driver").help("Thin needs no Oracle software; Instant Client is for OS authentication and other thick-only features"),
                FieldSpec::new("driver_options", "Driver options")
                    .placeholder("SSL=true; WALLET=/path/to/wallet; ENCRYPTION=REQUIRED")
                    .help("Thin driver: go-ora options separated by ';'"),
                FieldSpec::new("client_lib_dir", "Instant Client directory").placeholder("/opt/oracle/instantclient_23_3"),
            ],
            note: Some("The thin driver (default) needs no Oracle software; it is downloaded on first use (5 MB)."),
        }
    }

    async fn connect(&self, cfg: &ConnectionConfig, creds: Arc<dyn CredentialSource>) -> Result<Box<dyn Session>> {
        Ok(Box::new(open(cfg, creds).await?))
    }
}

async fn open(cfg: &ConnectionConfig, creds: Arc<dyn CredentialSource>) -> Result<OracleSession> {
    {
        let (user, password) = match creds.get().await? {
            Credential::Password { user, password } => (user, password.expose_secret().to_string()),
            _ => return Err(ConnectorError::config("Oracle requires user/password authentication")),
        };
        if !uses_instant_client(cfg) {
            let path = agent::ensure().await?;
            let a = agent::Agent::spawn(&path).await?;
            let options: serde_json::Map<String, serde_json::Value> = parse_driver_options(cfg.opt("driver_options").unwrap_or(""))
                .into_iter()
                .map(|(k, v)| (k, serde_json::Value::String(v)))
                .collect();
            a.call(serde_json::json!({
                "op": "connect",
                "user": user,
                "password": password,
                "host": cfg.host_or_default(),
                "port": cfg.port.unwrap_or(1521),
                "service": cfg.opt("service_name").or(cfg.database.as_deref().filter(|d| !d.is_empty())).unwrap_or("FREEPDB1"),
                "connect_string": cfg.opt("connect_string").unwrap_or(""),
                "options": options,
                "read_only": cfg.read_only,
            }))
            .await?;
            return Ok(OracleSession { backend: Backend::Thin(a) });
        }
        let cs = connect_string(cfg);
        let lib = cfg.opt("client_lib_dir").map(str::to_string);
        let read_only = cfg.read_only;
        let conn = blocking(move || {
            init_client(lib.as_deref());
            let mut c = Connection::connect(&user, &password, &cs).map_err(map_err)?;
            // Read-only: one READ ONLY transaction, never committed (with
            // autocommit on, it would end right after it starts).
            c.set_autocommit(!read_only);
            if read_only {
                c.execute("set transaction read only", &[]).map_err(map_err)?;
            }
            let _ = c.execute("alter session set nls_date_format = 'YYYY-MM-DD HH24:MI:SS'", &[]);
            Ok(c)
        })
        .await?;
        Ok(OracleSession { backend: Backend::Thick { conn: Arc::new(conn), lock: Arc::new(Mutex::new(())) } })
    }
}

/// Instant Client (thick) driver chosen for this connection?
pub fn uses_instant_client(cfg: &ConnectionConfig) -> bool {
    matches!(cfg.opt("driver").map(str::trim), Some("instant_client" | "thick"))
}

/// `KEY=value; KEY=value` (also newlines) → pairs.
fn parse_driver_options(s: &str) -> Vec<(String, String)> {
    s.split([';', '\n'])
        .filter_map(|p| {
            let (k, v) = p.split_once('=')?;
            let k = k.trim();
            (!k.is_empty()).then(|| (k.to_string(), v.trim().to_string()))
        })
        .collect()
}

enum Backend {
    Thick {
        conn: Arc<Connection>,
        /// Serializes statements on the connection.
        lock: Arc<Mutex<()>>,
    },
    Thin(Arc<agent::Agent>),
}

pub struct OracleSession {
    backend: Backend,
}

/// SQL as Oracle accepts it (no trailing `;`, except for PL/SQL blocks) and
/// whether it returns rows.
fn prepare_sql(sql: &str) -> (&str, bool) {
    let trimmed = sql.trim_end();
    let kw = databrain_connector_core::sql::leading_keyword(trimmed);
    let plsql_like = kw == "BEGIN" || kw == "DECLARE" || (kw == "CREATE" && trimmed.to_ascii_uppercase().contains(" END"));
    let sql = if !plsql_like { trimmed.trim_end_matches(';') } else { trimmed };
    (sql, kw == "SELECT" || kw == "WITH")
}

fn col_type(t: &OracleType) -> ColType {
    match t {
        OracleType::Number(p, s) if *s == 0 && *p > 0 && *p <= 18 => ColType::Int64,
        OracleType::Int64 => ColType::Int64,
        OracleType::UInt64 => ColType::UInt64,
        OracleType::BinaryFloat | OracleType::BinaryDouble | OracleType::Float(_) => ColType::Float64,
        OracleType::Date | OracleType::Timestamp(_) => ColType::Timestamp,
        OracleType::TimestampTZ(_) | OracleType::TimestampLTZ(_) => ColType::TimestampTz,
        OracleType::Raw(_) | OracleType::BLOB | OracleType::LongRaw => ColType::Binary,
        OracleType::Boolean => ColType::Bool,
        _ => ColType::Utf8,
    }
}

fn db_type(t: &OracleType) -> String {
    match t {
        OracleType::Number(0, -127) => "NUMBER".into(),
        OracleType::Number(p, s) => format!("NUMBER({p},{s})"),
        other => other.to_string(),
    }
}

fn ts_micros(t: &Timestamp) -> i64 {
    let days = days_from_civil(t.year() as i64, t.month(), t.day());
    let secs = t.hour() as i64 * 3600 + t.minute() as i64 * 60 + t.second() as i64;
    days * MICROS_PER_DAY + secs * 1_000_000 + (t.nanosecond() / 1000) as i64
}

fn convert(t: ColType, v: &SqlValue) -> Value {
    if v.is_null().unwrap_or(true) {
        return Value::Null;
    }
    let text = || v.get::<String>().map(Value::Text).unwrap_or_else(|_| Value::Text(v.to_string()));
    match t {
        ColType::Int64 => v.get::<i64>().map(Value::Int).unwrap_or_else(|_| text()),
        ColType::UInt64 => v.get::<u64>().map(Value::UInt).unwrap_or_else(|_| text()),
        ColType::Float64 => v.get::<f64>().map(Value::Float).unwrap_or_else(|_| text()),
        ColType::Bool => v.get::<bool>().map(Value::Bool).unwrap_or_else(|_| text()),
        ColType::Binary => v.get::<Vec<u8>>().map(Value::Bytes).unwrap_or_else(|_| text()),
        ColType::Timestamp => v.get::<Timestamp>().map(|ts| Value::Timestamp(ts_micros(&ts))).unwrap_or_else(|_| text()),
        ColType::TimestampTz => v
            .get::<Timestamp>()
            .map(|ts| Value::TimestampTz(ts_micros(&ts) - ts.tz_offset() as i64 * 1_000_000))
            .unwrap_or_else(|_| text()),
        _ => text(),
    }
}

fn run(conn: &Connection, sql: &str, opts: &ExecOptions, tx: &StreamSender) -> Result<()> {
    // Oracle rejects a trailing semicolon in SQL (but PL/SQL blocks need theirs).
    let (sql, _) = prepare_sql(sql);
    let batch = opts.batch_size.clamp(1, 10_000);
    let mut stmt = conn
        .statement(sql)
        .fetch_array_size(batch as u32)
        .prefetch_rows(batch as u32 + 1)
        .build()
        .map_err(map_err)?;
    if !stmt.is_query() {
        stmt.execute(&[]).map_err(map_err)?;
        let n = stmt.row_count().ok();
        tx.blocking_send(Ok(StreamEvent::Done(ExecSummary { rows_affected: n })));
        return Ok(());
    }
    let rows = stmt.query(&[]).map_err(map_err)?;
    let cols: Vec<Column> = rows
        .column_info()
        .iter()
        .map(|c| Column::new(c.name(), col_type(c.oracle_type()), db_type(c.oracle_type())))
        .collect();
    let types: Vec<ColType> = cols.iter().map(|c| c.col_type).collect();
    let mut b = BatchBuilder::new(&cols, batch);
    if !tx.blocking_send(Ok(StreamEvent::Schema(b.schema()))) {
        return Ok(());
    }
    for (n, row) in rows.enumerate() {
        if opts.max_rows.is_some_and(|m| n >= m) {
            break; // the caller keeps no more
        }
        let row = row.map_err(map_err)?;
        b.push_row(row.sql_values().iter().zip(&types).map(|(v, t)| convert(*t, v)));
        if b.is_full() && !tx.blocking_send(Ok(StreamEvent::Batch(b.finish()?))) {
            return Ok(());
        }
        if opts.cancel.is_cancelled() {
            return Err(ConnectorError::cancelled());
        }
    }
    if !b.is_empty() && !tx.blocking_send(Ok(StreamEvent::Batch(b.finish()?))) {
        return Ok(());
    }
    tx.blocking_send(Ok(StreamEvent::Done(ExecSummary::default())));
    Ok(())
}

impl OracleSession {
    /// Three dictionary queries for the whole batch of owners, with primary
    /// keys joined once (not looked up per column) and large fetches.
    async fn bulk_metadata_batched(&self, schemas: &[String]) -> Result<Vec<databrain_connector_core::SchemaMetadata>> {
        if schemas.is_empty() {
            return Ok(vec![]);
        }
        let owners = owner_list(schemas);
        let obj_rows = self
            .query_dynamic(format!(
                "select o.owner, o.object_name, o.object_type, c.comments, to_char(t.num_rows) \
                 from all_objects o \
                 left join all_tab_comments c on c.owner = o.owner and c.table_name = o.object_name \
                 left join all_tables t on t.owner = o.owner and t.table_name = o.object_name \
                 where o.owner in ({owners}) and o.object_type in ('TABLE','VIEW','MATERIALIZED VIEW') and o.object_name not like 'BIN$%' \
                 order by o.owner, o.object_name, case o.object_type when 'MATERIALIZED VIEW' then 0 else 1 end"
            ))
            .await?;
        let mut objects: Vec<DbObject> = Vec::with_capacity(obj_rows.len());
        for r in obj_rows {
            let g = |i: usize| r.get(i).cloned().flatten();
            let (Some(owner), Some(name)) = (g(0), g(1)) else { continue };
            // A materialized view is listed twice (also as TABLE): keep the first.
            if objects.last().is_some_and(|o| o.schema == owner && o.name == name) {
                continue;
            }
            let kind = match g(2).as_deref() {
                Some("VIEW") => ObjectKind::View,
                Some("MATERIALIZED VIEW") => ObjectKind::MaterializedView,
                _ => ObjectKind::Table,
            };
            objects.push(DbObject { schema: owner, name, kind, comment: g(3), row_estimate: g(4).and_then(|n| n.parse().ok()) });
        }
        let col_rows = self
            .query_dynamic(format!(
                "with pk as ( \
                   select cc.owner, cc.table_name, cc.column_name from all_constraints k \
                   join all_cons_columns cc on cc.owner = k.owner and cc.constraint_name = k.constraint_name \
                   where k.constraint_type = 'P' and k.owner in ({owners})) \
                 select c.owner, c.table_name, c.column_name, \
                   c.data_type || case when c.data_type in ('VARCHAR2','NVARCHAR2','CHAR','NCHAR','RAW') then '(' || c.char_length || ')' \
                                       when c.data_type = 'NUMBER' and c.data_precision is not null then '(' || c.data_precision || ',' || c.data_scale || ')' \
                                       else '' end, \
                   c.nullable, case when pk.column_name is not null then 'Y' else 'N' end, cm.comments \
                 from all_tab_columns c \
                 left join pk on pk.owner = c.owner and pk.table_name = c.table_name and pk.column_name = c.column_name \
                 left join all_col_comments cm on cm.owner = c.owner and cm.table_name = c.table_name and cm.column_name = c.column_name \
                 where c.owner in ({owners}) and c.table_name not like 'BIN$%' \
                 order by c.owner, c.table_name, c.column_id"
            ))
            .await?;
        let columns = col_rows
            .into_iter()
            .map(|r| {
                let g = |i: usize| r.get(i).cloned().flatten();
                (
                    g(0).unwrap_or_default(),
                    g(1).unwrap_or_default(),
                    ColumnInfo {
                        name: g(2).unwrap_or_default(),
                        data_type: g(3).unwrap_or_default(),
                        nullable: g(4).as_deref() != Some("N"),
                        is_primary_key: g(5).as_deref() == Some("Y"),
                        default: None,
                        comment: g(6),
                    },
                )
            })
            .collect();
        let fk_rows = self
            .query_dynamic(format!(
                "select a.owner, a.table_name, a.constraint_name, a.column_name, r.owner, r.table_name, r.column_name \
                 from all_constraints k \
                 join all_cons_columns a on a.owner = k.owner and a.constraint_name = k.constraint_name \
                 join all_cons_columns r on r.owner = k.r_owner and r.constraint_name = k.r_constraint_name and r.position = a.position \
                 where k.constraint_type = 'R' and k.owner in ({owners}) \
                 order by a.owner, a.table_name, a.constraint_name, a.position"
            ))
            .await?;
        // Rows of one constraint are adjacent (ordered by owner, table, name).
        let mut fks: Vec<(String, String, ForeignKey)> = Vec::new();
        let mut last: Option<(String, String, String)> = None;
        for r in fk_rows {
            let g = |i: usize| r.get(i).cloned().flatten().unwrap_or_default();
            let key = (g(0), g(1), g(2));
            if last.as_ref() == Some(&key) {
                if let Some((_, _, fk)) = fks.last_mut() {
                    fk.columns.push(g(3));
                    fk.ref_columns.push(g(6));
                }
            } else {
                fks.push((key.0.clone(), key.1.clone(), ForeignKey { columns: vec![g(3)], ref_schema: g(4), ref_table: g(5), ref_columns: vec![g(6)] }));
                last = Some(key);
            }
        }
        Ok(databrain_connector_core::assemble_metadata(schemas, objects, columns, fks))
    }

    async fn with_conn<T: Send + 'static>(&self, f: impl FnOnce(&Connection) -> Result<T> + Send + 'static) -> Result<T> {
        let Backend::Thick { conn, lock } = &self.backend else {
            return Err(ConnectorError::internal("Instant Client call on a thin connection"));
        };
        let (conn, lock) = (conn.clone(), lock.clone());
        // The blocking call can't be aborted. If the caller stops waiting,
        // `BreakOnDrop` skips it when it hasn't started, or breaks the
        // statement (OCIBreak → ORA-01013) when it is running, so the
        // connection is free for the next caller.
        let state = Arc::new(CallState::default());
        let mut guard = BreakOnDrop { conn: conn.clone(), state: state.clone(), armed: true };
        let r = blocking(move || {
            let _g = lock.lock().map_err(|_| ConnectorError::internal("lock poisoned"))?;
            if state.abandoned.load(Ordering::SeqCst) {
                return Err(ConnectorError::cancelled());
            }
            state.running.store(true, Ordering::SeqCst);
            let r = f(&conn);
            state.running.store(false, Ordering::SeqCst);
            r
        })
        .await;
        guard.armed = false; // finished: nothing to stop
        r
    }

    async fn query_named(&self, sql: &'static str, params: Vec<(&'static str, Option<String>)>) -> Result<Vec<Vec<Option<String>>>> {
        if let Backend::Thin(a) = &self.backend {
            return a.text(sql, &params).await;
        }
        self.with_conn(move |c| {
            let binds: Vec<(&str, &dyn ToSql)> = params.iter().map(|(k, v)| (*k, v as &dyn ToSql)).collect();
            // Catalog queries return many rows: fetch them in large round trips.
            let mut st = c.statement(sql).fetch_array_size(METADATA_FETCH).prefetch_rows(METADATA_FETCH).build().map_err(map_err)?;
            let rs = st.query_named(&binds).map_err(map_err)?;
            let mut out = Vec::new();
            for r in rs {
                let r = r.map_err(map_err)?;
                out.push(
                    r.sql_values()
                        .iter()
                        .map(|v| if v.is_null().unwrap_or(true) { None } else { v.get::<String>().ok() })
                        .collect(),
                );
            }
            Ok(out)
        })
        .await
    }

    /// A catalog query built at runtime (owner lists), rows as text.
    async fn query_dynamic(&self, sql: String) -> Result<Vec<Vec<Option<String>>>> {
        if let Backend::Thin(a) = &self.backend {
            return a.text(&sql, &[]).await;
        }
        self.with_conn(move |c| {
            let mut st = c.statement(&sql).fetch_array_size(METADATA_FETCH).prefetch_rows(METADATA_FETCH).build().map_err(map_err)?;
            let rs = st.query(&[]).map_err(map_err)?;
            let mut out = Vec::new();
            for r in rs {
                let r = r.map_err(map_err)?;
                out.push(r.sql_values().iter().map(|v| if v.is_null().unwrap_or(true) { None } else { v.get::<String>().ok() }).collect());
            }
            Ok(out)
        })
        .await
    }

    async fn columns(&self, owner: &str, table: Option<&str>) -> Result<Vec<(String, ColumnInfo)>> {
        let rows = self
            .query_named(
                "select c.table_name, c.column_name, \
                   c.data_type || case when c.data_type in ('VARCHAR2','NVARCHAR2','CHAR','NCHAR','RAW') then '(' || c.char_length || ')' \
                                       when c.data_type = 'NUMBER' and c.data_precision is not null then '(' || c.data_precision || ',' || c.data_scale || ')' \
                                       else '' end, \
                   c.nullable, \
                   case when exists (select 1 from all_cons_columns cc join all_constraints k \
                        on k.owner = cc.owner and k.constraint_name = cc.constraint_name \
                        where k.constraint_type = 'P' and cc.owner = c.owner and cc.table_name = c.table_name and cc.column_name = c.column_name) \
                        then 'Y' else 'N' end, \
                   cm.comments \
                 from all_tab_columns c \
                 left join all_col_comments cm on cm.owner = c.owner and cm.table_name = c.table_name and cm.column_name = c.column_name \
                 where c.owner = :owner and (:tbl is null or c.table_name = :tbl) \
                 order by c.table_name, c.column_id",
                vec![("owner", Some(owner.to_string())), ("tbl", table.map(str::to_string))],
            )
            .await?;
        Ok(rows
            .into_iter()
            .map(|r| {
                let g = |i: usize| r.get(i).cloned().flatten();
                (
                    g(0).unwrap_or_default(),
                    ColumnInfo {
                        name: g(1).unwrap_or_default(),
                        data_type: g(2).unwrap_or_default(),
                        nullable: g(3).as_deref() != Some("N"),
                        is_primary_key: g(4).as_deref() == Some("Y"),
                        default: None,
                        comment: g(5),
                    },
                )
            })
            .collect())
    }

    async fn foreign_keys(&self, owner: &str, table: Option<&str>) -> Result<Vec<(String, ForeignKey)>> {
        let rows = self
            .query_named(
                "select a.table_name, a.constraint_name, a.column_name, r.owner, r.table_name, r.column_name \
                 from all_constraints k \
                 join all_cons_columns a on a.owner = k.owner and a.constraint_name = k.constraint_name \
                 join all_cons_columns r on r.owner = k.r_owner and r.constraint_name = k.r_constraint_name and r.position = a.position \
                 where k.constraint_type = 'R' and k.owner = :owner and (:tbl is null or k.table_name = :tbl) \
                 order by a.table_name, a.constraint_name, a.position",
                vec![("owner", Some(owner.to_string())), ("tbl", table.map(str::to_string))],
            )
            .await?;
        let mut out: Vec<(String, String, ForeignKey)> = Vec::new();
        for r in rows {
            let g = |i: usize| r.get(i).cloned().flatten().unwrap_or_default();
            let (t, n) = (g(0), g(1));
            match out.iter_mut().find(|(tt, nn, _)| *tt == t && *nn == n) {
                Some((_, _, fk)) => {
                    fk.columns.push(g(2));
                    fk.ref_columns.push(g(5));
                }
                None => out.push((t, n, ForeignKey { columns: vec![g(2)], ref_schema: g(3), ref_table: g(4), ref_columns: vec![g(5)] })),
            }
        }
        Ok(out.into_iter().map(|(t, _, f)| (t, f)).collect())
    }
}

#[async_trait]
impl Session for OracleSession {
    fn kind(&self) -> ConnectorKind {
        ConnectorKind::Oracle
    }

    async fn server_version(&self) -> Result<String> {
        if let Backend::Thin(a) = &self.backend {
            for sql in ["select banner from v$version where rownum = 1", "select 'Oracle ' || version from product_component_version where rownum = 1"] {
                if let Ok(rows) = a.text(sql, &[]).await {
                    if let Some(v) = rows.into_iter().next().and_then(|r| r.into_iter().next().flatten()) {
                        return Ok(v);
                    }
                }
            }
            return Ok("Oracle".into());
        }
        self.with_conn(|c| {
            let (v, banner) = c.server_version().map_err(map_err)?;
            Ok(if banner.is_empty() { format!("Oracle {v}") } else { banner.lines().next().unwrap_or("").to_string() })
        })
        .await
    }

    async fn ping(&self) -> Result<()> {
        if let Backend::Thin(a) = &self.backend {
            return a.call(serde_json::json!({"op": "ping"})).await.map(|_| ());
        }
        self.with_conn(|c| c.ping().map_err(map_err)).await
    }

    async fn execute(&self, sql: &str, opts: ExecOptions) -> Result<QueryStream> {
        let (conn, lock) = match &self.backend {
            Backend::Thin(a) => return Ok(thin_execute(a.clone(), sql, opts)),
            Backend::Thick { conn, lock } => (conn.clone(), lock.clone()),
        };
        let (tx, stream) = QueryStream::channel(4);
        let sql = sql.to_string();
        let done = tokio_util_token();
        let (cancel, done2) = (opts.cancel.clone(), done.clone());
        let breaker = conn.clone();
        tokio::spawn(async move {
            tokio::select! {
                _ = cancel.cancelled() => {
                    let _ = tokio::task::spawn_blocking(move || breaker.break_execution()).await;
                }
                _ = done2.cancelled() => {}
            }
        });
        tokio::task::spawn_blocking(move || {
            let _done = done.drop_guard();
            let Ok(_g) = lock.lock() else {
                tx.blocking_send(Err(ConnectorError::internal("lock poisoned")));
                return;
            };
            if let Err(e) = run(&conn, &sql, &opts, &tx) {
                let e = if opts.cancel.is_cancelled() { ConnectorError::cancelled() } else { e };
                tx.blocking_send(Err(e));
            }
        });
        Ok(stream)
    }

    async fn list_schemas(&self) -> Result<Vec<SchemaInfo>> {
        let sql_12c = "select username, case when username = sys_context('USERENV','CURRENT_SCHEMA') then 'Y' else 'N' end \
                       from all_users where oracle_maintained = 'N' or username = sys_context('USERENV','CURRENT_SCHEMA') \
                       order by 2 desc, 1";
        let rows = match self.query_named(sql_12c, vec![]).await {
            Ok(r) => r,
            Err(_) => {
                self.query_named(
                    "select username, case when username = sys_context('USERENV','CURRENT_SCHEMA') then 'Y' else 'N' end \
                     from all_users order by 2 desc, 1",
                    vec![],
                )
                .await?
            }
        };
        Ok(rows
            .into_iter()
            .map(|r| SchemaInfo {
                name: r.first().cloned().flatten().unwrap_or_default(),
                is_default: r.get(1).cloned().flatten().as_deref() == Some("Y"),
                catalog: None,
            })
            .collect())
    }

    async fn list_objects(&self, schema: &str) -> Result<Vec<DbObject>> {
        let rows = self
            .query_named(
                "select o.object_name, o.object_type, c.comments, to_char(t.num_rows) \
                 from all_objects o \
                 left join all_tab_comments c on c.owner = o.owner and c.table_name = o.object_name \
                 left join all_tables t on t.owner = o.owner and t.table_name = o.object_name \
                 where o.owner = :owner and o.object_type in ('TABLE','VIEW','MATERIALIZED VIEW','PROCEDURE','FUNCTION','PACKAGE','SEQUENCE') \
                   and o.object_name not like 'BIN$%' \
                 order by case o.object_type when 'TABLE' then 0 when 'VIEW' then 1 when 'MATERIALIZED VIEW' then 1 else 2 end, o.object_name",
                vec![("owner", Some(schema.to_string()))],
            )
            .await?;
        let mut seen = std::collections::HashSet::new();
        Ok(rows
            .into_iter()
            .filter_map(|r| {
                let g = |i: usize| r.get(i).cloned().flatten();
                let name = g(0)?;
                let kind = match g(1)?.as_str() {
                    "TABLE" => ObjectKind::Table,
                    "VIEW" => ObjectKind::View,
                    "MATERIALIZED VIEW" => ObjectKind::MaterializedView,
                    "PROCEDURE" => ObjectKind::Procedure,
                    "PACKAGE" => ObjectKind::Package,
                    "SEQUENCE" => ObjectKind::Sequence,
                    _ => ObjectKind::Function,
                };
                // A materialized view also appears as a TABLE.
                if !seen.insert(name.clone()) {
                    return None;
                }
                Some(DbObject { schema: schema.to_string(), name, kind, comment: g(2), row_estimate: g(3).and_then(|n| n.parse().ok()) })
            })
            .collect())
    }

    async fn table_layout(&self, schema: &str, name: &str) -> Result<databrain_connector_core::TableLayout> {
        use databrain_connector_core::{group_indexes, query_text, quote_literal, truthy, TableLayout};
        let (s, t) = (quote_literal(schema), quote_literal(name));
        let idx = query_text(
            self,
            &format!(
                "select ic.index_name, ic.column_name, case when i.uniqueness = 'UNIQUE' then 1 else 0 end, \
                        case when c.constraint_name is not null then 1 else 0 end, i.index_type \
                 from all_ind_columns ic \
                 join all_indexes i on i.owner = ic.index_owner and i.index_name = ic.index_name \
                 left join all_constraints c on c.owner = i.table_owner and c.index_name = i.index_name and c.constraint_type = 'P' \
                 where ic.table_owner = {s} and ic.table_name = {t} \
                 order by 4 desc, ic.index_name, ic.column_position"
            ),
        )
        .await?;
        let mut l = TableLayout {
            indexes: group_indexes(idx.into_iter().map(|r| {
                let m = r[4].clone().map(|m| m.to_ascii_lowercase()).filter(|m| m != "normal");
                (r[0].clone().unwrap_or_default(), r[1].clone().unwrap_or_default(), truthy(&r[2]), truthy(&r[3]), m)
            })),
            ..Default::default()
        };
        if let Ok(p) = query_text(
            self,
            &format!(
                "select k.column_name, lower(p.partitioning_type), p.partition_count from all_part_key_columns k \
                 join all_part_tables p on p.owner = k.owner and p.table_name = k.name \
                 where k.owner = {s} and k.name = {t} and k.object_type = 'TABLE' order by k.column_position"
            ),
        )
        .await
        {
            l.partition_by = p.iter().filter_map(|r| r[0].clone()).collect();
            l.partition_kind = p.first().and_then(|r| r[1].clone());
            if let Some(n) = p.first().and_then(|r| r[2].clone()) {
                l.notes.push(format!("{n} partitions"));
            }
        }
        if let Ok(r) = query_text(self, &format!("select num_rows from all_tables where owner = {s} and table_name = {t}")).await {
            l.row_estimate = r.first().and_then(|r| r[0].as_deref()).and_then(|x| x.parse().ok());
        }
        Ok(l)
    }

    async fn describe(&self, schema: &str, name: &str) -> Result<ObjectDetail> {
        let object = self
            .list_objects(schema)
            .await?
            .into_iter()
            .filter(|o| o.name == name)
            .min_by_key(|o| !o.kind.is_relation())
            .ok_or_else(|| ConnectorError::query(format!("object not found: {schema}.{name}")))?;
        let columns: Vec<ColumnInfo> = self.columns(schema, Some(name)).await?.into_iter().map(|(_, c)| c).collect();
        let obj_type = match object.kind {
            ObjectKind::View => "VIEW",
            ObjectKind::MaterializedView => "MATERIALIZED_VIEW",
            ObjectKind::Procedure => "PROCEDURE",
            ObjectKind::Function => "FUNCTION",
            ObjectKind::Package => "PACKAGE",
            ObjectKind::Sequence => "SEQUENCE",
            _ => "TABLE",
        };
        let (s, n) = (schema.to_string(), name.to_string());
        let ddl = if let Backend::Thin(a) = &self.backend {
            a.text("select dbms_metadata.get_ddl(:t, :n, :s) from dual", &[("t", Some(obj_type.to_string())), ("n", Some(n)), ("s", Some(s))])
                .await
                .ok()
                .and_then(|r| r.into_iter().next().and_then(|r| r.into_iter().next().flatten()))
                .map(|d| d.trim().to_string())
        } else {
            self.with_conn(move |c| {
                let r = c.query_row_as::<String>(
                    "select dbms_metadata.get_ddl(:1, :2, :3) from dual",
                    &[&obj_type, &n, &s],
                );
                Ok(r.ok().map(|d| d.trim().to_string()))
            })
            .await?
        };
        let ddl = ddl.or_else(|| {
                (object.kind == ObjectKind::Table).then(|| {
                    let cols: Vec<String> = columns
                        .iter()
                        .map(|c| {
                            format!(
                                "    {} {}{}",
                                quote_ident(ConnectorKind::Oracle, &c.name),
                                c.data_type,
                                if c.nullable { "" } else { " NOT NULL" }
                            )
                        })
                        .collect();
                    format!(
                        "CREATE TABLE {}.{} (\n{}\n);",
                        quote_ident(ConnectorKind::Oracle, schema),
                        quote_ident(ConnectorKind::Oracle, name),
                        cols.join(",\n")
                    )
                })
            });
        let foreign_keys = self.foreign_keys(schema, Some(name)).await?.into_iter().map(|(_, f)| f).collect();
        Ok(ObjectDetail { object, columns, ddl, foreign_keys })
    }

    /// Last DDL time and object count per owner (one aggregate over
    /// ALL_OBJECTS): ALTER, COMMENT, CREATE and DROP all change it.
    /// One query over ALL_OBJECTS: the schema's (or the current schema's)
    /// routines first, then other owners' (e.g. SYS's DBMS_OUTPUT, used
    /// through public synonyms) when no schema was written.
    async fn search_routines(&self, schema: Option<&str>, query: &str, limit: usize) -> Result<Vec<DbObject>> {
        let q = query.trim().to_uppercase().replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_");
        let rows = self
            .query_named(
                "select owner, object_name, object_type from ( \
                   select owner, object_name, object_type from all_objects \
                   where object_type in ('FUNCTION','PROCEDURE','PACKAGE') \
                     and object_name like '%' || :q || '%' escape '\\' \
                     and (owner = nvl(:owner, sys_context('USERENV','CURRENT_SCHEMA')) or :owner is null) \
                   order by case when owner = nvl(:owner, sys_context('USERENV','CURRENT_SCHEMA')) then 0 else 1 end, \
                            case when object_name like :q || '%' escape '\\' then 0 else 1 end, length(object_name), object_name, owner \
                 ) where rownum <= :lim",
                vec![("q", Some(q)), ("owner", schema.map(str::to_string)), ("lim", Some(limit.clamp(1, 1000).to_string()))],
            )
            .await?;
        Ok(rows
            .into_iter()
            .filter_map(|r| {
                let g = |i: usize| r.get(i).cloned().flatten();
                let kind = match g(2)?.as_str() {
                    "PROCEDURE" => ObjectKind::Procedure,
                    "PACKAGE" => ObjectKind::Package,
                    _ => ObjectKind::Function,
                };
                Some(DbObject { schema: g(0)?, name: g(1)?, kind, comment: None, row_estimate: None })
            })
            .collect())
    }

    /// Public functions/procedures of a package (overloads once). A member
    /// with a return value (argument at position 0) is a function.
    async fn package_members(&self, schema: &str, package: &str) -> Result<Vec<DbObject>> {
        let rows = self
            .query_named(
                "select p.procedure_name, \
                        max(case when exists (select 1 from all_arguments a where a.owner = p.owner and a.package_name = p.object_name \
                                 and a.object_name = p.procedure_name and a.position = 0 and a.argument_name is null) then 1 else 0 end) \
                 from all_procedures p \
                 where p.owner = :owner and p.object_name = :pkg and p.procedure_name is not null \
                 group by p.procedure_name order by p.procedure_name",
                vec![("owner", Some(schema.to_string())), ("pkg", Some(package.to_string()))],
            )
            .await?;
        Ok(rows
            .into_iter()
            .filter_map(|r| {
                let name = r.first().cloned().flatten()?;
                let function = r.get(1).cloned().flatten().as_deref() == Some("1");
                Some(DbObject {
                    schema: format!("{schema}.{package}"),
                    name,
                    kind: if function { ObjectKind::Function } else { ObjectKind::Procedure },
                    comment: None,
                    row_estimate: None,
                })
            })
            .collect())
    }

    async fn schema_fingerprints(&self) -> Result<Option<std::collections::HashMap<String, String>>> {
        let rows = self
            .query_named(
                "select owner, count(*) || '/' || to_char(max(last_ddl_time), 'YYYYMMDDHH24MISS') \
                 from all_objects where object_type in ('TABLE','VIEW','MATERIALIZED VIEW') and object_name not like 'BIN$%' \
                 group by owner",
                vec![],
            )
            .await?;
        Ok(Some(rows.into_iter().filter_map(|r| Some((r.first().cloned().flatten()?, r.get(1).cloned().flatten().unwrap_or_default()))).collect()))
    }

    async fn bulk_metadata(&self, schemas: &[String]) -> Result<Vec<databrain_connector_core::SchemaMetadata>> {
        match self.bulk_metadata_batched(schemas).await {
            Ok(m) => Ok(m),
            // E.g. no privilege on one catalog view: schema by schema reports per-schema errors.
            Err(_) => databrain_connector_core::default_bulk_metadata(self, schemas).await,
        }
    }

    async fn schema_columns(&self, schema: &str) -> Result<Vec<TableColumns>> {
        let mut out: Vec<TableColumns> = Vec::new();
        for (table, col) in self.columns(schema, None).await? {
            match out.last_mut() {
                Some(t) if t.table == table => t.columns.push(col),
                _ => out.push(TableColumns { table, columns: vec![col], foreign_keys: vec![] }),
            }
        }
        for (table, fk) in self.foreign_keys(schema, None).await? {
            if let Some(t) = out.iter_mut().find(|t| t.table == table) {
                t.foreign_keys.push(fk);
            }
        }
        Ok(out)
    }
}

/// Run a statement through the thin driver, streaming batches as they arrive.
fn thin_execute(a: Arc<agent::Agent>, sql: &str, opts: ExecOptions) -> QueryStream {
    let (tx, stream) = QueryStream::channel(4);
    let (sql, is_query) = prepare_sql(sql);
    let sql = sql.to_string();
    tokio::spawn(async move {
        if let Err(e) = thin_run(&a, &sql, is_query, &opts, &tx).await {
            let e = if opts.cancel.is_cancelled() { ConnectorError::cancelled() } else { e };
            tx.send_err(e).await;
        }
    });
    stream
}

async fn thin_run(a: &agent::Agent, sql: &str, is_query: bool, opts: &ExecOptions, tx: &StreamSender) -> Result<()> {
    use agent::Frame;
    let batch = opts.batch_size.clamp(1, 10_000);
    let req = serde_json::json!({
        "op": if is_query { "query" } else { "exec" },
        "sql": sql,
        "batch_size": batch,
        "max_rows": opts.max_rows.unwrap_or(0),
    });
    let (id, mut r) = a.start(req).await?;
    let mut builder: Option<(BatchBuilder, usize)> = None;
    let mut cancelled = false;
    // Stop asked (Stop button / row cap / consumer gone): tell the helper,
    // then keep reading until it ends the request.
    loop {
        let frame = tokio::select! {
            f = agent::Agent::next(&mut r, id) => f?,
            _ = opts.cancel.cancelled(), if !cancelled => {
                cancelled = true;
                a.cancel(id).await;
                continue;
            }
        };
        match frame {
            Frame::Json(j) => match j["type"].as_str() {
                Some("schema") => {
                    let cols: Vec<Column> = j["columns"]
                        .as_array()
                        .map(|c| {
                            c.iter()
                                .map(|c| {
                                    let t = match c["type"].as_str() {
                                        Some("int64") => ColType::Int64,
                                        Some("float64") => ColType::Float64,
                                        Some("timestamp") => ColType::Timestamp,
                                        Some("timestamptz") => ColType::TimestampTz,
                                        Some("binary") => ColType::Binary,
                                        Some("bool") => ColType::Bool,
                                        _ => ColType::Utf8,
                                    };
                                    Column::new(c["name"].as_str().unwrap_or(""), t, c["db_type"].as_str().unwrap_or("").to_string())
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    let b = BatchBuilder::new(&cols, batch);
                    if !cancelled && !tx.send(StreamEvent::Schema(b.schema())).await {
                        cancelled = true;
                        a.cancel(id).await;
                    }
                    builder = Some((b, cols.len()));
                }
                Some("done") => {
                    if cancelled {
                        return Err(ConnectorError::cancelled());
                    }
                    if let Some((b, _)) = builder.as_mut() {
                        if !b.is_empty() {
                            tx.send(StreamEvent::Batch(b.finish()?)).await;
                        }
                    }
                    let rows_affected = j["rows_affected"].as_u64();
                    tx.send(StreamEvent::Done(ExecSummary { rows_affected })).await;
                    return Ok(());
                }
                Some("error") => return Err(agent::reply_error(&j)),
                _ => {}
            },
            Frame::Batch { ncols, nrows, data, .. } => {
                if cancelled {
                    continue;
                }
                let Some((b, n)) = builder.as_mut() else { continue };
                if *n != ncols {
                    return Err(ConnectorError::internal("Oracle driver sent a batch with the wrong column count"));
                }
                agent::decode_rows(&data, ncols, nrows, |row| b.push_row(row.into_iter()))?;
                if b.is_full() && !tx.send(StreamEvent::Batch(b.finish()?)).await {
                    cancelled = true;
                    a.cancel(id).await;
                }
            }
        }
    }
}

fn tokio_util_token() -> databrain_connector_core::CancellationToken {
    databrain_connector_core::CancellationToken::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use databrain_auth::AuthMethod;

    #[test]
    fn builds_connect_string() {
        let mut c = ConnectionConfig::new(ConnectorKind::Oracle, AuthMethod::Password { user: "u".into() });
        c.host = Some("db".into());
        c.options.insert("service_name".into(), "ORCLPDB1".into());
        assert_eq!(connect_string(&c), "db:1521/ORCLPDB1");
        c.options.insert("connect_string".into(), "prod_tns".into());
        assert_eq!(connect_string(&c), "prod_tns");
    }

    #[test]
    fn maps_types() {
        assert_eq!(col_type(&OracleType::Number(10, 0)), ColType::Int64);
        assert_eq!(col_type(&OracleType::Number(0, -127)), ColType::Utf8);
        assert_eq!(col_type(&OracleType::Number(10, 2)), ColType::Utf8);
        assert_eq!(db_type(&OracleType::Number(10, 2)), "NUMBER(10,2)");
        assert_eq!(col_type(&OracleType::Date), ColType::Timestamp);
    }

    fn _assert_send_sync() {
        fn is<T: Send + Sync>() {}
        is::<Connection>();
    }

    async fn live(driver: &str) -> Option<Box<dyn Session>> {
        live_with(driver, false).await
    }

    async fn live_with(driver: &str, read_only: bool) -> Option<Box<dyn Session>> {
        Some(Box::new(live_session(driver, read_only).await?))
    }

    async fn live_session(driver: &str, read_only: bool) -> Option<OracleSession> {
        use databrain_auth::InlineCredentialSource;
        let host = std::env::var("DATABRAIN_ORACLE_HOST").ok()?;
        let user = std::env::var("DATABRAIN_ORACLE_USER").unwrap_or_else(|_| "system".into());
        let mut cfg = ConnectionConfig::new(ConnectorKind::Oracle, AuthMethod::Password { user: user.clone() });
        cfg.host = Some(host);
        cfg.port = std::env::var("DATABRAIN_ORACLE_PORT").ok().and_then(|p| p.parse().ok());
        cfg.options.insert("service_name".into(), std::env::var("DATABRAIN_ORACLE_SERVICE").unwrap_or_else(|_| "FREEPDB1".into()));
        cfg.options.insert("driver".into(), driver.into());
        cfg.read_only = read_only;
        let creds = Arc::new(InlineCredentialSource::new(AuthMethod::Password { user }, std::env::var("DATABRAIN_ORACLE_PASSWORD").ok().map(secrecy::SecretString::from)));
        Some(open(&cfg, creds).await.unwrap())
    }

    /// Live, thin driver (needs a built helper: `node scripts/build-oracle-agent.mjs --host`).
    #[tokio::test]
    async fn live_thin_driver() {
        check_driver("thin").await;
    }

    /// Live, Instant Client driver: same checks (DATABRAIN_ORACLE_LIB_DIR).
    #[tokio::test]
    async fn live_instant_client_driver() {
        let Ok(d) = std::env::var("DATABRAIN_ORACLE_LIB_DIR") else {
            return eprintln!("skipping: DATABRAIN_ORACLE_LIB_DIR not set");
        };
        init_client(Some(&d));
        check_driver("instant_client").await;
    }

    async fn run(s: &OracleSession, sql: &str) {
        s.execute(sql, ExecOptions::default()).await.unwrap().collect().await.unwrap_or_else(|e| panic!("{sql}: {e}"));
    }

    /// Routine completion: standalone functions/procedures, packages and
    /// their members (functions vs procedures), other owners' packages.
    async fn check_routines(driver: &str) {
        let Some(s) = live_session(driver, false).await else { return };
        for ddl in [
            "create or replace function db_fn_total(x number) return number is begin return x * 2; end;",
            "create or replace procedure db_proc_log(m varchar2) is begin null; end;",
            "create or replace package db_pkg_sales as function net(x number) return number; function net(x number, y number) return number; procedure refresh; end;",
        ] {
            run(&s, ddl).await;
        }
        let me = s.query_dynamic("select sys_context('USERENV','CURRENT_SCHEMA') from dual".into()).await.unwrap()[0][0].clone().unwrap();
        let hits = s.search_routines(None, "db_", 50).await.unwrap();
        let kinds: Vec<(String, ObjectKind)> = hits.iter().filter(|o| o.schema == me).map(|o| (o.name.clone(), o.kind)).collect();
        for want in [("DB_FN_TOTAL", ObjectKind::Function), ("DB_PROC_LOG", ObjectKind::Procedure), ("DB_PKG_SALES", ObjectKind::Package)] {
            assert!(kinds.contains(&(want.0.to_string(), want.1)), "{driver}: {want:?} in {kinds:?}");
        }
        assert!(s.search_routines(Some(&me), "PKG_SAL", 5).await.unwrap().iter().any(|o| o.name == "DB_PKG_SALES"));
        // SYS packages used through public synonyms.
        assert!(s.search_routines(None, "dbms_outp", 20).await.unwrap().iter().any(|o| o.name == "DBMS_OUTPUT" && o.kind == ObjectKind::Package), "{driver}");
        let m = s.package_members(&me, "DB_PKG_SALES").await.unwrap();
        let m: Vec<(&str, ObjectKind)> = m.iter().map(|o| (o.name.as_str(), o.kind)).collect();
        assert_eq!(m, vec![("NET", ObjectKind::Function), ("REFRESH", ObjectKind::Procedure)], "{driver}: overloads once");
        let sys = s.package_members("SYS", "DBMS_OUTPUT").await.unwrap();
        assert!(sys.iter().any(|o| o.name == "PUT_LINE" && o.kind == ObjectKind::Procedure), "{driver}");
        for d in ["drop function db_fn_total", "drop procedure db_proc_log", "drop package db_pkg_sales"] {
            run(&s, d).await;
        }
    }

    /// A catalog query whose caller stops waiting (cancelled indexing) is
    /// stopped on the server, and the session answers the next query at once.
    async fn check_abandoned_catalog_query(driver: &str) {
        let Some(s) = live_session(driver, false).await else { return };
        let slow = "select count(*) from all_objects a, all_objects b, all_objects c".to_string();
        let r = tokio::time::timeout(std::time::Duration::from_secs(2), s.query_dynamic(slow)).await;
        assert!(r.is_err(), "the slow query should still be running");
        let t = std::time::Instant::now();
        let rows = tokio::time::timeout(std::time::Duration::from_secs(15), s.query_dynamic("select 'ok' from dual".into())).await.expect("session still busy");
        assert_eq!(rows.unwrap()[0][0].as_deref(), Some("ok"), "{driver}");
        assert!(t.elapsed() < std::time::Duration::from_secs(10), "{driver}: next query waited {:?}", t.elapsed());
        eprintln!("{driver}: next catalog query answered {:?} after abandoning a slow one", t.elapsed());
    }

    async fn check_driver(driver: &str) {
        check_abandoned_catalog_query(driver).await;
        check_routines(driver).await;
        let Some(s) = live(driver).await else {
            eprintln!("skipping: DATABRAIN_ORACLE_HOST not set");
            return;
        };
        let run = |sql: &'static str| {
            let s = &s;
            async move { s.execute(sql, ExecOptions::default()).await.unwrap().collect().await }
        };
        assert!(s.server_version().await.unwrap().contains("Oracle"));
        s.ping().await.unwrap();

        // Types: same mapping as the Instant Client driver.
        let r = run("select 1 as a, 'x' as b, date '2024-01-02' as c, 1.5 as d, cast(null as date) as e, \
                     timestamp '2024-01-02 03:04:05.123456 +02:00' as f, hextoraw('ff00') as g, to_clob('long text') as h, \
                     12345678901234567890 as i, cast(2.5 as binary_double) as j, cast(42 as number(10)) as k from dual").await.unwrap();
        let b = &r.batches[0];
        let f = |i: usize| databrain_connector_core::arrow::util::display::ArrayFormatter::try_new(b.column(i).as_ref(), &Default::default()).unwrap().value(0).to_string();
        let types: Vec<String> = b.schema().fields().iter().map(|f| f.data_type().to_string()).collect();
        assert_eq!(f(1), "x");
        assert_eq!(f(2), "2024-01-02T00:00:00");
        assert_eq!(f(3), "1.5");
        assert!(b.column(4).is_null(0));
        assert!(f(5).starts_with("2024-01-02T01:04:05.123456"), "{}", f(5));
        assert_eq!(f(6), "ff00");
        assert_eq!(f(7), "long text");
        assert_eq!(f(8), "12345678901234567890", "exact NUMBER kept as text");
        assert_eq!(f(9), "2.5");
        assert_eq!((f(10).as_str(), types[10].as_str()), ("42", "Int64"));
        assert_eq!(types[9], "Float64");

        // Errors carry the ORA code and position.
        let e = run("select * from no_such_table_xyz").await.unwrap_err();
        assert_eq!(e.code.as_deref(), Some("ORA-00942"), "{e:?}");
        assert!(e.position.is_some(), "{e:?}");

        // DDL/DML, many rows in several batches, the catalog.
        let _ = run("drop table kb_thin purge").await;
        run("create table kb_thin (id number(10) primary key, name varchar2(40), note varchar2(100))").await.unwrap();
        run("comment on table kb_thin is 'thin test'").await.unwrap();
        let ins = run("insert into kb_thin select level, 'n' || level, null from dual connect by level <= 25000").await.unwrap();
        assert_eq!(ins.summary.rows_affected, Some(25000));
        let all = s.execute("select * from kb_thin order by id", ExecOptions { batch_size: 1000, ..Default::default() }).await.unwrap().collect().await.unwrap();
        assert_eq!((all.num_rows(), all.batches.len()), (25000, 25));
        let me = s.list_schemas().await.unwrap().into_iter().find(|x| x.is_default).unwrap().name;
        let m = s.bulk_metadata(std::slice::from_ref(&me)).await.unwrap();
        let o = m[0].objects.iter().find(|o| o.name == "KB_THIN").unwrap();
        assert_eq!(o.comment.as_deref(), Some("thin test"));
        let t = m[0].columns.iter().find(|t| t.table == "KB_THIN").unwrap();
        assert!(t.columns[0].is_primary_key && t.columns[1].data_type == "VARCHAR2(40)");
        let d = s.describe(&me, "KB_THIN").await.unwrap();
        assert!(d.ddl.unwrap_or_default().contains("CREATE TABLE"));
        let f0 = s.schema_fingerprints().await.unwrap().unwrap()[&me].clone();
        tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
        run("alter table kb_thin add (extra number)").await.unwrap();
        assert_ne!(f0, s.schema_fingerprints().await.unwrap().unwrap()[&me]);

        // Cancel a long query; the session stays usable.
        let cancel = databrain_connector_core::CancellationToken::new();
        let st = s
            .execute("select count(*) from all_objects a, all_objects b, all_objects c", ExecOptions { cancel: cancel.clone(), ..Default::default() })
            .await
            .unwrap();
        let c2 = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(700)).await;
            c2.cancel();
        });
        let t0 = std::time::Instant::now();
        let e = st.collect().await.unwrap_err();
        assert_eq!(e.kind, ErrorKind::Cancelled, "{e:?}");
        assert!(t0.elapsed() < std::time::Duration::from_secs(10));
        assert_eq!(run("select 7 from dual").await.unwrap().num_rows(), 1);

        // Row cap: stops early.
        let capped = s.execute("select * from kb_thin", ExecOptions { max_rows: Some(10), ..Default::default() }).await.unwrap().collect().await.unwrap();
        assert_eq!(capped.num_rows(), 10);
        run("drop table kb_thin purge").await.unwrap();

        // Read-only connections: reads work, writes are refused by the server.
        let ro = live_with(driver, true).await.unwrap();
        assert_eq!(ro.execute("select 1 from dual", ExecOptions::default()).await.unwrap().collect().await.unwrap().num_rows(), 1);
        let e = ro.execute("create table kb_ro (x int)", ExecOptions::default()).await.unwrap().collect().await;
        let e2 = ro.execute("insert into kb_ro values (1)", ExecOptions::default()).await.unwrap().collect().await;
        // (The engine refuses writes on read-only connections before they reach
        // the driver; this checks the server-side guard of the thin driver.)
        if driver == "thin" {
            assert!(e.is_err() || e2.is_err(), "write refused on a read-only connection");
        }
        let _ = s.execute("drop table kb_ro purge", ExecOptions::default()).await.unwrap().collect().await;
    }

    /// Live: DATABRAIN_ORACLE_HOST, _USER, _PASSWORD, _SERVICE (e.g. gvenzl/oracle-free).
    #[tokio::test]
    async fn live_roundtrip() {
        // DATABRAIN_ORACLE_DRIVER=instant_client tests the thick driver (+ DATABRAIN_ORACLE_LIB_DIR).
        if let Ok(d) = std::env::var("DATABRAIN_ORACLE_LIB_DIR") {
            init_client(Some(&d));
        }
        let Some(s) = live(&std::env::var("DATABRAIN_ORACLE_DRIVER").unwrap_or_else(|_| "thin".into())).await else {
            eprintln!("skipping: DATABRAIN_ORACLE_HOST not set");
            return;
        };
        let r = s
            .execute("select 1 as a, 'x' as b, sysdate as c, 1.5 as d from dual;", ExecOptions::default())
            .await
            .unwrap()
            .collect()
            .await
            .unwrap();
        assert_eq!(r.num_rows(), 1);
        assert!(!s.list_schemas().await.unwrap().is_empty());
    }
}
