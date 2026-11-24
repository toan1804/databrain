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
        /// Every statement runs in a READ ONLY transaction of its own.
        read_only: bool,
    },
    Thin(Arc<agent::Agent>),
}

/// [`Session::insert_array`] on Instant Client: a batch the size of the
/// rows, each column bound as text sized for its longest value in
/// characters (rust-oracle's size; Instant Client sizes every row's slot
/// for it, which `ArrayInsert` keeps bounded). A failed call is rolled
/// back.
fn thick_insert_array(c: &Connection, sql: &str, columns: &[Vec<Option<String>>]) -> Result<u64> {
    let rows = columns.first().map_or(0, Vec::len);
    let run = || -> std::result::Result<u64, oracle::Error> {
        let mut batch = c.batch(sql, rows).build()?;
        for (i, col) in columns.iter().enumerate() {
            let longest = col.iter().map(|v| v.as_ref().map_or(0, |s| s.chars().count())).max().unwrap_or(0).max(1);
            batch.set_type(i + 1, &oracle::sql_type::OracleType::Varchar2(longest as u32))?;
        }
        for r in 0..rows {
            let values: Vec<&Option<String>> = columns.iter().map(|col| &col[r]).collect();
            let row: Vec<&dyn oracle::sql_type::ToSql> = values.iter().map(|v| *v as &dyn oracle::sql_type::ToSql).collect();
            batch.append_row(&row)?;
        }
        batch.execute()?;
        Ok(rows as u64)
    };
    // Without batch-error mode Oracle doesn't say which row failed (the
    // error's offset is in the SQL): the load names the call's rows.
    run().map_err(|e| {
        let _ = c.rollback();
        map_err(e)
    })
}

pub struct OracleSession {
    pub(crate) backend: Backend,
}

impl OracleSession {
    pub(crate) async fn with_conn<T: Send + 'static>(&self, f: impl FnOnce(&Connection) -> Result<T> + Send + 'static) -> Result<T> {
        let Backend::Thick { conn, lock, .. } = &self.backend else {
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
            // Running first, then the check: a caller that gives up in
            // between sees it running and breaks it, or it sees the caller gone.
            state.running.store(true, Ordering::SeqCst);
            if state.abandoned.load(Ordering::SeqCst) {
                state.running.store(false, Ordering::SeqCst);
                return Err(ConnectorError::cancelled());
            }
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

    /// Autocommit is on: each statement commits. A read-only connection keeps
    /// one READ ONLY transaction, which holds nothing to lose.
    fn transaction_state(&self) -> databrain_connector_core::TxState {
        databrain_connector_core::TxState::None
    }

    async fn server_version(&self) -> Result<String> {
        if let Backend::Thin(a) = &self.backend {
            for sql in ["select banner from v$version where rownum = 1", "select 'Oracle ' || version from product_component_version where rownum = 1"] {
                if let Ok(rows) = a.text(sql, &[]).await
                    && let Some(v) = rows.into_iter().next().and_then(|r| r.into_iter().next().flatten())
                {
                    return Ok(v);
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

    /// `ALTER USER … IDENTIFIED BY "new" REPLACE "current"` for the user
    /// signed in (`REPLACE` is what a profile's verify function needs). A
    /// password is a quoted identifier there: it can't hold `"` or a
    /// control character.
    async fn change_password(&self, current: &str, new: &str) -> Result<()> {
        if [new, current].iter().any(|p| p.contains('"') || p.chars().any(char::is_control)) {
            return Err(ConnectorError::config("An Oracle password can't contain a double quote (\") or a control character"));
        }
        let rows = self.query_named("select user from dual", vec![]).await?;
        let user =
            rows.first().and_then(|r| r.first().cloned().flatten()).ok_or_else(|| ConnectorError::query("the server didn't say which user is signed in"))?;
        let sql = format!("ALTER USER {} IDENTIFIED BY \"{new}\" REPLACE \"{current}\"", quote_ident(ConnectorKind::Oracle, &user));
        self.run_internal(&sql).await
    }

    async fn ping(&self) -> Result<()> {
        if let Backend::Thin(a) = &self.backend {
            return a.call(serde_json::json!({"op": "ping"})).await.map(|_| ());
        }
        self.with_conn(|c| c.ping().map_err(map_err)).await
    }

    /// Array DML: one prepared INSERT run with every row's values bound at
    /// once (a batch on Instant Client, `execmany` on the thin helper). The
    /// values are text as the session reads it (numbers with its decimal
    /// separator, see `ArrayInsert::next`); a failed call is rolled back, so
    /// it keeps none of its rows (rows before the failing one would
    /// otherwise stay in the transaction and commit with the next statement).
    async fn insert_array(
        &self,
        sql: &str,
        columns: Vec<Vec<Option<String>>>,
        _types: &[&str],
        cancel: databrain_connector_core::CancellationToken,
    ) -> Result<Option<u64>> {
        if columns.first().is_none_or(Vec::is_empty) {
            return Ok(Some(0));
        }
        if cancel.is_cancelled() {
            return Err(ConnectorError::cancelled());
        }
        match &self.backend {
            Backend::Thin(a) => {
                let req = serde_json::json!({"op": "execmany", "sql": sql, "columns": columns});
                // The helper reads requests up to 64 MB: a larger one is refused here, not sent.
                let size = serde_json::to_vec(&req).map_or(usize::MAX, |b| b.len());
                if size > agent::MAX_REQUEST {
                    return Err(ConnectorError::query(format!(
                        "An array INSERT of {} MB is larger than the Oracle driver takes ({} MB)",
                        size >> 20,
                        agent::MAX_REQUEST >> 20
                    )));
                }
                let reply = tokio::select! {
                    // Dropping the call asks the helper to cancel it.
                    r = a.call(req) => r,
                    _ = cancel.cancelled() => return Err(ConnectorError::cancelled()),
                };
                match reply {
                    Ok(j) => Ok(Some(j["rows_affected"].as_u64().unwrap_or(0))),
                    // A helper older than array binding: INSERTs instead.
                    Err(e) if e.message.contains("unknown op") => Ok(None),
                    Err(e) => Err(e),
                }
            }
            Backend::Thick { read_only: true, .. } => Err(ConnectorError::query("This connection is read-only")),
            Backend::Thick { .. } => {
                let sql = sql.to_string();
                tokio::select! {
                    // Dropping the call breaks the statement (see `with_conn`).
                    r = self.with_conn(move |c| thick_insert_array(c, &sql, &columns)) => r.map(Some),
                    _ = cancel.cancelled() => Err(ConnectorError::cancelled()),
                }
            }
        }
    }

    fn insert_array_possible(&self) -> bool {
        !matches!(self.backend, Backend::Thick { read_only: true, .. })
    }

    async fn execute(&self, stmt: databrain_connector_core::Statement<'_>, opts: ExecOptions) -> Result<QueryStream> {
        let sql = stmt.sql();
        let (conn, lock, read_only) = match &self.backend {
            Backend::Thin(a) => return Ok(thin_execute(a.clone(), sql, opts)),
            Backend::Thick { conn, lock, read_only } => (conn.clone(), lock.clone(), *read_only),
        };
        let (tx, stream) = QueryStream::channel(4);
        let sql = sql.to_string();
        let done = tokio_util_token();
        let (cancel, done2) = (opts.cancel.clone(), done.clone());
        let breaker = conn.clone();
        // Stop breaks this statement only once it holds the session: while
        // it waits, the statement running is another one's.
        let state = Arc::new(CallState::default());
        let st = state.clone();
        tokio::spawn(async move {
            tokio::select! {
                _ = cancel.cancelled() => {
                    if st.running.load(Ordering::SeqCst) {
                        let _ = tokio::task::spawn_blocking(move || breaker.break_execution()).await;
                    }
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
            // Stopped while it waited for the session: it doesn't run.
            // Running is set first, so a Stop from now on breaks it.
            state.running.store(true, Ordering::SeqCst);
            if opts.cancel.is_cancelled() {
                tx.blocking_send(Err(ConnectorError::cancelled()));
                return;
            }
            // Read-only: a new READ ONLY transaction per statement, so each
            // reads the data as it is now (one transaction reads as of its
            // start, which was the connect), and a COMMIT run in it can't
            // leave the session writable.
            if read_only && let Err(e) = conn.rollback().and_then(|_| conn.execute("set transaction read only", &[]).map(|_| ())) {
                tx.blocking_send(Err(map_err(e)));
                return;
            }
            let r = run(&conn, &sql, &opts, &tx);
            // And ends with it: what a block ran after a COMMIT inside it
            // (no longer read-only), not committed, is rolled back now.
            if read_only {
                let _ = conn.rollback();
            }
            if let Err(e) = r {
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
        let mviews = crate::schema::mview_names(&rows, 0, 1);
        Ok(rows
            .into_iter()
            .filter(|r| !crate::schema::is_mview_table(r, 0, 1, &mviews))
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
                // Another object of the same name (a package and its body…): the first.
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
            // An INTERVAL table's count is the most it may have (1048575), not how many it has.
            if let Some(n) = p.first().and_then(|r| r[2].clone()).filter(|n| n != "1048575") {
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

    /// Oracle's describe reads no DDL; object_ddl builds it from the dictionary: never answered from a description.
    fn describe_ddl_is_object_ddl(&self) -> bool {
        false
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
        // No DDL here: Show DDL reads it (`object_ddl`), and building it
        // takes several dictionary reads the columns don't need.
        let ddl = None;
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
        let mviews = crate::schema::mview_names(&rows, 0, 1);
        for r in rows {
            if crate::schema::is_mview_table(&r, 0, 1, &mviews) {
                continue;
            }
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
        s.execute(databrain_connector_core::Statement::internal(sql), ExecOptions::default())
            .await
            .unwrap()
            .collect()
            .await
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
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
        // The source keeps the case it was written in (Oracle 23 doesn't upper-case it).
        assert!(s.object_ddl(&me, "DB_FN_TOTAL", ObjectKind::Function).await.unwrap().unwrap().to_uppercase().contains("FUNCTION"));
        assert!(s.object_ddl(&me, "DB_PROC_LOG", ObjectKind::Procedure).await.unwrap().unwrap().to_uppercase().contains("PROCEDURE"));
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

    /// The first value of `sql`'s result, run as a statement (`execute`).
    async fn cell(s: &OracleSession, sql: &'static str) -> String {
        let c = s.execute(databrain_connector_core::Statement::internal(sql), ExecOptions::default()).await.unwrap().collect().await.unwrap();
        databrain_connector_core::arrow::util::display::ArrayFormatter::try_new(c.batches[0].column(0).as_ref(), &Default::default())
            .unwrap()
            .value(0)
            .to_string()
    }

    /// The first value of `sql`'s first row, as text (a catalog query).
    async fn first(s: &OracleSession, sql: &str) -> String {
        s.query_dynamic(sql.to_string()).await.unwrap_or_else(|e| panic!("{sql}: {e}"))[0][0].clone().unwrap_or_default()
    }

    /// PL/SQL units whose `end;` starts a line, DBMS_OUTPUT, materialized
    /// views, RAW lengths, and a read-only session that sees new data
    /// without reconnecting.
    async fn check_plsql_catalog_and_read_only(driver: &str) {
        let s = live_session(driver, false).await.expect("live test: DATABRAIN_ORACLE_HOST not set (run with --ignored)");
        let exec = |sql: &'static str| {
            let s = &s;
            async move { s.execute(databrain_connector_core::Statement::internal(sql), ExecOptions::default()).await.unwrap().collect().await }
        };
        // `end;` at the start of a line keeps its `;`: the procedure compiles.
        exec("create or replace procedure kb_endline is\nbegin\n  null;\nend;").await.unwrap();
        assert_eq!(first(&s, "select status from user_objects where object_name = 'KB_ENDLINE'").await, "VALID", "{driver}");
        exec("create or replace type kb_pair as object (a number, b number);").await.unwrap();
        assert_eq!(first(&s, "select status from user_objects where object_name = 'KB_PAIR'").await, "VALID", "{driver}");
        // A view with CASE … END is SQL: its `;` goes.
        exec("create or replace view kb_case_v as select case when 1 = 1 then 'y' end as c from dual;").await.unwrap();

        // DBMS_OUTPUT of a block, as one notice; nothing after a plain query.
        let r = exec("begin dbms_output.put_line('hello'); dbms_output.put_line(''); dbms_output.put_line('world'); end;").await.unwrap();
        assert_eq!(r.notices, ["hello\n\nworld"], "{driver}");
        let r = exec("declare x number; begin for i in 1 .. 3000 loop dbms_output.put_line(rpad('x', 20, 'x') || i); end loop; end;").await.unwrap();
        assert_eq!(r.notices.len(), 1, "{driver}");
        assert_eq!(r.notices[0].lines().count(), 3000, "{driver}: lines over several reads");
        // The longest line DBMS_OUTPUT holds comes whole, and the next one after it.
        let r = exec("begin dbms_output.put_line(rpad('y', 32767, 'y')); dbms_output.put_line('next'); end;").await.unwrap();
        let lines: Vec<&str> = r.notices[0].lines().collect();
        assert!(
            lines.len() == 2 && lines[0].len() == 32767 && lines[0].bytes().all(|b| b == b'y') && lines[1] == "next",
            "{driver}: {:?}",
            lines.iter().map(|l| l.len()).collect::<Vec<_>>()
        );
        assert!(exec("select 1 from dual").await.unwrap().notices.is_empty(), "{driver}");
        // Printed before an error: still shown.
        let e = s
            .execute(
                databrain_connector_core::Statement::internal("begin dbms_output.put_line('before'); raise_application_error(-20001, 'boom'); end;"),
                ExecOptions::default(),
            )
            .await
            .unwrap();
        let mut notices = Vec::new();
        let mut failed = false;
        let mut e = e;
        while let Some(ev) = e.next().await {
            match ev {
                Ok(databrain_connector_core::StreamEvent::Notice(n)) => notices.push(n),
                Err(_) => failed = true,
                _ => {}
            }
        }
        assert!(failed && notices == ["before"], "{driver}: {notices:?}");

        // A materialized view lists as one (not as its table); RAW keeps its length.
        let _ = exec("drop materialized view kb_mv").await;
        let _ = exec("drop table kb_raw purge").await;
        exec("create table kb_raw (id raw(16), n number)").await.unwrap();
        exec("create materialized view kb_mv as select n from kb_raw").await.unwrap();
        let me = first(&s, "select sys_context('USERENV','CURRENT_SCHEMA') from dual").await;
        let objs = s.list_objects(&me).await.unwrap();
        let mv: Vec<ObjectKind> = objs.iter().filter(|o| o.name == "KB_MV").map(|o| o.kind).collect();
        assert_eq!(mv, [ObjectKind::MaterializedView], "{driver}");
        let versions = s.object_versions(&me).await.unwrap().unwrap();
        assert!(versions.contains_key(&databrain_connector_core::version_key(ObjectKind::MaterializedView, "KB_MV")), "{driver}");
        assert!(!versions.contains_key(&databrain_connector_core::version_key(ObjectKind::Table, "KB_MV")), "{driver}");
        let cols = s.columns(&me, Some("KB_RAW")).await.unwrap();
        assert_eq!(cols[0].1.data_type, "RAW(16)", "{driver}");
        let m = s.bulk_metadata(std::slice::from_ref(&me)).await.unwrap();
        let t = m[0].columns.iter().find(|t| t.table == "KB_RAW").unwrap();
        assert_eq!(t.columns[0].data_type, "RAW(16)", "{driver}");

        // Read-only: each statement sees the data as it is now, on the same session.
        let ro = live_session(driver, true).await.unwrap();
        const COUNT: &str = "select count(*) from kb_raw";
        const SID: &str = "select sys_context('USERENV','SID') from dual";
        let sid0 = cell(&ro, SID).await;
        // A read-only transaction can't read a table created seconds before
        // it (ORA-01466, the DDL time's granularity): wait for that.
        for _ in 0..20 {
            let r = ro.execute(databrain_connector_core::Statement::internal(COUNT), ExecOptions::default()).await.unwrap().collect().await;
            match r {
                Err(e) if e.code.as_deref() == Some("ORA-01466") => tokio::time::sleep(std::time::Duration::from_millis(500)).await,
                _ => break,
            }
        }
        assert_eq!(cell(&ro, COUNT).await, "0");
        exec("insert into kb_raw values (hextoraw('ff'), 1)").await.unwrap();
        assert_eq!(cell(&ro, COUNT).await, "1", "{driver}: the read-only session sees the new row");
        assert_eq!(cell(&ro, SID).await, sid0, "{driver}: no reconnect between statements");
        // Still read-only after a COMMIT run on it.
        let _ = ro.execute(databrain_connector_core::Statement::internal("commit"), ExecOptions::default()).await.unwrap().collect().await;
        let w = ro
            .execute(databrain_connector_core::Statement::internal("insert into kb_raw values (null, 2)"), ExecOptions::default())
            .await
            .unwrap()
            .collect()
            .await;
        assert!(w.is_err(), "{driver}: write refused after COMMIT");

        // Oracle doesn't enforce read-only beyond the transaction: a COMMIT
        // or DDL ends it. These are run here past the classifier, which is
        // what refuses them on a read-only connection (checked first).
        let ro_run = |sql: &'static str| {
            let ro = &ro;
            async move { ro.execute(databrain_connector_core::Statement::internal(sql), ExecOptions::default()).await.unwrap().collect().await }
        };
        for sql in [
            "begin commit; update kb_raw set n = 3 where n = 1; end;",
            "begin commit; insert into kb_raw values (null, 4); commit; end;",
            "create table kb_ro_ddl (x number)",
        ] {
            let c = databrain_connector_core::sql::classify(sql, ConnectorKind::Oracle);
            assert!(!databrain_connector_core::sql::read_only_allows(sql, &c), "{driver}: the classifier refuses {sql}");
        }
        // A write after a COMMIT in a block, not committed: rolled back as the
        // statement ends, so it holds no lock (it held the row until the next statement).
        ro_run("begin commit; update kb_raw set n = 3 where n = 1; end;").await.unwrap();
        exec("select n from kb_raw where n = 1 for update nowait").await.unwrap_or_else(|e| panic!("{driver}: the row is still locked: {e}"));
        exec("rollback").await.unwrap();
        assert_eq!(first(&s, "select count(*) from kb_raw where n = 3").await, "0", "{driver}: not kept");
        // Committed inside the block, or DDL (which commits itself): the server keeps it.
        ro_run("begin commit; insert into kb_raw values (null, 4); commit; end;").await.unwrap();
        assert_eq!(first(&s, "select count(*) from kb_raw where n = 4").await, "1", "{driver}: not enforced by the server");
        ro_run("create table kb_ro_ddl (x number)").await.unwrap();
        exec("drop table kb_ro_ddl purge").await.unwrap();

        exec("drop materialized view kb_mv").await.unwrap();
        exec("drop table kb_raw purge").await.unwrap();
        exec("drop view kb_case_v").await.unwrap();
        exec("drop procedure kb_endline").await.unwrap();
        exec("drop type kb_pair").await.unwrap();
    }

    async fn check_driver(driver: &str) {
        check_abandoned_catalog_query(driver).await;
        check_routines(driver).await;
        check_plsql_catalog_and_read_only(driver).await;
        let Some(s) = live(driver).await else {
            panic!("live test: DATABRAIN_ORACLE_HOST not set (run with --ignored)");
        };
        let run = |sql: &'static str| {
            let s = &s;
            async move { s.execute(databrain_connector_core::Statement::internal(sql), ExecOptions::default()).await.unwrap().collect().await }
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

        // XMLTYPE table columns (stored as CLOB or binary XML): go-ora fails
        // on them ("invalid size for GetInt64"); the helper reads them as text.
        let _ = run("drop table kb_xml purge").await;
        run("create table kb_xml (id number, x sys.xmltype, y xmltype) xmltype column x store as clob xmltype column y store as binary xml").await.unwrap();
        run("insert into kb_xml values (1, xmltype('<a>1</a>'), xmltype('<b>2</b>'))").await.unwrap();
        run("insert into kb_xml values (2, null, null)").await.unwrap();
        let x = run("select id, x, y from kb_xml order by id").await.unwrap();
        let xb = &x.batches[0];
        let xf = |c: usize, r: usize| {
            databrain_connector_core::arrow::util::display::ArrayFormatter::try_new(xb.column(c).as_ref(), &Default::default()).unwrap().value(r).to_string()
        };
        assert_eq!((xf(1, 0).trim(), xf(2, 0).trim()), ("<a>1</a>", "<b>2</b>"), "{driver}");
        assert!(xb.column(1).is_null(1) && xb.column(2).is_null(1), "{driver}");
        // And the connection still works.
        assert_eq!(run("select count(*) from kb_xml").await.unwrap().num_rows(), 1);
        run("drop table kb_xml purge").await.unwrap();

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
        let all = s
            .execute(databrain_connector_core::Statement::internal("select * from kb_thin order by id"), ExecOptions { batch_size: 1000, ..Default::default() })
            .await
            .unwrap()
            .collect()
            .await
            .unwrap();
        assert_eq!((all.num_rows(), all.batches.len()), (25000, 25));
        let me = s.list_schemas().await.unwrap().into_iter().find(|x| x.is_default).unwrap().name;
        let m = s.bulk_metadata(std::slice::from_ref(&me)).await.unwrap();
        let o = m[0].objects.iter().find(|o| o.name == "KB_THIN").unwrap();
        assert_eq!(o.comment.as_deref(), Some("thin test"));
        let t = m[0].columns.iter().find(|t| t.table == "KB_THIN").unwrap();
        assert!(t.columns[0].is_primary_key && t.columns[1].data_type == "VARCHAR2(40)");
        let d = s.describe(&me, "KB_THIN").await.unwrap();
        assert!(d.ddl.is_none() && d.columns.len() >= 2);
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
        // A partitioned table's DDL: its first partition and how many more,
        // and it runs (an INTERVAL table needs a partition in it). Checks
        // come without a LONG on 12.2+.
        let _ = run("drop table kb_part purge").await;
        run("create table kb_part (id number, d date not null, constraint kb_part_ck check (id > 0)) \
             partition by range (d) interval (numtoyminterval(1, 'MONTH')) (partition p0 values less than (date '2020-01-01'))")
        .await
        .unwrap();
        run("insert into kb_part values (1, date '2024-05-01')").await.unwrap();
        run("insert into kb_part values (2, date '2024-07-01')").await.unwrap();
        let p = s.object_ddl(&me, "KB_PART", ObjectKind::Table).await.unwrap().unwrap();
        assert!(
            p.contains("PARTITION BY RANGE (\"D\") INTERVAL (NUMTOYMINTERVAL(1, 'MONTH'))") && p.contains("PARTITION \"P0\" VALUES LESS THAN (TO_DATE"),
            "{driver}: {p}"
        );
        assert!(p.contains("lists them all") && !p.contains("1048575") && p.contains("CHECK (id > 0)"), "{driver}: {p}");
        run("drop table kb_part purge").await.unwrap();
        let create = p.split(";\n").next().unwrap().to_string();
        s.execute(databrain_connector_core::Statement::internal(&create), ExecOptions::default())
            .await
            .unwrap()
            .collect()
            .await
            .unwrap_or_else(|e| panic!("{driver}: {e:?}\n{create}"));
        run("drop table kb_part purge").await.unwrap();
        let f0 = s.explorer_fingerprints().await.unwrap().unwrap()[&me].clone();
        tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
        run("alter table kb_thin add (extra number)").await.unwrap();
        assert_ne!(f0, s.explorer_fingerprints().await.unwrap().unwrap()[&me]);

        // Cancel a long query; the session stays usable.
        let cancel = databrain_connector_core::CancellationToken::new();
        let st = s
            .execute(
                databrain_connector_core::Statement::internal("select count(*) from all_objects a, all_objects b, all_objects c"),
                ExecOptions { cancel: cancel.clone(), ..Default::default() },
            )
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

        // Stop in the middle of a large fetch (frames of up to 8 MB, so the
        // Stop lands inside one): the session still answers, each time.
        for _ in 0..5 {
            let cancel = databrain_connector_core::CancellationToken::new();
            let mut st = s
                .execute(
                    databrain_connector_core::Statement::internal("select level n, rpad('x', 2000, 'x') s from dual connect by level <= 300000"),
                    ExecOptions { cancel: cancel.clone(), ..Default::default() },
                )
                .await
                .unwrap();
            while let Some(ev) = st.next().await {
                if matches!(ev, Ok(databrain_connector_core::StreamEvent::Batch(_))) {
                    cancel.cancel();
                }
                if ev.is_err() {
                    break;
                }
            }
            let ok = tokio::time::timeout(std::time::Duration::from_secs(20), run("select 7 from dual")).await;
            assert_eq!(ok.expect("{driver}: session hangs after Stop").unwrap().num_rows(), 1, "{driver}");
        }

        // Stopped while another statement holds the session: it never runs.
        let _ = run("drop table kb_wait purge").await;
        run("create table kb_wait (n number)").await.unwrap();
        let slow_cancel = databrain_connector_core::CancellationToken::new();
        let slow = s
            .execute(
                databrain_connector_core::Statement::internal("select count(*) from all_objects a, all_objects b, all_objects c"),
                ExecOptions { cancel: slow_cancel.clone(), ..Default::default() },
            )
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let wait_cancel = databrain_connector_core::CancellationToken::new();
        let waiting = s
            .execute(
                databrain_connector_core::Statement::internal("insert into kb_wait values (1)"),
                ExecOptions { cancel: wait_cancel.clone(), ..Default::default() },
            )
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        wait_cancel.cancel();
        // Stopping the waiting one doesn't stop the running one.
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        slow_cancel.cancel();
        assert_eq!(waiting.collect().await.unwrap_err().kind, ErrorKind::Cancelled, "{driver}");
        assert_eq!(slow.collect().await.unwrap_err().kind, ErrorKind::Cancelled, "{driver}");
        let other = live_session(driver, false).await.unwrap();
        assert_eq!(first(&other, "select count(*) from kb_wait").await, "0", "{driver}: the stopped insert didn't run");
        run("drop table kb_wait purge").await.unwrap();

        // Row cap: stops early.
        let capped = s
            .execute(databrain_connector_core::Statement::internal("select * from kb_thin"), ExecOptions { max_rows: Some(10), ..Default::default() })
            .await
            .unwrap()
            .collect()
            .await
            .unwrap();
        assert_eq!(capped.num_rows(), 10);
        run("drop table kb_thin purge").await.unwrap();

        // Read-only connections: reads work, writes are refused by the server.
        let ro = live_with(driver, true).await.unwrap();
        assert_eq!(
            ro.execute(databrain_connector_core::Statement::internal("select 1 from dual"), ExecOptions::default())
                .await
                .unwrap()
                .collect()
                .await
                .unwrap()
                .num_rows(),
            1
        );
        let e = ro.execute(databrain_connector_core::Statement::internal("create table kb_ro (x int)"), ExecOptions::default()).await.unwrap().collect().await;
        let e2 =
            ro.execute(databrain_connector_core::Statement::internal("insert into kb_ro values (1)"), ExecOptions::default()).await.unwrap().collect().await;
        // (The engine refuses writes on read-only connections before they reach
        // the driver; this checks the server-side guard of the thin driver.)
        if driver == "thin" {
            assert!(e.is_err() || e2.is_err(), "write refused on a read-only connection");
        }
        let _ = s.execute(databrain_connector_core::Statement::internal("drop table kb_ro purge"), ExecOptions::default()).await.unwrap().collect().await;
    }

    /// Live: an array INSERT on each driver (DATABRAIN_ORACLE_DRIVER: thin
    /// or instant_client, with DATABRAIN_ORACLE_LIB_DIR). In a session whose
    /// decimal separator is `,`, numbers written with it (as the load writes
    /// them) go into a NUMBER and a VARCHAR2 column as literals would, the
    /// session's settings stay as they were, every type goes in through its
    /// format, and a failed call keeps none of its rows.
    #[ignore = "live: needs DATABRAIN_ORACLE_HOST (cargo test -- --ignored)"]
    #[tokio::test]
    async fn live_array_insert() {
        use databrain_connector_core::{CancellationToken, Statement};
        if let Ok(d) = std::env::var("DATABRAIN_ORACLE_LIB_DIR") {
            init_client(Some(&d)).unwrap();
        }
        let driver = std::env::var("DATABRAIN_ORACLE_DRIVER").unwrap_or_else(|_| "thin".into());
        let Some(s) = live(&driver).await else {
            panic!("live test: DATABRAIN_ORACLE_HOST not set (run with --ignored)");
        };
        let s = &s;
        let run = |sql: &'static str| async move { databrain_connector_core::query_text(s.as_ref(), sql).await };
        let _ = run("drop table kb_arr purge").await;
        run("create table kb_arr (n number(12,3), s varchar2(50), d date, ts timestamp(6), tz timestamp(6) with time zone, b raw(10), f binary_double, v varchar2(20))")
            .await
            .unwrap();
        // A German session: ',' is its decimal separator.
        s.execute(Statement::internal("alter session set nls_numeric_characters = ',.'"), ExecOptions::default()).await.unwrap().collect().await.unwrap();
        let sql = "INSERT INTO kb_arr (n, s, d, ts, tz, b, f, v) VALUES (:1, :2, TO_DATE(:3, 'YYYY-MM-DD'), \
                   TO_TIMESTAMP(:4, 'YYYY-MM-DD HH24:MI:SS.FF'), TO_TIMESTAMP_TZ(:5, 'YYYY-MM-DD HH24:MI:SS.FF TZH:TZM'), HEXTORAW(:6), :7, :8)";
        let v = |x: &str| Some(x.to_string());
        let columns = vec![
            vec![v("1234,5"), None, v("-0,125")],
            vec![v("ünï 'q'"), None, v("x")],
            vec![v("2026-11-17"), None, v("2000-02-29")],
            vec![v("2026-11-17 10:20:30.123456"), None, v("2026-01-01 00:00:00")],
            vec![v("2026-11-17 10:20:30 +02:00"), None, v("2026-11-17 23:59:59.5 -05:30")],
            vec![v("00ff"), None, v("41")],
            vec![v("1e-7"), None, v("2,5")],
            // A number into a VARCHAR2 column, as the load writes it.
            vec![v("1234,5"), None, v("-0,125")],
        ];
        assert_eq!(s.insert_array(sql, columns, &[], CancellationToken::new()).await.unwrap(), Some(3));
        let rows = run("select to_char(n, 'FM999999990.000', 'NLS_NUMERIC_CHARACTERS=''.,'''), s, to_char(d, 'YYYY-MM-DD HH24:MI:SS'), \
             to_char(ts, 'YYYY-MM-DD HH24:MI:SS.FF6'), to_char(tz, 'YYYY-MM-DD HH24:MI:SS.FF1 TZH:TZM'), rawtohex(b), \
             to_char(f, 'FM0.0000000EEEE', 'NLS_NUMERIC_CHARACTERS=''.,'''), v from kb_arr order by n nulls last")
        .await
        .unwrap();
        let t = |r: usize, c: usize| rows[r][c].clone();
        assert_eq!(t(0, 0).as_deref(), Some("-0.125"));
        assert_eq!(t(1, 0).as_deref(), Some("1234.500"), "read with the session's ','");
        assert_eq!(t(1, 1).as_deref(), Some("ünï 'q'"));
        assert_eq!(t(1, 2).as_deref(), Some("2026-11-17 00:00:00"));
        assert_eq!(t(1, 3).as_deref(), Some("2026-11-17 10:20:30.123456"));
        assert_eq!(t(1, 4).as_deref(), Some("2026-11-17 10:20:30.0 +02:00"));
        assert_eq!(t(0, 4).as_deref(), Some("2026-11-17 23:59:59.5 -05:30"));
        assert_eq!(t(1, 5).as_deref(), Some("00FF"));
        assert_eq!(t(1, 6).as_deref(), Some("1.0000000E-07"));
        assert_eq!(t(0, 3).as_deref(), Some("2026-01-01 00:00:00.000000"), "a time without a fraction");
        assert!(rows[2].iter().all(Option::is_none), "the NULL row: {:?}", rows[2]);
        // Into VARCHAR2: the text a literal 1234.5 becomes in this session.
        let literal = run("select to_char(1234.5) from dual").await.unwrap();
        assert_eq!(t(1, 7), literal[0][0]);
        assert_eq!(t(1, 7).as_deref(), Some("1234,5"));
        // The session's settings weren't touched.
        let nls = run("select value from nls_session_parameters where parameter = 'NLS_NUMERIC_CHARACTERS'").await.unwrap();
        assert_eq!(nls[0][0].as_deref(), Some(",."));
        // A call that fails at its second row keeps none of its rows, also
        // after the next statement commits.
        let mut bad = vec![vec![None, None]; 8];
        bad[0] = vec![v("7"), v("not a number")];
        let e = s.insert_array(sql, bad, &[], CancellationToken::new()).await.unwrap_err();
        assert!(e.message.contains("ORA-01722") || e.message.to_ascii_lowercase().contains("invalid number"), "{}", e.message);
        s.execute(Statement::internal("commit"), ExecOptions::default()).await.unwrap().collect().await.unwrap();
        assert_eq!(run("select count(*) from kb_arr").await.unwrap()[0][0].as_deref(), Some("3"), "the failed call's first row isn't kept");
        run("drop table kb_arr purge").await.unwrap();
    }

    /// Live: a user changes its own password (`REPLACE` the current one):
    /// the new one signs in, the old one no longer does. Needs a user that
    /// may create users (DATABRAIN_ORACLE_USER, e.g. system).
    #[ignore = "live: needs DATABRAIN_ORACLE_HOST (cargo test -- --ignored)"]
    #[tokio::test]
    async fn live_change_password() {
        let driver = std::env::var("DATABRAIN_ORACLE_DRIVER").unwrap_or_else(|_| "thin".into());
        let Some(admin) = live_session(&driver, false).await else {
            panic!("live test: DATABRAIN_ORACLE_HOST not set (run with --ignored)");
        };
        let connect = |pw: &'static str| {
            let driver = driver.clone();
            async move {
                use databrain_auth::InlineCredentialSource;
                let mut cfg = ConnectionConfig::new(ConnectorKind::Oracle, AuthMethod::Password { user: "kb_pw".into() });
                cfg.host = std::env::var("DATABRAIN_ORACLE_HOST").ok();
                cfg.port = std::env::var("DATABRAIN_ORACLE_PORT").ok().and_then(|p| p.parse().ok());
                cfg.options.insert("service_name".into(), std::env::var("DATABRAIN_ORACLE_SERVICE").unwrap_or_else(|_| "FREEPDB1".into()));
                cfg.options.insert("driver".into(), driver);
                open(&cfg, Arc::new(InlineCredentialSource::new(AuthMethod::Password { user: "kb_pw".into() }, Some(secrecy::SecretString::from(pw))))).await
            }
        };
        let _ = admin.run_internal("drop user kb_pw").await;
        admin.run_internal("create user kb_pw identified by \"Old_pw_1\"").await.unwrap();
        admin.run_internal("grant create session to kb_pw").await.unwrap();
        let s = connect("Old_pw_1").await.unwrap();
        assert!(s.change_password("Old_pw_1", "a\"b").await.is_err(), "a double quote is refused");
        s.change_password("Old_pw_1", "New'pw_2").await.unwrap();
        drop(s);
        assert!(connect("New'pw_2").await.is_ok(), "the new password signs in");
        assert!(connect("Old_pw_1").await.is_err(), "the old one no longer does");
        // Its sessions may take a moment to end (ORA-01940).
        let mut tries = 0;
        while let Err(e) = admin.run_internal("drop user kb_pw").await {
            tries += 1;
            assert!(tries < 20, "{e:?}");
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
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
        let r = s
            .execute(databrain_connector_core::Statement::internal("select 1 as a, 'x' as b, sysdate as c, 1.5 as d from dual;"), ExecOptions::default())
            .await
            .unwrap()
            .collect()
            .await
            .unwrap();
        assert_eq!(r.num_rows(), 1);
        assert!(!s.list_schemas().await.unwrap().is_empty());
    }
}
