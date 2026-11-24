use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use databrain_connector_core::external::ExternalTablesSlot;
use databrain_connector_core::{
    ColumnInfo, ConnectorError, ConnectorKind, DbObject, ExecOptions, ObjectDetail, ObjectKind, QueryStream, Result, SchemaInfo, Session, TableColumns,
    quote_literal,
};

use crate::database::FileConn;
use crate::error::{map_err, sandbox_hint};
use crate::exec::run;
use crate::outputs::RESULTS_SCHEMA_ID;

pub struct DuckSession {
    pub(crate) conn: Arc<Mutex<FileConn>>,
    pub(crate) interrupt: Arc<duckdb::InterruptHandle>,
    pub(crate) outputs: Option<Arc<ExternalTablesSlot>>,
    /// Runs in a sealed instance (see [`seal`](crate::sandbox::seal)).
    pub(crate) sandboxed: bool,
    /// The session's connection is to a database file other sessions share.
    pub(crate) shared: bool,
    /// Transactions, followed from the statements run: the driver's
    /// `is_autocommit()` always says yes.
    pub(crate) state: Arc<databrain_connector_core::conn_state::ConnState>,
    /// Which statement holds the connection (see [`Running`]).
    pub(crate) running: Arc<Running>,
}

/// Which statement holds the connection, so Stop interrupts only its own
/// statement: DuckDB's interrupt stops whatever runs on the connection, so a
/// Stop for a statement still waiting for it stopped the one running (and
/// the stopped one then ran), and a Stop whose watcher ran late could reach
/// the next statement.
#[derive(Default)]
pub(crate) struct Running {
    current: Mutex<u64>,
    next: std::sync::atomic::AtomicU64,
}

impl Running {
    /// A new statement's number (never 0).
    fn ticket(&self) -> u64 {
        self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1
    }

    /// Statement `id` holds the connection (called with the connection's
    /// lock held) until the returned guard drops.
    fn start(self: &Arc<Self>, id: u64) -> RunningGuard {
        *self.current.lock().unwrap_or_else(|e| e.into_inner()) = id;
        RunningGuard(self.clone(), id)
    }

    /// Run `f` (the interrupt) only while statement `id` holds the connection.
    fn interrupt_if(&self, id: u64, f: impl FnOnce()) {
        let current = self.current.lock().unwrap_or_else(|e| e.into_inner());
        if *current == id {
            f();
        }
    }
}

pub(crate) struct RunningGuard(Arc<Running>, u64);

impl Drop for RunningGuard {
    fn drop(&mut self) {
        let mut current = self.0.current.lock().unwrap_or_else(|e| e.into_inner());
        if *current == self.1 {
            *current = 0;
        }
    }
}

impl DuckSession {
    async fn with_conn<T: Send + 'static>(&self, f: impl FnOnce(&duckdb::Connection) -> Result<T> + Send + 'static) -> Result<T> {
        let c = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let g = c.lock().map_err(|_| ConnectorError::internal("lock poisoned"))?;
            f(&g)
        })
        .await
        .map_err(|e| ConnectorError::internal(e.to_string()))?
    }

    async fn strings(&self, sql: String, params: Vec<String>) -> Result<Vec<Vec<Option<String>>>> {
        self.with_conn(move |c| {
            let mut stmt = c.prepare(&sql).map_err(map_err)?;
            let p: Vec<&dyn duckdb::ToSql> = params.iter().map(|s| s as &dyn duckdb::ToSql).collect();
            let mut rows = stmt.query(p.as_slice()).map_err(map_err)?;
            let n = rows.as_ref().map(|s| s.column_count()).unwrap_or(0);
            let mut out = Vec::new();
            while let Some(r) = rows.next().map_err(map_err)? {
                let mut row = Vec::with_capacity(n);
                for i in 0..n {
                    let v: duckdb::types::Value = r.get(i).map_err(map_err)?;
                    row.push(match v {
                        duckdb::types::Value::Null => None,
                        duckdb::types::Value::Text(s) => Some(s),
                        duckdb::types::Value::Boolean(b) => Some(b.to_string()),
                        duckdb::types::Value::BigInt(i) => Some(i.to_string()),
                        duckdb::types::Value::Int(i) => Some(i.to_string()),
                        duckdb::types::Value::HugeInt(i) => Some(i.to_string()),
                        duckdb::types::Value::UBigInt(i) => Some(i.to_string()),
                        other => Some(format!("{other:?}")),
                    });
                }
                out.push(row);
            }
            Ok(out)
        })
        .await
    }
}

#[async_trait]
impl Session for DuckSession {
    fn kind(&self) -> ConnectorKind {
        ConnectorKind::Duckdb
    }

    /// Followed from the statements run (`BEGIN` … `COMMIT`/`ROLLBACK`):
    /// the driver's `is_autocommit()` is a stub that always says yes.
    fn transaction_state(&self) -> databrain_connector_core::TxState {
        self.state.tx_state()
    }

    async fn server_version(&self) -> Result<String> {
        let r = self.strings("select version()".into(), vec![]).await?;
        Ok(format!("DuckDB {}", r.first().and_then(|r| r[0].clone()).unwrap_or_default()))
    }

    async fn ping(&self) -> Result<()> {
        self.strings("select 1".into(), vec![]).await.map(|_| ())
    }

    /// The appender: the rows go into a temporary table of their own types
    /// (the appender converts nothing, so its types must match), then one
    /// `INSERT INTO table (columns) SELECT …` converts each value with
    /// DuckDB's casts, as an INSERT's literals are; the temporary table is
    /// dropped. In the load's transaction when it has one.
    async fn append_rows(
        &self,
        table: &str,
        columns: &[String],
        batch: databrain_connector_core::arrow::array::RecordBatch,
        cancel: databrain_connector_core::CancellationToken,
    ) -> Result<Option<u64>> {
        use databrain_connector_core::arrow::datatypes::{Field, Schema};
        if batch.num_rows() == 0 {
            return Ok(Some(0));
        }
        let n = batch.num_rows() as u64;
        // Columns `c0`, `c1`, …: the rows' own names may clash or need quoting.
        let normal = crate::outputs::normalize_for_duckdb(&batch)?;
        let renamed = Arc::new(Schema::new(
            normal.schema().fields().iter().enumerate().map(|(i, f)| Field::new(format!("c{i}"), f.data_type().clone(), true)).collect::<Vec<_>>(),
        ));
        let rows = databrain_connector_core::arrow::array::RecordBatch::try_new(renamed.clone(), normal.columns().to_vec())
            .map_err(|e| ConnectorError::internal(e.to_string()))?;
        let rows = crate::outputs::to_arrow58(&rows)?;
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let temp = format!("databrain_append_{}", NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
        let defs = renamed.fields().iter().map(|f| format!("{} {}", f.name(), crate::outputs::duck_type(f.data_type()))).collect::<Vec<_>>().join(", ");
        let target_cols = columns.iter().map(|c| databrain_connector_core::quote_ident(ConnectorKind::Duckdb, c)).collect::<Vec<_>>().join(", ");
        let source_cols = (0..columns.len()).map(|i| format!("c{i}")).collect::<Vec<_>>().join(", ");
        let insert = format!(
            "INSERT INTO {} ({target_cols}) SELECT {source_cols} FROM temp.main.{temp}",
            databrain_connector_core::quote_path(ConnectorKind::Duckdb, table)
        );
        let interrupt = self.interrupt.clone();
        let watch = cancel.clone();
        let stop = tokio::spawn(async move {
            watch.cancelled().await;
            interrupt.interrupt();
        });
        let r = self
            .with_conn(move |c| {
                c.execute_batch(&format!("CREATE TEMP TABLE {temp} ({defs})")).map_err(map_err)?;
                let run = || -> Result<()> {
                    {
                        let mut app = c.appender_to_catalog_and_db(&temp, "temp", "main").map_err(map_err)?;
                        app.append_record_batch(rows).map_err(map_err)?;
                        app.flush().map_err(map_err)?;
                    }
                    c.execute_batch(&insert).map_err(map_err)
                };
                let r = run();
                let _ = c.execute_batch(&format!("DROP TABLE IF EXISTS temp.main.{temp}"));
                r
            })
            .await;
        stop.abort();
        if cancel.is_cancelled() {
            return Err(ConnectorError::cancelled());
        }
        r.map(|()| Some(n))
    }

    fn append_rows_possible(&self) -> bool {
        true
    }

    /// The pairs DuckDB's casts convert differently from an INSERT's
    /// literal ([`appender_refused`]), from the table's column types.
    async fn append_rows_refused(
        &self,
        table: &str,
        columns: &[String],
        schema: &databrain_connector_core::arrow::datatypes::SchemaRef,
    ) -> Result<Option<String>> {
        let sql = format!("DESCRIBE SELECT * FROM {}", databrain_connector_core::quote_path(ConnectorKind::Duckdb, table));
        let types: Vec<(String, String)> =
            self.strings(sql, vec![]).await?.into_iter().filter_map(|r| Some((r.first()?.clone()?, r.get(1)?.clone()?))).collect();
        for (f, c) in schema.fields().iter().zip(columns) {
            // DuckDB's names are case-insensitive.
            let target = types.iter().find(|(n, _)| n == c).or_else(|| types.iter().find(|(n, _)| n.eq_ignore_ascii_case(c)));
            if let Some(why) = target.and_then(|(_, t)| appender_refused(f, t)) {
                return Ok(Some(format!("{c}: {why}")));
            }
        }
        Ok(None)
    }

    async fn execute(&self, stmt: databrain_connector_core::Statement<'_>, opts: ExecOptions) -> Result<QueryStream> {
        let sql = stmt.sql();
        let state = self.state.clone();
        let (tx, stream) = QueryStream::channel(4);
        let conn = self.conn.clone();
        let sql = sql.to_string();
        let (cancel, interrupt) = (opts.cancel.clone(), self.interrupt.clone());
        let outputs = self.outputs.clone();
        let sandboxed = self.sandboxed;
        let shared = self.shared;
        let done = tokio_util::sync::CancellationToken::new();
        let done_guard = done.clone();
        // duckdb-rs disconnects *before* clearing the interrupt handle's
        // pointer, so an `interrupt()` racing a drop of the last Connection
        // is a use-after-free. Holding a clone of `conn` keeps the connection
        // alive until this task ends; `biased` checks `done` first so a query
        // that already finished is never interrupted.
        let keepalive = conn.clone();
        // Interrupted only while this statement holds the connection.
        let (running, id) = (self.running.clone(), self.running.ticket());
        let watch = running.clone();
        tokio::spawn(async move {
            tokio::select! {
                biased;
                _ = done.cancelled() => {}
                _ = cancel.cancelled() => watch.interrupt_if(id, || interrupt.interrupt()),
            }
            drop(keepalive);
        });
        tokio::task::spawn_blocking(move || {
            let _done = done_guard.drop_guard();
            let Ok(g) = conn.lock() else {
                tx.blocking_send(Err(ConnectorError::internal("lock poisoned")));
                return;
            };
            // Ends (before the connection is let go) when the statement does.
            let _running = running.start(id);
            // Stopped while it waited for the connection: it doesn't run.
            if opts.cancel.is_cancelled() {
                tx.blocking_send(Err(ConnectorError::cancelled()));
                return;
            }
            // Followed as it runs (a statement stopped before it ran changes nothing).
            state.observe(&sql, ConnectorKind::Duckdb);
            if let Err(e) = run(&g, &sql, &opts, &tx, outputs.as_deref(), shared, sandboxed) {
                let e = if opts.cancel.is_cancelled() {
                    ConnectorError::cancelled()
                } else if sandboxed {
                    sandbox_hint(e)
                } else {
                    e
                };
                tx.blocking_send(Err(e));
            }
        });
        Ok(stream)
    }

    async fn list_schemas(&self) -> Result<Vec<SchemaInfo>> {
        let rows = self
            .strings(
                "select database_name, schema_name, (database_name = current_database() and schema_name = current_schema())::varchar \
                 from duckdb_schemas() \
                 where (not internal or (schema_name = 'main' and database_name not in ('system', 'temp'))) \
                   and schema_name <> 'pg_catalog' and database_name <> 'results' \
                 order by database_name = current_database() desc, database_name in ('files', 'attached_files') desc, 1, 2"
                    .into(),
                vec![],
            )
            .await?;
        Ok(rows
            .into_iter()
            .filter_map(|r| Some(SchemaInfo::in_catalog(r[0].clone()?, r[1].as_deref()?, r[2].as_deref() == Some("true"))))
            .chain((!self.output_catalog().is_empty()).then(|| SchemaInfo::in_catalog("results", "main", false)))
            .collect())
    }

    async fn list_catalogs(&self) -> Result<Option<Vec<databrain_connector_core::CatalogInfo>>> {
        let rows = self
            .strings(
                "select database_name, (database_name = current_database())::varchar from duckdb_databases() \
                 where not internal and database_name <> 'results' order by database_name = current_database() desc, 1"
                    .into(),
                vec![],
            )
            .await?;
        Ok(Some(
            rows.into_iter()
                .filter_map(|r| Some(databrain_connector_core::CatalogInfo { name: r[0].clone()?, is_default: r[1].as_deref() == Some("true") }))
                .chain((!self.output_catalog().is_empty()).then(|| databrain_connector_core::CatalogInfo { name: "results".into(), is_default: false }))
                .collect(),
        ))
    }

    /// Hash of every column definition and comment per schema (local
    /// catalogs: cheap). `results` outputs are left out (always re-read).
    /// Tables, columns, macros and sequences per schema.
    async fn explorer_fingerprints(&self) -> Result<Option<std::collections::HashMap<String, String>>> {
        let rows = self
            .strings(
                "select database_name || '.' || schema_name, \
                        md5(string_agg(table_name || ':' || column_name || ':' || data_type || ':' || coalesce(comment, ''), ',' order by table_name, column_index)) \
                 from duckdb_columns() where not internal and database_name <> 'results' group by 1"
                    .into(),
                vec![],
            )
            .await?;
        let mut map: std::collections::HashMap<String, String> = rows.into_iter().filter_map(|r| Some((r[0].clone()?, r[1].clone()?))).collect();
        // Macros and sequences, each on its own: one failing doesn't hide the other.
        let mut others = Vec::new();
        for sql in [
            "select database_name || '.' || schema_name, 'f' || md5(string_agg(function_name || ':' || coalesce(macro_definition, ''), ',' order by function_name)) \
             from duckdb_functions() where not internal and database_name <> 'results' group by 1",
            "select database_name || '.' || schema_name, 's' || md5(string_agg(sequence_name || ':' || cast(increment_by as varchar), ',' order by sequence_name)) \
             from duckdb_sequences() where database_name <> 'results' group by 1",
        ] {
            // Missing on an older DuckDB: that part is left out.
            if let Ok(rows) = self.strings(sql.into(), vec![]).await {
                others.extend(rows);
            }
        }
        for r in others {
            if let (Some(s), Some(f)) = (r[0].clone(), r[1].clone()) {
                let e = map.entry(s).or_default();
                e.push('|');
                e.push_str(&f);
            }
        }
        // Schemas without columns (an empty `main`) are unchanged too.
        for s in self.list_schemas().await? {
            if s.catalog.as_deref() != Some("results") {
                map.entry(s.name).or_default();
            }
        }
        Ok(Some(map))
    }

    async fn schema_object_counts(&self) -> Result<Option<std::collections::HashMap<String, usize>>> {
        let rows = self
            .strings(
                "select s, count(*)::varchar from (select database_name || '.' || schema_name s from duckdb_tables() where not internal \
                 union all select database_name || '.' || schema_name from duckdb_views() where not internal) group by 1"
                    .into(),
                vec![],
            )
            .await?;
        let mut counts: std::collections::HashMap<String, usize> = rows
            .into_iter()
            .filter_map(|r| Some((r[0].clone()?, r[1].as_deref()?.parse().ok()?)))
            .filter(|(s, _): &(String, usize)| !s.starts_with("results."))
            .collect();
        let n = self.output_catalog().len();
        if n > 0 {
            counts.insert(RESULTS_SCHEMA_ID.into(), n);
        }
        Ok(Some(counts))
    }

    async fn search_objects(&self, query: &str, limit: usize) -> Result<Vec<DbObject>> {
        let term = databrain_connector_core::search_sql_term(query);
        let rows = self
            .strings(
                "select database_name || '.' || schema_name, table_name, 'table', comment, estimated_size::varchar from duckdb_tables() \
                 where not internal and database_name <> 'results' and contains(lower(table_name), ?) \
                 union all select database_name || '.' || schema_name, view_name, 'view', comment, null from duckdb_views() \
                 where not internal and database_name <> 'results' and contains(lower(view_name), ?) limit 5000"
                    .into(),
                vec![term.clone(), term],
            )
            .await?;
        let mut hits: Vec<DbObject> = rows
            .into_iter()
            .filter_map(|r| {
                Some(DbObject {
                    schema: r[0].clone()?,
                    name: r[1].clone()?,
                    kind: if r[2].as_deref() == Some("view") { ObjectKind::View } else { ObjectKind::Table },
                    comment: r[3].clone().filter(|c| !c.is_empty()),
                    row_estimate: r[4].as_deref().and_then(|n| n.parse().ok()),
                })
            })
            .chain(self.output_objects())
            // `files.sales` names the `files` catalog's `main` too, as DuckDB reads it.
            .filter(|o| {
                databrain_connector_core::object_matches(query, &o.schema, &o.name)
                    || o.schema.strip_suffix(".main").is_some_and(|cat| databrain_connector_core::object_matches(query, cat, &o.name))
            })
            .collect();
        databrain_connector_core::rank_matches(query, &mut hits, limit);
        Ok(hits)
    }

    async fn list_objects(&self, schema: &str) -> Result<Vec<DbObject>> {
        if schema == RESULTS_SCHEMA_ID {
            return Ok(self.output_objects());
        }
        let (db, sch) = schema.split_once('.').ok_or_else(|| ConnectorError::query("expected database.schema"))?;
        let rows = self
            .strings(
                "select table_name, 'table', comment, estimated_size::varchar from duckdb_tables() where database_name = ? and schema_name = ? \
                 union all select view_name, 'view', comment, null from duckdb_views() where database_name = ? and schema_name = ? and not internal \
                 union all select sequence_name, 'sequence', comment, null from duckdb_sequences() where database_name = ? and schema_name = ? \
                 union all select distinct function_name, 'function', null, null from duckdb_functions() \
                   where database_name = ? and schema_name = ? and not internal and function_type in ('macro', 'table_macro') \
                 order by 1"
                    .into(),
                vec![db.into(), sch.into(), db.into(), sch.into(), db.into(), sch.into(), db.into(), sch.into()],
            )
            .await?;
        let mut out: Vec<DbObject> = rows
            .into_iter()
            .filter_map(|r| {
                Some(DbObject {
                    schema: schema.to_string(),
                    name: r[0].clone()?,
                    kind: match r[1].as_deref() {
                        Some("view") => ObjectKind::View,
                        Some("sequence") => ObjectKind::Sequence,
                        Some("function") => ObjectKind::Function,
                        _ => ObjectKind::Table,
                    },
                    comment: r[2].clone().filter(|c| !c.is_empty()),
                    row_estimate: r[3].as_deref().and_then(|n| n.parse().ok()),
                })
            })
            .collect();
        // Tables, views, sequences, functions; by name within each (stable sort).
        let rank = |k: ObjectKind| match k {
            ObjectKind::Table => 0,
            ObjectKind::View => 1,
            ObjectKind::Sequence => 2,
            _ => 3,
        };
        out.sort_by_key(|o| rank(o.kind));
        Ok(out)
    }

    async fn table_layout(&self, schema: &str, name: &str) -> Result<databrain_connector_core::TableLayout> {
        use databrain_connector_core::{IndexInfo, TableLayout, parse_list_text, query_text, truthy};
        if schema == RESULTS_SCHEMA_ID {
            return Ok(TableLayout::default());
        }
        let (db, sch) = schema.split_once('.').unwrap_or((schema, "main"));
        let w = format!("database_name = {} AND schema_name = {} AND table_name = {}", quote_literal(db), quote_literal(sch), quote_literal(name));
        let mut l = TableLayout::default();
        for r in query_text(
            self,
            &format!("SELECT constraint_type, constraint_column_names FROM duckdb_constraints() WHERE {w} AND constraint_type IN ('PRIMARY KEY', 'UNIQUE')"),
        )
        .await?
        {
            let primary = r[0].as_deref() == Some("PRIMARY KEY");
            let columns = r[1].as_deref().map(parse_list_text).unwrap_or_default();
            l.indexes.push(IndexInfo { name: r[0].clone().unwrap_or_default(), columns, unique: true, primary, method: None });
        }
        for r in query_text(self, &format!("SELECT index_name, expressions, is_unique, is_primary FROM duckdb_indexes() WHERE {w}")).await? {
            let columns = r[1].as_deref().map(parse_list_text).unwrap_or_default();
            l.indexes.push(IndexInfo {
                name: r[0].clone().unwrap_or_default(),
                columns,
                unique: truthy(&r[2]),
                primary: truthy(&r[3]),
                method: Some("art".into()),
            });
        }
        l.indexes.sort_by_key(|i| !i.primary);
        if let Ok(r) = query_text(self, &format!("SELECT estimated_size FROM duckdb_tables() WHERE {w}")).await {
            l.row_estimate = r.first().and_then(|r| r[0].as_deref()).and_then(|x| x.parse().ok());
        }
        Ok(l)
    }

    async fn describe(&self, schema: &str, name: &str) -> Result<ObjectDetail> {
        if schema == RESULTS_SCHEMA_ID {
            let object = self.output_objects().into_iter().find(|o| o.name == name).ok_or_else(|| ConnectorError::query(format!("output {name} not found")))?;
            let columns = self.output_columns().into_iter().find(|t| t.table == name).map(|t| t.columns).unwrap_or_default();
            return Ok(ObjectDetail { object, columns, ddl: None, foreign_keys: vec![] });
        }
        // The table or view by name with its DDL, in one query (not the
        // schema's every table, view, sequence and macro); another object
        // (a sequence, a macro) from the listing.
        let (db, sch) = schema.split_once('.').unwrap_or((schema, "main"));
        let row = self
            .strings(
                "select 'table', comment, estimated_size::varchar, sql from duckdb_tables() where database_name = ? and schema_name = ? and table_name = ? \
                 union all select 'view', comment, null, sql from duckdb_views() where database_name = ? and schema_name = ? and view_name = ? and not internal"
                    .into(),
                vec![db.into(), sch.into(), name.into(), db.into(), sch.into(), name.into()],
            )
            .await?
            .into_iter()
            .next();
        let (object, ddl) = match row {
            Some(r) => (
                DbObject {
                    schema: schema.to_string(),
                    name: name.to_string(),
                    kind: if r[0].as_deref() == Some("view") { ObjectKind::View } else { ObjectKind::Table },
                    comment: r[1].clone().filter(|c| !c.is_empty()),
                    row_estimate: r[2].as_deref().and_then(|n| n.parse().ok()),
                },
                r[3].clone(),
            ),
            None => (
                self.list_objects(schema)
                    .await?
                    .into_iter()
                    .find(|o| o.name == name)
                    .ok_or_else(|| ConnectorError::query(format!("object not found: {schema}.{name}")))?,
                None,
            ),
        };
        let columns = self.columns(schema, Some(name)).await?.into_iter().next().map(|t| t.columns).unwrap_or_default();
        Ok(ObjectDetail { object, columns, ddl, foreign_keys: vec![] })
    }

    async fn schema_columns(&self, schema: &str) -> Result<Vec<TableColumns>> {
        if schema == RESULTS_SCHEMA_ID {
            return Ok(self.output_columns());
        }
        self.columns(schema, None).await
    }
}

impl DuckSession {
    async fn columns(&self, schema: &str, table: Option<&str>) -> Result<Vec<TableColumns>> {
        let (db, sch) = schema.split_once('.').ok_or_else(|| ConnectorError::query("expected database.schema"))?;
        let rows = self
            .strings(
                "select table_name, column_name, data_type, is_nullable::varchar, comment from duckdb_columns() \
                 where database_name = ? and schema_name = ? and (? = '' or table_name = ?) order by table_name, column_index"
                    .into(),
                vec![db.into(), sch.into(), table.unwrap_or("").into(), table.unwrap_or("").into()],
            )
            .await?;
        let mut out: Vec<TableColumns> = Vec::new();
        for r in rows {
            let t = r[0].clone().unwrap_or_default();
            let col = ColumnInfo {
                name: r[1].clone().unwrap_or_default(),
                data_type: r[2].clone().unwrap_or_default(),
                nullable: r[3].as_deref() != Some("false"),
                is_primary_key: false,
                default: None,
                comment: r[4].clone().filter(|c| !c.is_empty()),
            };
            match out.last_mut() {
                Some(x) if x.table == t => x.columns.push(col),
                _ => out.push(TableColumns { table: t, columns: vec![col], foreign_keys: vec![] }),
            }
        }
        Ok(out)
    }
}

/// Why a value of Arrow type `source` doesn't go through the appender into
/// a column of DuckDB type `target`: the appender's cast would write
/// something other than the INSERT's literal. Floats are cast as binary
/// floats (a double `2.5` into an INTEGER rounds half to even, `1.005` into
/// DECIMAL(5,2) is `1.00`; a FLOAT `0.1` into a DOUBLE is `0.100000001…`),
/// where the literal's decimal text rounds half away from zero; an instant
/// with a time zone into anything but TIMESTAMPTZ is converted in the
/// session's time zone, where the literal carries its offset; an exact
/// decimal carried as text is a number literal (`00123.50` into VARCHAR is
/// `123.50`), so it goes only into number columns, which read the text alike.
pub(crate) fn appender_refused(f: &databrain_connector_core::arrow::datatypes::Field, target: &str) -> Option<String> {
    use databrain_connector_core::arrow::datatypes::DataType as D;
    let t = target.trim().to_ascii_uppercase();
    let source = f.data_type();
    if databrain_connector_core::value::is_decimal_text(f) {
        let number = t.starts_with("DECIMAL") || matches!(t.as_str(), "DOUBLE" | "FLOAT" | "HUGEINT" | "BIGINT" | "INTEGER" | "SMALLINT" | "TINYINT");
        return (!number).then(|| format!("a decimal as text into {target} would be written as text, not as the number the INSERT writes"));
    }
    let fine = match source {
        D::Float64 => matches!(t.as_str(), "DOUBLE" | "FLOAT" | "VARCHAR"),
        D::Float32 => matches!(t.as_str(), "FLOAT" | "VARCHAR"),
        D::Timestamp(_, Some(_)) => t == "TIMESTAMP WITH TIME ZONE",
        _ => true,
    };
    (!fine).then(|| format!("{source} into {target} would be converted differently from an INSERT's value"))
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::test_util::*;
    use databrain_auth::AuthMethod;

    use databrain_connector_core::{CancellationToken, ConnectionConfig, ConnectorKind, ExecOptions, Statement};

    /// Stop of a statement still waiting for the connection stops only it:
    /// the one running goes on, and the stopped one never runs. A Stop of the
    /// running one still stops it.
    #[tokio::test(flavor = "multi_thread")]
    async fn stop_while_waiting_stops_only_that_statement() {
        let t = std::time::Duration::from_secs(20);
        let s = session(ConnectionConfig::new(ConnectorKind::Duckdb, AuthMethod::None)).await;
        let long = "select count(*) from range(400000000) a where a.range % 7 = 3";
        let running = s.execute(Statement::internal(long), ExecOptions::default()).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let stop = CancellationToken::new();
        let waiting = s.execute(Statement::internal("create table stopped(a int)"), ExecOptions { cancel: stop.clone(), ..Default::default() }).await.unwrap();
        stop.cancel();
        let r = tokio::time::timeout(t, running.collect()).await.expect("no hang").expect("the running statement isn't stopped");
        assert_eq!(r.num_rows(), 1);
        let e = tokio::time::timeout(t, waiting.collect()).await.expect("no hang").unwrap_err();
        assert!(e.is_cancelled(), "{e:?}");
        let n = tokio::time::timeout(t, q(&*s, "select count(*) from duckdb_tables() where table_name = 'stopped'")).await.expect("no hang");
        let n = databrain_connector_core::arrow::util::display::array_value_to_string(n.batches[0].column(0), 0).unwrap();
        assert_eq!(n, "0", "it never ran");
        // A real Stop still stops the running statement.
        let stop = CancellationToken::new();
        let running = s.execute(Statement::internal(long), ExecOptions { cancel: stop.clone(), ..Default::default() }).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        stop.cancel();
        let e = tokio::time::timeout(t, running.collect()).await.expect("stopped quickly").unwrap_err();
        assert!(e.is_cancelled(), "{e:?}");
        assert_eq!(tokio::time::timeout(t, q(&*s, "select 1")).await.expect("no hang").num_rows(), 1, "still usable");
    }

    /// The appender is refused for the pairs its casts convert differently
    /// from an INSERT's literal, read from the table's column types.
    #[tokio::test(flavor = "multi_thread")]
    async fn appender_refuses_pairs_converted_differently() {
        use databrain_connector_core::arrow::datatypes::{DataType as D, Field, Schema, TimeUnit};
        let s = session(ConnectionConfig::new(ConnectorKind::Duckdb, AuthMethod::None)).await;
        q(&*s, "create table \"T\" (i integer, d decimal(5,2), f float, x double, v varchar, ts timestamp, tz timestamptz, tm time)").await;
        let schema = |dt: D| Arc::new(Schema::new(vec![Field::new("c", dt, true)]));
        let tz = D::Timestamp(TimeUnit::Microsecond, Some("UTC".into()));
        for (dt, col) in [(D::Float64, "i"), (D::Float64, "d"), (D::Float32, "x"), (D::Float32, "D"), (tz.clone(), "ts"), (tz.clone(), "v"), (tz.clone(), "tm")]
        {
            let why = s.append_rows_refused("T", &[col.into()], &schema(dt.clone())).await.unwrap();
            assert!(why.as_deref().is_some_and(|w| w.starts_with(&format!("{col}: "))), "{dt:?} into {col}: {why:?}");
        }
        for (dt, col) in [
            (D::Float64, "x"),
            (D::Float64, "f"),
            (D::Float64, "v"),
            (D::Float32, "f"),
            (D::Float32, "V"),
            (tz, "tz"),
            (D::Int64, "x"),
            (D::Decimal128(38, 10), "i"),
        ] {
            assert_eq!(s.append_rows_refused("T", &[col.into()], &schema(dt.clone())).await.unwrap(), None, "{dt:?} into {col}");
        }
        // An exact decimal carried as text (`numeric` from another database).
        let decimal_text = Arc::new(Schema::new(vec![
            Field::new("c", D::Utf8, true).with_metadata([(databrain_connector_core::value::META_DB_TYPE.to_string(), "numeric(12,2)".to_string())].into()),
        ]));
        assert!(s.append_rows_refused("T", &["v".into()], &decimal_text).await.unwrap().is_some(), "decimal text into varchar");
        assert_eq!(s.append_rows_refused("T", &["d".into()], &decimal_text).await.unwrap(), None, "decimal text into decimal");
        assert_eq!(s.append_rows_refused("T", &["v".into()], &schema(D::Utf8)).await.unwrap(), None, "text into varchar");
    }

    /// The interrupt reaches a statement only while it holds the connection.
    #[test]
    fn an_interrupt_reaches_only_its_own_statement() {
        let r = Arc::new(Running::default());
        let (a, b) = (r.ticket(), r.ticket());
        let hits = std::cell::Cell::new(0);
        r.interrupt_if(a, || hits.set(hits.get() + 1));
        assert_eq!(hits.get(), 0, "not started");
        let held = r.start(a);
        r.interrupt_if(b, || hits.set(hits.get() + 10));
        r.interrupt_if(a, || hits.set(hits.get() + 1));
        assert_eq!(hits.get(), 1);
        drop(held);
        let _b = r.start(b);
        r.interrupt_if(a, || hits.set(hits.get() + 100));
        assert_eq!(hits.get(), 1, "a's late Stop doesn't reach b");
    }

    #[tokio::test]
    async fn catalogs_first() {
        let s = session(ConnectionConfig::new(ConnectorKind::Duckdb, AuthMethod::None)).await;
        q(&*s, "ATTACH ':memory:' AS other; CREATE SCHEMA other.sales").await;
        let cats = s.list_catalogs().await.unwrap().unwrap();
        assert_eq!(cats.iter().map(|c| (c.name.as_str(), c.is_default)).collect::<Vec<_>>(), vec![("memory", true), ("other", false)]);
        let sch: Vec<String> = s.list_catalog_schemas("other").await.unwrap().into_iter().map(|x| x.name).collect();
        let all: Vec<String> = s.list_schemas().await.unwrap().into_iter().filter(|x| x.catalog.as_deref() == Some("other")).map(|x| x.name).collect();
        assert!(sch.contains(&"other.sales".to_string()));
        assert_eq!(sch, all, "same ids as list_schemas");
    }

    #[tokio::test]
    async fn lists_sequences_and_macros() {
        let s = session(ConnectionConfig::new(ConnectorKind::Duckdb, AuthMethod::None)).await;
        q(&*s, "CREATE TABLE t (a INT); CREATE VIEW v AS SELECT 1; CREATE SEQUENCE seq1; CREATE MACRO add1(x) AS x + 1; CREATE MACRO t(x) AS x").await;
        let objs = s.list_objects("memory.main").await.unwrap();
        let k = |n: &str, kind: ObjectKind| objs.iter().any(|o| o.name == n && o.kind == kind);
        assert!(k("t", ObjectKind::Table) && k("v", ObjectKind::View) && k("seq1", ObjectKind::Sequence) && k("add1", ObjectKind::Function), "{objs:?}");
        assert_eq!(objs[0].kind, ObjectKind::Table, "tables first");
        // A macro with a table's name does not hide the table.
        assert_eq!(s.describe("memory.main", "t").await.unwrap().object.kind, ObjectKind::Table);
        // Routine completion (default: the default schema's listing), routines only.
        let r = s.search_routines(Some("memory.main"), "ADD", 10).await.unwrap();
        assert_eq!(r.iter().map(|o| (o.name.as_str(), o.kind)).collect::<Vec<_>>(), vec![("add1", ObjectKind::Function)]);
        assert!(s.search_routines(Some("memory.main"), "seq", 10).await.unwrap().is_empty(), "sequences are not routines");
    }

    #[tokio::test]
    async fn table_layout_lists_keys_and_indexes() {
        let s = session(ConnectionConfig::new(ConnectorKind::Duckdb, AuthMethod::None)).await;
        q(&*s, "CREATE TABLE o (id INTEGER PRIMARY KEY, code VARCHAR UNIQUE, cust INTEGER, day DATE)").await;
        q(&*s, "CREATE INDEX o_cust ON o (cust, day)").await;
        let l = s.table_layout("memory.main", "o").await.unwrap();
        assert!(l.indexes[0].primary, "{l:?}");
        assert_eq!(l.indexes[0].columns, vec!["id"]);
        assert!(l.indexes.iter().any(|i| i.unique && i.columns == vec!["code"]), "{l:?}");
        assert!(l.indexes.iter().any(|i| i.name == "o_cust" && i.columns == vec!["cust", "day"]), "{l:?}");
    }
}
