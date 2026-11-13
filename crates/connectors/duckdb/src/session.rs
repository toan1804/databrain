use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use databrain_connector_core::external::ExternalTablesSlot;
use databrain_connector_core::{
    ColumnInfo, ConnectorError, ConnectorKind, DbObject, ExecOptions, ObjectDetail, ObjectKind, QueryStream, Result, SchemaInfo, Session, TableColumns,
    quote_literal,
};

use crate::database::{FileConn, Loaded};
use crate::error::{map_err, sandbox_hint};
use crate::exec::run;
use crate::outputs::RESULTS_SCHEMA_ID;

pub struct DuckSession {
    pub(crate) conn: Arc<Mutex<FileConn>>,
    pub(crate) interrupt: Arc<duckdb::InterruptHandle>,
    pub(crate) outputs: Option<Arc<ExternalTablesSlot>>,
    /// Output tables loaded in the database: lowercase name → version key.
    pub(crate) loaded: Arc<Loaded>,
    /// Runs in a sealed instance (see [`seal`](crate::sandbox::seal)).
    pub(crate) sandboxed: bool,
    /// The session's connection is to a database file other sessions share.
    pub(crate) shared: bool,
    /// Transactions, followed from the statements run: the driver's
    /// `is_autocommit()` always says yes.
    pub(crate) state: Arc<databrain_connector_core::conn_state::ConnState>,
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

    async fn execute(&self, sql: &str, opts: ExecOptions) -> Result<QueryStream> {
        self.state.observe(sql, ConnectorKind::Duckdb);
        let (tx, stream) = QueryStream::channel(4);
        let conn = self.conn.clone();
        let sql = sql.to_string();
        let (cancel, interrupt) = (opts.cancel.clone(), self.interrupt.clone());
        let (outputs, loaded) = (self.outputs.clone(), self.loaded.clone());
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
        tokio::spawn(async move {
            tokio::select! {
                biased;
                _ = done.cancelled() => {}
                _ = cancel.cancelled() => interrupt.interrupt(),
            }
            drop(keepalive);
        });
        tokio::task::spawn_blocking(move || {
            let _done = done_guard.drop_guard();
            let Ok(g) = conn.lock() else {
                tx.blocking_send(Err(ConnectorError::internal("lock poisoned")));
                return;
            };
            if let Err(e) = run(&g, &sql, &opts, &tx, outputs.as_deref(), &loaded, shared, sandboxed) {
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
                 order by database_name = current_database() desc, schema_name = 'files' desc, 1, 2"
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
            .filter(|o| databrain_connector_core::object_matches(query, &o.schema, &o.name))
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
        let object = self
            .list_objects(schema)
            .await?
            .into_iter()
            .filter(|o| o.name == name)
            .min_by_key(|o| !o.kind.is_relation())
            .ok_or_else(|| ConnectorError::query(format!("object not found: {schema}.{name}")))?;
        let columns = self.columns(schema, Some(name)).await?.into_iter().next().map(|t| t.columns).unwrap_or_default();
        let (db, sch) = schema.split_once('.').unwrap_or((schema, "main"));
        let ddl = self
            .strings(
                "select sql from duckdb_tables() where database_name = ? and schema_name = ? and table_name = ? \
                 union all select sql from duckdb_views() where database_name = ? and schema_name = ? and view_name = ?"
                    .into(),
                vec![db.into(), sch.into(), name.into(), db.into(), sch.into(), name.into()],
            )
            .await?
            .into_iter()
            .next()
            .and_then(|r| r[0].clone());
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

#[cfg(test)]
mod tests {
    use super::*;

    use crate::test_util::*;
    use databrain_auth::AuthMethod;

    use databrain_connector_core::{ConnectionConfig, ConnectorKind};

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
