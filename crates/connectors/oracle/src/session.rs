//! The Oracle session: connection handling, query execution and schema introspection.

use crate::agent;
use crate::error::map_err;
use crate::exec::BreakOnDrop;
use crate::exec::CallState;
use crate::exec::blocking;
use crate::exec::run;
use crate::exec::thin_execute;
use crate::exec::tokio_util_token;
use crate::schema::METADATA_FETCH;
use crate::schema::kind_of;
use async_trait::async_trait;
use databrain_connector_core::ColumnInfo;
use databrain_connector_core::ConnectorError;
use databrain_connector_core::ConnectorKind;
use databrain_connector_core::DbObject;
use databrain_connector_core::ExecOptions;
use databrain_connector_core::ObjectDetail;
use databrain_connector_core::ObjectKind;
use databrain_connector_core::QueryStream;
use databrain_connector_core::Result;
use databrain_connector_core::SchemaInfo;
use databrain_connector_core::Session;
use databrain_connector_core::TableColumns;
use databrain_connector_core::quote_ident;
use oracle::Connection;
use oracle::sql_type::ToSql;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::Ordering;

pub(crate) enum Backend {
    Thick {
        conn: Arc<Connection>,
        /// Serializes statements on the connection.
        lock: Arc<Mutex<()>>,
    },
    Thin(Arc<agent::Agent>),
}

pub struct OracleSession {
    pub(crate) backend: Backend,
}

impl OracleSession {
    pub(crate) async fn with_conn<T: Send + 'static>(&self, f: impl FnOnce(&Connection) -> Result<T> + Send + 'static) -> Result<T> {
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

    pub(crate) async fn query_named(&self, sql: &'static str, params: Vec<(&'static str, Option<String>)>) -> Result<Vec<Vec<Option<String>>>> {
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
                out.push(r.sql_values().iter().map(|v| if v.is_null().unwrap_or(true) { None } else { v.get::<String>().ok() }).collect());
            }
            Ok(out)
        })
        .await
    }

    /// A catalog query built at runtime (owner lists), rows as text.
    pub(crate) async fn query_dynamic(&self, sql: String) -> Result<Vec<Vec<Option<String>>>> {
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
        use databrain_connector_core::{TableLayout, group_indexes, query_text, quote_literal, truthy};
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
        // From the optimizer statistics (no DBA views needed): rows × average row length.
        if let Ok(r) = query_text(self, &format!("select num_rows, num_rows * avg_row_len from all_tables where owner = {s} and table_name = {t}")).await {
            l.row_estimate = r.first().and_then(|r| r[0].as_deref()).and_then(|x| x.parse().ok());
            l.size_bytes = r.first().and_then(|r| r.get(1).cloned().flatten()).and_then(|x| x.parse().ok());
        }
        Ok(l)
    }

    /// Built from the ALL_* views (see [`ddl`]): works for every object the
    /// user can see, with a few small queries. DBMS_METADATA only when
    /// that gives nothing (e.g. a wrapped or invisible source).
    async fn object_ddl(&self, schema: &str, name: &str, kind: ObjectKind) -> Result<Option<String>> {
        let kind = self.object_kind(schema, name).await.unwrap_or(None).unwrap_or(kind);
        let built = self.dictionary_ddl(schema, name, kind).await;
        match built {
            Ok(Some(d)) => Ok(Some(d)),
            other => match self.metadata_ddl(schema, name, kind).await {
                Some(d) => Ok(Some(d)),
                None => match other {
                    Err(e) => Err(e),
                    Ok(_) => Err(ConnectorError::query(format!(
                        "no definition of {schema}.{name} is visible to this user (it may not exist, or its source needs more privileges)"
                    ))),
                },
            },
        }
    }

    async fn describe(&self, schema: &str, name: &str) -> Result<ObjectDetail> {
        let found = self
            .query_named(
                "select o.object_type, c.comments, to_char(t.num_rows) from all_objects o \
                 left join all_tab_comments c on c.owner = o.owner and c.table_name = o.object_name \
                 left join all_tables t on t.owner = o.owner and t.table_name = o.object_name \
                 where o.owner = :o and o.object_name = :n \
                   and o.object_type in ('TABLE','VIEW','MATERIALIZED VIEW','PROCEDURE','FUNCTION','PACKAGE','SEQUENCE') \
                 order by decode(o.object_type, 'MATERIALIZED VIEW', 0, 'TABLE', 1, 'VIEW', 2, 3)",
                vec![("o", Some(schema.to_string())), ("n", Some(name.to_string()))],
            )
            .await?;
        let r = found.first().ok_or_else(|| ConnectorError::query(format!("object not found: {schema}.{name}")))?;
        let g = |i: usize| r.get(i).cloned().flatten();
        let kind = g(0).as_deref().map(kind_of).unwrap_or(ObjectKind::Table);
        let object = DbObject { schema: schema.to_string(), name: name.to_string(), kind, comment: g(1), row_estimate: g(2).and_then(|n| n.parse().ok()) };
        let columns: Vec<ColumnInfo> = self.columns(schema, Some(name)).await?.into_iter().map(|(_, c)| c).collect();
        let ddl = self.dictionary_ddl(schema, name, kind).await.ok().flatten().or_else(|| {
            (kind == ObjectKind::Table).then(|| {
                let cols: Vec<String> = columns
                    .iter()
                    .map(|c| format!("    {} {}{}", quote_ident(ConnectorKind::Oracle, &c.name), c.data_type, if c.nullable { "" } else { " NOT NULL" }))
                    .collect();
                format!("CREATE TABLE {}.{} (\n{}\n);", quote_ident(ConnectorKind::Oracle, schema), quote_ident(ConnectorKind::Oracle, name), cols.join(",\n"))
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

    /// Every object type the explorer lists: count + hash of name and DDL time.
    async fn explorer_fingerprints(&self) -> Result<Option<std::collections::HashMap<String, String>>> {
        let rows = self
            .query_named(
                "select owner, count(*) || '/' || sum(ora_hash(object_type || ':' || object_name || ':' || to_char(last_ddl_time, 'YYYYMMDDHH24MISS'))) \
                 from all_objects where object_type in ('TABLE','VIEW','MATERIALIZED VIEW','PROCEDURE','FUNCTION','PACKAGE','SEQUENCE') \
                   and object_name not like 'BIN$%' group by owner",
                vec![],
            )
            .await?;
        Ok(Some(rows.into_iter().filter_map(|r| Some((r.first().cloned().flatten()?, r.get(1).cloned().flatten().unwrap_or_default()))).collect()))
    }

    /// last_ddl_time per object (same kinds and de-duplication as `list_objects`).
    async fn object_versions(&self, schema: &str) -> Result<Option<std::collections::HashMap<String, String>>> {
        let rows = self
            .query_named(
                "select object_name, object_type, to_char(last_ddl_time, 'YYYYMMDDHH24MISS') from all_objects \
                 where owner = :owner and object_type in ('TABLE','VIEW','MATERIALIZED VIEW','PROCEDURE','FUNCTION','PACKAGE','SEQUENCE') \
                   and object_name not like 'BIN$%' \
                 order by case object_type when 'TABLE' then 0 when 'VIEW' then 1 when 'MATERIALIZED VIEW' then 1 else 2 end, object_name",
                vec![("owner", Some(schema.to_string()))],
            )
            .await?;
        let mut seen = std::collections::HashSet::new();
        let mut out = std::collections::HashMap::new();
        for r in rows {
            let g = |i: usize| r.get(i).cloned().flatten();
            let (Some(name), Some(t)) = (g(0), g(1)) else { continue };
            if !seen.insert(name.clone()) {
                continue;
            }
            let kind = match t.as_str() {
                "TABLE" => ObjectKind::Table,
                "VIEW" => ObjectKind::View,
                "MATERIALIZED VIEW" => ObjectKind::MaterializedView,
                "PROCEDURE" => ObjectKind::Procedure,
                "PACKAGE" => ObjectKind::Package,
                "SEQUENCE" => ObjectKind::Sequence,
                _ => ObjectKind::Function,
            };
            out.insert(databrain_connector_core::version_key(kind, &name), g(2).unwrap_or_default());
        }
        Ok(Some(out))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::init_client;
    use crate::config::open;
    use databrain_auth::AuthMethod;
    use databrain_connector_core::ConnectionConfig;
    use databrain_connector_core::ErrorKind;

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
        let creds = Arc::new(InlineCredentialSource::new(
            AuthMethod::Password { user },
            std::env::var("DATABRAIN_ORACLE_PASSWORD").ok().map(secrecy::SecretString::from),
        ));
        Some(open(&cfg, creds).await.unwrap())
    }

    /// Live, thin driver (needs a built helper: `node scripts/build-oracle-agent.mjs --host`).
    #[ignore = "live: needs DATABRAIN_ORACLE_HOST (cargo test -- --ignored)"]
    #[tokio::test]
    async fn live_thin_driver() {
        check_driver("thin").await;
    }

    /// Live, Instant Client driver: same checks (DATABRAIN_ORACLE_LIB_DIR).
    #[ignore = "live: needs DATABRAIN_ORACLE_HOST and DATABRAIN_ORACLE_LIB_DIR (cargo test -- --ignored)"]
    #[tokio::test]
    async fn live_instant_client_driver() {
        let Ok(d) = std::env::var("DATABRAIN_ORACLE_LIB_DIR") else {
            panic!("live test: DATABRAIN_ORACLE_LIB_DIR not set (run with --ignored)");
        };
        init_client(Some(&d)).unwrap();
        check_driver("instant_client").await;
    }

    async fn run(s: &OracleSession, sql: &str) {
        s.execute(sql, ExecOptions::default()).await.unwrap().collect().await.unwrap_or_else(|e| panic!("{sql}: {e}"));
    }

    /// Routine completion: standalone functions/procedures, packages and
    /// their members (functions vs procedures), other owners' packages.
    async fn check_routines(driver: &str) {
        let s = live_session(driver, false).await.expect("live test: DATABRAIN_ORACLE_HOST not set (run with --ignored)");
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
        // Show DDL: package (spec + body), function, procedure.
        run(
            &s,
            "create or replace package body db_pkg_sales as function net(x number) return number is begin return x; end; \
                 function net(x number, y number) return number is begin return x - y; end; procedure refresh is begin null; end; end;",
        )
        .await;
        let p = s.object_ddl(&me, "DB_PKG_SALES", ObjectKind::Package).await.unwrap().unwrap();
        let up = p.to_uppercase();
        assert!(up.contains("CREATE OR REPLACE PACKAGE DB_PKG_SALES") && up.contains("CREATE OR REPLACE PACKAGE BODY DB_PKG_SALES"), "{driver}: {p}");
        assert!(s.object_ddl(&me, "DB_FN_TOTAL", ObjectKind::Function).await.unwrap().unwrap().contains("FUNCTION"));
        assert!(s.object_ddl(&me, "DB_PROC_LOG", ObjectKind::Procedure).await.unwrap().unwrap().contains("PROCEDURE"));
        let sys = s.package_members("SYS", "DBMS_OUTPUT").await.unwrap();
        assert!(sys.iter().any(|o| o.name == "PUT_LINE" && o.kind == ObjectKind::Procedure), "{driver}");
        for d in ["drop function db_fn_total", "drop procedure db_proc_log", "drop package db_pkg_sales"] {
            run(&s, d).await;
        }
    }

    /// A catalog query whose caller stops waiting (cancelled indexing) is
    /// stopped on the server, and the session answers the next query at once.
    async fn check_abandoned_catalog_query(driver: &str) {
        let s = live_session(driver, false).await.expect("live test: DATABRAIN_ORACLE_HOST not set (run with --ignored)");
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
            panic!("live test: DATABRAIN_ORACLE_HOST not set (run with --ignored)");
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
                     12345678901234567890 as i, cast(2.5 as binary_double) as j, cast(42 as number(10)) as k from dual")
        .await
        .unwrap();
        let b = &r.batches[0];
        let f = |i: usize| {
            databrain_connector_core::arrow::util::display::ArrayFormatter::try_new(b.column(i).as_ref(), &Default::default()).unwrap().value(0).to_string()
        };
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
        let all =
            s.execute("select * from kb_thin order by id", ExecOptions { batch_size: 1000, ..Default::default() }).await.unwrap().collect().await.unwrap();
        assert_eq!((all.num_rows(), all.batches.len()), (25000, 25));
        let me = s.list_schemas().await.unwrap().into_iter().find(|x| x.is_default).unwrap().name;
        let m = s.bulk_metadata(std::slice::from_ref(&me)).await.unwrap();
        let o = m[0].objects.iter().find(|o| o.name == "KB_THIN").unwrap();
        assert_eq!(o.comment.as_deref(), Some("thin test"));
        let t = m[0].columns.iter().find(|t| t.table == "KB_THIN").unwrap();
        assert!(t.columns[0].is_primary_key && t.columns[1].data_type == "VARCHAR2(40)");
        let d = s.describe(&me, "KB_THIN").await.unwrap();
        assert!(d.ddl.unwrap_or_default().contains("CREATE TABLE"));
        // Show DDL from the dictionary: constraints, indexes, comments; views.
        run("create index kb_thin_name on kb_thin (upper(name), note desc)").await.unwrap();
        run("create or replace view kb_thin_v as select id, name from kb_thin where id > 10").await.unwrap();
        let t = s.object_ddl(&me, "KB_THIN", ObjectKind::Table).await.unwrap().unwrap();
        assert!(t.contains("PRIMARY KEY (\"ID\")") && t.contains("\"NAME\" VARCHAR2(40 BYTE)"), "{driver}: {t}");
        assert!(t.contains("CREATE INDEX") && t.contains("UPPER(\"NAME\")") && t.contains("\"NOTE\" DESC"), "{driver}: {t}");
        assert!(t.contains("COMMENT ON TABLE") && t.contains("'thin test'"), "{driver}: {t}");
        let v = s.object_ddl(&me, "KB_THIN_V", ObjectKind::View).await.unwrap().unwrap();
        assert!(v.starts_with("CREATE OR REPLACE VIEW") && v.contains("where id > 10"), "{driver}: {v}");
        run("drop view kb_thin_v").await.unwrap();
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
    #[ignore = "live: needs DATABRAIN_ORACLE_HOST (cargo test -- --ignored)"]
    #[tokio::test]
    async fn live_roundtrip() {
        // DATABRAIN_ORACLE_DRIVER=instant_client tests the thick driver (+ DATABRAIN_ORACLE_LIB_DIR).
        if let Ok(d) = std::env::var("DATABRAIN_ORACLE_LIB_DIR") {
            init_client(Some(&d)).unwrap();
        }
        let Some(s) = live(&std::env::var("DATABRAIN_ORACLE_DRIVER").unwrap_or_else(|_| "thin".into())).await else {
            panic!("live test: DATABRAIN_ORACLE_HOST not set (run with --ignored)");
        };
        let r = s.execute("select 1 as a, 'x' as b, sysdate as c, 1.5 as d from dual;", ExecOptions::default()).await.unwrap().collect().await.unwrap();
        assert_eq!(r.num_rows(), 1);
        assert!(!s.list_schemas().await.unwrap().is_empty());
    }
}
