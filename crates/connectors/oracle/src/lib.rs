//! Oracle Database connector built on `oracle` (ODPI-C).
//!
//! Requires Oracle Instant Client at runtime (download from oracle.com and
//! set the `client_lib_dir` option or the platform library path). The driver
//! API is synchronous, so all calls run on blocking threads.

pub mod client;

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
                FieldSpec::new("client_lib_dir", "Instant Client directory").placeholder("/opt/oracle/instantclient_23_3"),
            ],
            note: Some("Requires Oracle Instant Client installed on this computer."),
        }
    }

    async fn connect(&self, cfg: &ConnectionConfig, creds: Arc<dyn CredentialSource>) -> Result<Box<dyn Session>> {
        let (user, password) = match creds.get().await? {
            Credential::Password { user, password } => (user, password.expose_secret().to_string()),
            _ => return Err(ConnectorError::config("Oracle requires user/password authentication")),
        };
        let cs = connect_string(cfg);
        let lib = cfg.opt("client_lib_dir").map(str::to_string);
        let read_only = cfg.read_only;
        let conn = blocking(move || {
            init_client(lib.as_deref());
            let mut c = Connection::connect(&user, &password, &cs).map_err(map_err)?;
            c.set_autocommit(true);
            if read_only {
                c.execute("set transaction read only", &[]).map_err(map_err)?;
                c.set_autocommit(false);
            }
            let _ = c.execute("alter session set nls_date_format = 'YYYY-MM-DD HH24:MI:SS'", &[]);
            Ok(c)
        })
        .await?;
        Ok(Box::new(OracleSession { conn: Arc::new(conn), lock: Arc::new(Mutex::new(())) }))
    }
}

pub struct OracleSession {
    conn: Arc<Connection>,
    /// Serializes statements on the connection.
    lock: Arc<Mutex<()>>,
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
    let trimmed = sql.trim_end();
    let plsql_like = {
        let kw = databrain_connector_core::sql::leading_keyword(trimmed);
        kw == "BEGIN" || kw == "DECLARE" || (kw == "CREATE" && trimmed.to_ascii_uppercase().contains(" END"))
    };
    let sql = if !plsql_like { trimmed.trim_end_matches(';') } else { trimmed };
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
    for row in rows {
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
        let (conn, lock) = (self.conn.clone(), self.lock.clone());
        blocking(move || {
            let _g = lock.lock().map_err(|_| ConnectorError::internal("lock poisoned"))?;
            f(&conn)
        })
        .await
    }

    async fn query_named(&self, sql: &'static str, params: Vec<(&'static str, Option<String>)>) -> Result<Vec<Vec<Option<String>>>> {
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
        self.with_conn(|c| {
            let (v, banner) = c.server_version().map_err(map_err)?;
            Ok(if banner.is_empty() { format!("Oracle {v}") } else { banner.lines().next().unwrap_or("").to_string() })
        })
        .await
    }

    async fn ping(&self) -> Result<()> {
        self.with_conn(|c| c.ping().map_err(map_err)).await
    }

    async fn execute(&self, sql: &str, opts: ExecOptions) -> Result<QueryStream> {
        let (tx, stream) = QueryStream::channel(4);
        let (conn, lock) = (self.conn.clone(), self.lock.clone());
        let sql = sql.to_string();
        let done = tokio_util_token();
        let (cancel, done2) = (opts.cancel.clone(), done.clone());
        let breaker = self.conn.clone();
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
            ObjectKind::Sequence => "SEQUENCE",
            _ => "TABLE",
        };
        let (s, n) = (schema.to_string(), name.to_string());
        let ddl = self
            .with_conn(move |c| {
                let r = c.query_row_as::<String>(
                    "select dbms_metadata.get_ddl(:1, :2, :3) from dual",
                    &[&obj_type, &n, &s],
                );
                Ok(r.ok().map(|d| d.trim().to_string()))
            })
            .await?
            .or_else(|| {
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

    /// Live: DATABRAIN_ORACLE_HOST, _USER, _PASSWORD, _SERVICE (e.g. gvenzl/oracle-free).
    #[tokio::test]
    async fn live_roundtrip() {
        let Ok(host) = std::env::var("DATABRAIN_ORACLE_HOST") else {
            eprintln!("skipping: DATABRAIN_ORACLE_HOST not set");
            return;
        };
        use databrain_auth::InlineCredentialSource;
        let user = std::env::var("DATABRAIN_ORACLE_USER").unwrap_or_else(|_| "system".into());
        let mut cfg = ConnectionConfig::new(ConnectorKind::Oracle, AuthMethod::Password { user: user.clone() });
        cfg.host = Some(host);
        cfg.options.insert("service_name".into(), std::env::var("DATABRAIN_ORACLE_SERVICE").unwrap_or_else(|_| "FREEPDB1".into()));
        if let Ok(d) = std::env::var("DATABRAIN_ORACLE_LIB_DIR") {
            cfg.options.insert("client_lib_dir".into(), d);
        }
        let creds = Arc::new(InlineCredentialSource::new(
            AuthMethod::Password { user },
            std::env::var("DATABRAIN_ORACLE_PASSWORD").ok().map(secrecy::SecretString::from),
        ));
        let s = OracleConnector.connect(&cfg, creds).await.unwrap();
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
