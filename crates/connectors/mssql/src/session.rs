//! The SQL Server session: connection handling, query execution and schema introspection.

use crate::config::Conn;
use crate::config::open;
use crate::error::map_err;
use crate::exec::Ended;
use crate::exec::run;
use crate::schema::COLUMNS_SQL;
use crate::schema::EXPLORER_FINGERPRINTS_SQL;
use crate::schema::MSSQL_TABLE_EXTRAS;
use crate::schema::SCHEMA_FILTER;
use crate::schema::SCHEMA_FINGERPRINTS_SQL;
use crate::schema::SCHEMAS_SQL;
use crate::schema::column_fill;
use crate::schema::column_from;
use crate::schema::column_line;
use crate::schema::each_db;
use crate::schema::in_db;
use crate::schema::table_ddl;
use async_trait::async_trait;
use databrain_connector_core::ColumnInfo;
use databrain_connector_core::ConnectorError;
use databrain_connector_core::ConnectorKind;
use databrain_connector_core::DbObject;
use databrain_connector_core::ExecOptions;
use databrain_connector_core::ExecSummary;
use databrain_connector_core::ObjectDetail;
use databrain_connector_core::ObjectKind;
use databrain_connector_core::QueryStream;
use databrain_connector_core::Result;
use databrain_connector_core::SchemaInfo;
use databrain_connector_core::Session;
use databrain_connector_core::StreamEvent;
use databrain_connector_core::TableColumns;
use databrain_connector_core::quote_ident;
use std::sync::Arc;
use tiberius::Config;
use tokio::sync::Mutex;

pub struct MssqlSession {
    pub(crate) config: Config,
    pub(crate) use_browser: bool,
    pub(crate) conn: Arc<Mutex<Option<Conn>>>,
    /// What the connection carries (transactions, SET, #temp tables).
    pub(crate) state: Arc<databrain_connector_core::ConnState>,
    /// The user ran `SET ROWCOUNT`: the row limit must not reset it.
    pub(crate) user_rowcount: Arc<std::sync::atomic::AtomicBool>,
    /// No database configured: every accessible database is a catalog and
    /// schema ids are `database.schema` (like Databricks catalogs).
    pub(crate) all_dbs: bool,
}

impl MssqlSession {
    pub(crate) async fn with_conn<T>(&self, f: impl for<'c> FnOnce(&'c mut Conn) -> futures::future::BoxFuture<'c, Result<T>>) -> Result<T> {
        let mut g = self.conn.lock().await;
        if g.is_none() {
            *g = Some(open(&self.config, self.use_browser).await?);
            self.state.reset();
        }
        let r = f(g.as_mut().expect("connected")).await;
        if matches!(&r, Err(e) if e.kind == databrain_connector_core::ErrorKind::Connection) {
            *g = None;
        }
        r
    }

    pub(crate) async fn rows(&self, sql: impl Into<String>, params: Vec<String>) -> Result<Vec<tiberius::Row>> {
        let sql: String = sql.into();
        self.with_conn(move |c| {
            Box::pin(async move {
                let p: Vec<&dyn tiberius::ToSql> = params.iter().map(|s| s as &dyn tiberius::ToSql).collect();
                c.query(sql.as_str(), &p).await.map_err(map_err)?.into_first_result().await.map_err(map_err)
            })
        })
        .await
    }

    /// Split a schema id: `(database, schema)` in all-databases mode.
    pub(crate) fn split<'a>(&self, schema: &'a str) -> (Option<&'a str>, &'a str) {
        match (self.all_dbs, schema.split_once('.')) {
            (true, Some((db, s))) => (Some(db), s),
            _ => (None, schema),
        }
    }

    /// The schema id of `schema` in `db` (`db.schema` in all-databases mode).
    pub(crate) fn id(&self, db: Option<&str>, schema: &str) -> String {
        match db {
            Some(db) if self.all_dbs => format!("{db}.{schema}"),
            _ => schema.to_string(),
        }
    }

    /// `rows` in the context of the schema id's database.
    pub(crate) async fn rows_in(&self, db: Option<&str>, sql: &str, params: Vec<String>) -> Result<Vec<tiberius::Row>> {
        self.rows(in_db(db, sql, params.len()), params).await
    }

    /// Every result set of a batch (`each_db`).
    pub(crate) async fn rows_all(&self, sql: String) -> Result<Vec<tiberius::Row>> {
        self.with_conn(move |c| {
            Box::pin(async move { Ok(c.simple_query(sql).await.map_err(map_err)?.into_results().await.map_err(map_err)?.into_iter().flatten().collect()) })
        })
        .await
    }

    /// Databases the login can open: `(name, is the session's default)`,
    /// user databases first. `model` and `tempdb` are left out.
    pub(crate) async fn databases(&self) -> Result<Vec<(String, bool)>> {
        let rows = self
            .rows(
                "select name, cast(case when name = db_name() and database_id > 4 then 1 else 0 end as bit) from sys.databases \
                 where state = 0 and name not in ('model', 'tempdb') and has_dbaccess(name) = 1 \
                 order by case when name = db_name() and database_id > 4 then 0 when database_id > 4 then 1 else 2 end, name",
                vec![],
            )
            .await?;
        Ok(rows.iter().filter_map(|r| Some((r.get::<&str, _>(0)?.to_string(), r.get::<bool, _>(1).unwrap_or(false)))).collect())
    }

    pub(crate) async fn db_names(&self) -> Result<Vec<String>> {
        Ok(self.databases().await?.into_iter().map(|d| d.0).collect())
    }

    /// A catalog query built at runtime (schema lists as N'' literals).
    pub(crate) async fn rows_dyn(&self, sql: String) -> Result<Vec<tiberius::Row>> {
        self.with_conn(move |c| Box::pin(async move { c.simple_query(sql).await.map_err(map_err)?.into_first_result().await.map_err(map_err) })).await
    }

    pub(crate) async fn query_text_in(&self, db: Option<&str>, sql: &str) -> Result<Vec<Vec<Option<String>>>> {
        databrain_connector_core::query_text(self, &in_db(db, sql, 0)).await
    }

    pub(crate) async fn rows_dyn_in(&self, db: Option<&str>, sql: String) -> Result<Vec<tiberius::Row>> {
        self.rows_dyn(in_db(db, &sql, 0)).await
    }

    /// `describe`, plus how the database fills each column (identity,
    /// computed; see `column_fill`), in column order.
    async fn describe_with_fills(&self, schema: &str, name: &str) -> Result<(ObjectDetail, Vec<Option<String>>)> {
        let objs = self.list_objects(schema).await?;
        let object = objs
            .into_iter()
            .filter(|o| o.name == name)
            .min_by_key(|o| !o.kind.is_relation())
            .ok_or_else(|| ConnectorError::query(format!("object not found: {schema}.{name}")))?;
        let (db, sch) = self.split(schema);
        let rows = self.rows_in(db, COLUMNS_SQL, vec![sch.into(), name.into()]).await?;
        let columns: Vec<ColumnInfo> = rows.iter().map(|r| column_from(r).1).collect();
        let fills: Vec<Option<String>> = rows.iter().map(column_fill).collect();
        let full = format!("{}.{}", quote_ident(ConnectorKind::Mssql, sch), quote_ident(ConnectorKind::Mssql, name));
        let ddl = match object.kind {
            ObjectKind::Table => Some(table_ddl(&full, &columns, &fills)),
            _ => self
                .rows_in(db, "select object_definition(object_id(@P1))", vec![full.clone()])
                .await?
                .first()
                .and_then(|r| r.get::<&str, _>(0).map(str::to_string)),
        };
        let foreign_keys = self.foreign_keys(schema, Some(name)).await?.into_iter().map(|(_, f)| f).collect();
        Ok((ObjectDetail { object, columns, ddl, foreign_keys }, fills))
    }
}

#[async_trait]
impl Session for MssqlSession {
    fn kind(&self) -> ConnectorKind {
        ConnectorKind::Mssql
    }

    async fn server_version(&self) -> Result<String> {
        let r = self.rows("select @@version", vec![]).await?;
        Ok(r.first().and_then(|r| r.get::<&str, _>(0)).map(|s| s.lines().next().unwrap_or(s).trim().to_string()).unwrap_or_default())
    }

    fn in_transaction(&self) -> bool {
        self.state.in_transaction()
    }

    async fn ping(&self) -> Result<()> {
        self.rows("select 1", vec![]).await.map(|_| ())
    }

    async fn execute(&self, sql: &str, opts: ExecOptions) -> Result<QueryStream> {
        let (tx, stream) = QueryStream::channel(4);
        let conn = self.conn.clone();
        let state = self.state.clone();
        let user_rowcount = self.user_rowcount.clone();
        let (config, browser) = (self.config.clone(), self.use_browser);
        let sql = sql.to_string();
        tokio::spawn(async move {
            let mut g = conn.lock().await;
            if g.is_none() {
                match open(&config, browser).await {
                    Ok(c) => {
                        *g = Some(c);
                        state.reset();
                    }
                    Err(e) => {
                        tx.send_err(e).await;
                        return;
                    }
                }
            }
            state.observe(&sql, ConnectorKind::Mssql);
            let replaceable = state.replaceable();
            let lower = sql.to_ascii_lowercase();
            if lower.contains("rowcount") && lower.contains("set") {
                user_rowcount.store(true, std::sync::atomic::Ordering::SeqCst);
            }
            let user_rowcount = user_rowcount.load(std::sync::atomic::Ordering::SeqCst);
            let c = g.as_mut().expect("connected");
            let res = tokio::select! {
                r = run(c, &sql, &opts, &tx, replaceable, user_rowcount) => r,
                _ = opts.cancel.cancelled() => Err(ConnectorError::cancelled()),
            };
            match res {
                Ok(Ended::Complete) => {}
                Ok(Ended::Stopped { last, unread }) => {
                    if unread {
                        // Stopped mid-result: reading the rest would take as
                        // long as the whole result. Close this connection
                        // (the server stops sending) and open a new one for
                        // the next statement.
                        *g = None;
                    }
                    drop(g);
                    if let Some(b) = last
                        && !tx.send(StreamEvent::Batch(b)).await
                    {
                        return;
                    }
                    tx.send(StreamEvent::Done(ExecSummary::default())).await;
                }
                Err(mut e) => {
                    if e.is_cancelled() || e.kind == databrain_connector_core::ErrorKind::Connection {
                        // Connection state is unknown after an aborted read.
                        *g = None;
                        if e.is_cancelled() && !replaceable {
                            e.message.push_str(
                                ". SQL Server statements can only be stopped by closing the connection: its open transaction, SET options and #temp tables are gone",
                            );
                        }
                    }
                    tx.send_err(e).await;
                }
            }
        });
        Ok(stream)
    }

    async fn list_schemas(&self) -> Result<Vec<SchemaInfo>> {
        if !self.all_dbs {
            let rows = self.rows(SCHEMAS_SQL, vec![]).await?;
            return Ok(rows
                .iter()
                .map(|r| SchemaInfo {
                    name: r.get::<&str, _>(0).unwrap_or_default().to_string(),
                    is_default: r.get::<bool, _>(1).unwrap_or(false),
                    catalog: None,
                })
                .collect());
        }
        // Every database's schemas in one batch.
        let dbs = self.databases().await?;
        let names: Vec<String> = dbs.iter().map(|d| d.0.clone()).collect();
        let sql = format!("select db_name(), s.name, cast(case when s.name = schema_name() then 1 else 0 end as bit) from sys.schemas s where {SCHEMA_FILTER}");
        let rows = self.rows_all(each_db(&names, &sql)).await?;
        let mut out: Vec<SchemaInfo> = rows
            .iter()
            .filter_map(|r| {
                let (db, sch) = (r.get::<&str, _>(0)?, r.get::<&str, _>(1)?);
                let default_db = dbs.iter().any(|(n, d)| *d && n == db);
                Some(SchemaInfo::in_catalog(db, sch, default_db && r.get::<bool, _>(2).unwrap_or(false)))
            })
            .collect();
        out.sort_by_key(|s| !s.is_default);
        Ok(out)
    }

    async fn list_catalogs(&self) -> Result<Option<Vec<databrain_connector_core::CatalogInfo>>> {
        if !self.all_dbs {
            return Ok(None);
        }
        Ok(Some(self.databases().await?.into_iter().map(|(name, is_default)| databrain_connector_core::CatalogInfo { name, is_default }).collect()))
    }

    async fn list_catalog_schemas(&self, catalog: &str) -> Result<Vec<SchemaInfo>> {
        if !self.all_dbs {
            return Ok(self.list_schemas().await?.into_iter().filter(|s| s.catalog.as_deref() == Some(catalog)).collect());
        }
        let default_db = self.databases().await?.into_iter().any(|(n, d)| d && n == catalog);
        let rows = self.rows_in(Some(catalog), SCHEMAS_SQL, vec![]).await?;
        Ok(rows
            .iter()
            .filter_map(|r| Some(SchemaInfo::in_catalog(catalog, r.get::<&str, _>(0)?, default_db && r.get::<bool, _>(1).unwrap_or(false))))
            .collect())
    }

    async fn search_objects(&self, query: &str, limit: usize) -> Result<Vec<DbObject>> {
        if !self.all_dbs {
            return databrain_connector_core::search_by_listing(self, query, limit).await;
        }
        // One query per database, in one batch.
        let term = databrain_connector_core::search_sql_term(query).replace('[', "[[]").replace('%', "[%]").replace('_', "[_]");
        let sql = format!(
            "select top {} db_name(), schema_name(o.schema_id), o.name, rtrim(o.type) from sys.objects o \
             where o.type in ('U','V') and o.is_ms_shipped = 0 and lower(o.name) like N{}",
            (limit * 4).max(200),
            databrain_connector_core::quote_literal(&format!("%{term}%"))
        );
        let rows = self.rows_all(each_db(&self.db_names().await?, &sql)).await?;
        let mut hits: Vec<DbObject> = rows
            .iter()
            .filter_map(|r| {
                Some(DbObject {
                    schema: format!("{}.{}", r.get::<&str, _>(0)?, r.get::<&str, _>(1)?),
                    name: r.get::<&str, _>(2)?.to_string(),
                    kind: if r.get::<&str, _>(3) == Some("V") { ObjectKind::View } else { ObjectKind::Table },
                    comment: None,
                    row_estimate: None,
                })
            })
            .filter(|o| databrain_connector_core::object_matches(query, &o.schema, &o.name))
            .collect();
        databrain_connector_core::rank_matches(query, &mut hits, limit);
        Ok(hits)
    }

    async fn list_objects(&self, schema: &str) -> Result<Vec<DbObject>> {
        let (db, sch) = self.split(schema);
        let rows = self
            .rows_in(
                db,
                "select o.name, rtrim(o.type), cast(ep.value as nvarchar(4000)), \
                   (select sum(p.rows) from sys.partitions p where p.object_id = o.object_id and p.index_id in (0,1)) \
                 from sys.objects o \
                 left join sys.extended_properties ep on ep.major_id = o.object_id and ep.minor_id = 0 and ep.name = 'MS_Description' \
                 where schema_name(o.schema_id) = @P1 and o.type in ('U','V','P','FN','IF','TF','SO') and o.is_ms_shipped = 0 \
                 order by case rtrim(o.type) when 'U' then 0 when 'V' then 1 when 'SO' then 3 else 2 end, o.name",
                vec![sch.into()],
            )
            .await?;
        Ok(rows
            .iter()
            .map(|r| DbObject {
                schema: schema.to_string(),
                name: r.get::<&str, _>(0).unwrap_or_default().to_string(),
                kind: match r.get::<&str, _>(1).unwrap_or_default() {
                    "U" => ObjectKind::Table,
                    "V" => ObjectKind::View,
                    "P" => ObjectKind::Procedure,
                    "SO" => ObjectKind::Sequence,
                    _ => ObjectKind::Function,
                },
                comment: r.get::<&str, _>(2).map(str::to_string),
                row_estimate: r.get::<i64, _>(3),
            })
            .collect())
    }

    async fn table_layout(&self, schema: &str, name: &str) -> Result<databrain_connector_core::TableLayout> {
        use databrain_connector_core::{TableLayout, group_indexes, quote_literal, truthy};
        let (db, schema) = self.split(schema);
        let obj = format!("N{}", quote_literal(&format!("{}.{}", quote_ident(ConnectorKind::Mssql, schema), quote_ident(ConnectorKind::Mssql, name))));
        let idx = self
            .query_text_in(
                db,
                &format!(
                    "select i.name, c.name, cast(i.is_unique as int), cast(i.is_primary_key as int), lower(i.type_desc) \
                 from sys.indexes i \
                 join sys.index_columns ic on ic.object_id = i.object_id and ic.index_id = i.index_id \
                 join sys.columns c on c.object_id = ic.object_id and c.column_id = ic.column_id \
                 where i.object_id = object_id({obj}) and ic.is_included_column = 0 and ic.key_ordinal > 0 \
                 order by i.is_primary_key desc, i.name, ic.key_ordinal"
                ),
            )
            .await?;
        let mut l = TableLayout {
            indexes: group_indexes(idx.into_iter().map(|r| {
                let m = r[4].clone().filter(|m| m != "nonclustered");
                (r[0].clone().unwrap_or_default(), r[1].clone().unwrap_or_default(), truthy(&r[2]), truthy(&r[3]), m)
            })),
            ..Default::default()
        };
        let heap = self.query_text_in(db, &format!("select count(*) from sys.indexes where object_id = object_id({obj}) and type = 0")).await.ok();
        if heap.as_ref().and_then(|r| r.first()).and_then(|r| r[0].as_deref()) == Some("1") {
            l.notes.push("heap: no clustered index".into());
        }
        if let Ok(p) = self
            .query_text_in(
                db,
                &format!(
                    "select c.name, lower(pf.type_desc) from sys.indexes i \
                 join sys.partition_schemes ps on ps.data_space_id = i.data_space_id \
                 join sys.partition_functions pf on pf.function_id = ps.function_id \
                 join sys.index_columns ic on ic.object_id = i.object_id and ic.index_id = i.index_id and ic.partition_ordinal > 0 \
                 join sys.columns c on c.object_id = ic.object_id and c.column_id = ic.column_id \
                 where i.object_id = object_id({obj}) and i.index_id in (0, 1) order by ic.partition_ordinal"
                ),
            )
            .await
        {
            l.partition_by = p.iter().filter_map(|r| r[0].clone()).collect();
            l.partition_kind = p.first().and_then(|r| r[1].clone());
        }
        if let Ok(r) = self.query_text_in(db, &format!("select sum(rows) from sys.partitions where object_id = object_id({obj}) and index_id in (0, 1)")).await
        {
            l.row_estimate = r.first().and_then(|r| r[0].as_deref()).and_then(|x| x.parse().ok());
        }
        // Pages in use (8 KB), data and indexes; catalog views, no extra permission.
        if let Ok(r) = self
            .query_text_in(
                db,
                &format!("select sum(a.used_pages) * 8192 from sys.partitions p join sys.allocation_units a on a.container_id = p.partition_id where p.object_id = object_id({obj})"),
            )
            .await
        {
            l.size_bytes = r.first().and_then(|r| r[0].as_deref()).and_then(|x| x.parse().ok());
        }
        Ok(l)
    }

    /// Tables: columns plus keys, foreign keys, checks and indexes. Views,
    /// procedures and functions: their definition.
    async fn object_ddl(&self, schema: &str, name: &str, _kind: ObjectKind) -> Result<Option<String>> {
        let (d, fills) = self.describe_with_fills(schema, name).await?;
        if d.object.kind != ObjectKind::Table {
            return Ok(d.ddl);
        }
        let (db, schema) = self.split(schema);
        let full = format!("{}.{}", quote_ident(ConnectorKind::Mssql, schema), quote_ident(ConnectorKind::Mssql, name));
        // One row per extra line: (0 = inside CREATE TABLE, 1 = after it, text).
        let extra = self.rows_in(db, MSSQL_TABLE_EXTRAS, vec![full.clone()]).await;
        let Ok(extra) = extra else { return Ok(d.ddl) }; // e.g. no STRING_AGG (before 2017)
        let mut inside: Vec<String> = Vec::new();
        let mut after: Vec<String> = Vec::new();
        for r in &extra {
            let line = r.get::<&str, _>(1).unwrap_or_default().to_string();
            if r.get::<i32, _>(0) == Some(0) { inside.push(format!("    {line}")) } else { after.push(format!("{line};")) }
        }
        let mut lines: Vec<String> = d.columns.iter().enumerate().map(|(i, c)| column_line(c, fills.get(i).and_then(|f| f.as_deref()))).collect();
        lines.extend(inside);
        let mut ddl = format!("CREATE TABLE {full} (\n{}\n);", lines.join(",\n"));
        for a in after {
            ddl.push('\n');
            ddl.push_str(&a);
        }
        Ok(Some(ddl))
    }

    async fn describe(&self, schema: &str, name: &str) -> Result<ObjectDetail> {
        Ok(self.describe_with_fills(schema, name).await?.0)
    }

    /// Per schema: object count + last modify_date of tables, views and
    /// foreign keys (ALTER TABLE updates it), plus a checksum of
    /// MS_Description comments (which do not touch modify_date).
    /// Every object the explorer lists (tables, views, routines, sequences):
    /// count + checksum of (id, modify_date) + table comments.
    async fn explorer_fingerprints(&self) -> Result<Option<std::collections::HashMap<String, String>>> {
        if !self.all_dbs {
            let rows = self.rows(EXPLORER_FINGERPRINTS_SQL, vec![]).await?;
            return Ok(Some(
                rows.iter().filter_map(|r| Some((r.get::<&str, _>(0)?.to_string(), r.get::<&str, _>(1).unwrap_or_default().to_string()))).collect(),
            ));
        }
        let sql = format!("select db_name() + '.' + x.n, x.f from ({EXPLORER_FINGERPRINTS_SQL}) x(n, f)");
        let rows = self.rows_all(each_db(&self.db_names().await?, &sql)).await?;
        Ok(Some(rows.iter().filter_map(|r| Some((r.get::<&str, _>(0)?.to_string(), r.get::<&str, _>(1).unwrap_or_default().to_string()))).collect()))
    }

    /// modify_date per object (any ALTER, column change) + its comments.
    async fn object_versions(&self, schema: &str) -> Result<Option<std::collections::HashMap<String, String>>> {
        let (db, schema) = self.split(schema);
        let rows = self
            .rows_in(
                db,
                "select rtrim(o.type), o.name, concat(convert(varchar(30), o.modify_date, 126), ':', \
                   (select checksum_agg(checksum(ep.minor_id, cast(ep.value as nvarchar(4000)))) from sys.extended_properties ep \
                      where ep.class = 1 and ep.major_id = o.object_id and ep.name = 'MS_Description')) \
                 from sys.objects o where schema_name(o.schema_id) = @P1 and o.type in ('U','V','P','FN','IF','TF','SO') and o.is_ms_shipped = 0",
                vec![schema.into()],
            )
            .await?;
        Ok(Some(
            rows.iter()
                .filter_map(|r| {
                    let kind = match r.get::<&str, _>(0)? {
                        "U" => ObjectKind::Table,
                        "V" => ObjectKind::View,
                        "P" => ObjectKind::Procedure,
                        "SO" => ObjectKind::Sequence,
                        _ => ObjectKind::Function,
                    };
                    Some((databrain_connector_core::version_key(kind, r.get::<&str, _>(1)?), r.get::<&str, _>(2).unwrap_or_default().to_string()))
                })
                .collect(),
        ))
    }

    async fn schema_fingerprints(&self) -> Result<Option<std::collections::HashMap<String, String>>> {
        if !self.all_dbs {
            let rows = self.rows(SCHEMA_FINGERPRINTS_SQL, vec![]).await?;
            return Ok(Some(
                rows.iter().filter_map(|r| Some((r.get::<&str, _>(0)?.to_string(), r.get::<&str, _>(1).unwrap_or_default().to_string()))).collect(),
            ));
        }
        let sql = format!("select db_name() + '.' + x.n, x.f from ({SCHEMA_FINGERPRINTS_SQL}) x(n, f)");
        let rows = self.rows_all(each_db(&self.db_names().await?, &sql)).await?;
        Ok(Some(rows.iter().filter_map(|r| Some((r.get::<&str, _>(0)?.to_string(), r.get::<&str, _>(1).unwrap_or_default().to_string()))).collect()))
    }

    /// Three catalog queries for the whole batch of schemas.
    async fn bulk_metadata(&self, schemas: &[String]) -> Result<Vec<databrain_connector_core::SchemaMetadata>> {
        match self.bulk_metadata_batched(schemas).await {
            Ok(m) => Ok(m),
            Err(_) => databrain_connector_core::default_bulk_metadata(self, schemas).await,
        }
    }

    async fn schema_columns(&self, schema: &str) -> Result<Vec<TableColumns>> {
        let (db, sch) = self.split(schema);
        let rows = self.rows_in(db, COLUMNS_SQL, vec![sch.into(), String::new()]).await?;
        let mut out: Vec<TableColumns> = Vec::new();
        for r in &rows {
            let (table, col) = column_from(r);
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
    use crate::MssqlConnector;
    use databrain_connector_core::ConnectionConfig;
    use databrain_connector_core::Connector;
    use std::time::Duration;

    /// Live: DATABRAIN_MSSQL_HOST, _USER, _PASSWORD (e.g. the mcr.microsoft.com/mssql/server image).
    #[ignore = "live: needs DATABRAIN_MSSQL_HOST (cargo test -- --ignored)"]
    #[tokio::test]
    async fn live_roundtrip() {
        let Ok(host) = std::env::var("DATABRAIN_MSSQL_HOST") else {
            panic!("live test: DATABRAIN_MSSQL_HOST not set (run with --ignored)");
        };
        use databrain_auth::{AuthMethod, InlineCredentialSource};
        let user = std::env::var("DATABRAIN_MSSQL_USER").unwrap_or_else(|_| "sa".into());
        let mut cfg = ConnectionConfig::new(ConnectorKind::Mssql, AuthMethod::Password { user: user.clone() });
        cfg.host = Some(host);
        cfg.port = std::env::var("DATABRAIN_MSSQL_PORT").ok().and_then(|p| p.parse().ok());
        // DATABRAIN_MSSQL_SSL=disable to test without TLS.
        if std::env::var("DATABRAIN_MSSQL_SSL").as_deref() == Ok("disable") {
            cfg.ssl_mode = databrain_connector_core::SslMode::Disable;
        }
        cfg.options.insert("trust_cert".into(), "true".into());
        let creds = Arc::new(InlineCredentialSource::new(
            AuthMethod::Password { user },
            std::env::var("DATABRAIN_MSSQL_PASSWORD").ok().map(secrecy::SecretString::from),
        ));
        let s = MssqlConnector.connect(&cfg, creds).await.unwrap();
        let r = s
            .execute("select 1 as a, N'x' as b, sysdatetime() as c, cast(1.5 as decimal(5,2)) as d", ExecOptions::default())
            .await
            .unwrap()
            .collect()
            .await
            .unwrap();
        assert_eq!(r.num_rows(), 1);
        assert!(!s.list_schemas().await.unwrap().is_empty());
        let cancel = databrain_connector_core::CancellationToken::new();
        let st = s.execute("waitfor delay '00:00:30'", ExecOptions { batch_size: 10, cancel: cancel.clone(), ..Default::default() }).await.unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        cancel.cancel();
        assert!(st.collect().await.unwrap_err().is_cancelled());
        assert_eq!(s.execute("select 2", ExecOptions::default()).await.unwrap().collect().await.unwrap().num_rows(), 1);

        // Show DDL: a table with every kind of constraint and an index, recreated from its DDL.
        let run = |sql: String| {
            let s = &s;
            async move { s.execute(&sql, ExecOptions::default()).await.unwrap().collect().await.unwrap_or_else(|e| panic!("{sql}: {e}")) }
        };
        for stmt in [
            "if object_id('dbo.kb_orders') is not null drop table dbo.kb_orders",
            "if object_id('dbo.kb_customers') is not null drop table dbo.kb_customers",
            "create table dbo.kb_customers (id int not null constraint pk_kb_customers primary key, email nvarchar(100) not null constraint uq_kb_email unique)",
            "create table dbo.kb_orders (id int not null, customer_id int not null, status varchar(10) not null constraint df_kb_status default 'new', \
               total decimal(10,2), constraint pk_kb_orders primary key clustered (id desc), \
               constraint fk_kb_customer foreign key (customer_id) references dbo.kb_customers (id) on delete cascade, \
               constraint ck_kb_status check (status in ('new','paid')))",
            "create index ix_kb_status on dbo.kb_orders (status) include (total) where status <> 'paid'",
        ] {
            run(stmt.into()).await;
        }
        let ddl = s.object_ddl("dbo", "kb_orders", ObjectKind::Table).await.unwrap().unwrap();
        for part in [
            "CREATE TABLE [dbo].[kb_orders] (",
            "CONSTRAINT [pk_kb_orders] PRIMARY KEY CLUSTERED ([id] DESC)",
            "CONSTRAINT [fk_kb_customer] FOREIGN KEY ([customer_id]) REFERENCES [dbo].[kb_customers] ([id]) ON DELETE CASCADE",
            "CONSTRAINT [ck_kb_status] CHECK",
            "CREATE NONCLUSTERED INDEX [ix_kb_status] ON [dbo].[kb_orders] ([status]) INCLUDE ([total]) WHERE",
        ] {
            assert!(ddl.contains(part), "missing {part:?} in\n{ddl}");
        }
        assert!(
            s.object_ddl("dbo", "kb_customers", ObjectKind::Table).await.unwrap().unwrap().contains("CONSTRAINT [uq_kb_email] UNIQUE NONCLUSTERED ([email])")
        );
        run("drop table dbo.kb_orders".into()).await;
        for stmt in ddl.split(";\n").map(str::trim).filter(|x| !x.is_empty()) {
            run(stmt.trim_end_matches(';').to_string()).await;
        }
        assert_eq!(s.object_ddl("dbo", "kb_orders", ObjectKind::Table).await.unwrap().unwrap(), ddl, "same DDL after recreating");
        run("if object_id('dbo.kb_noop') is not null drop procedure dbo.kb_noop".into()).await;
        run("create procedure dbo.kb_noop as select 1".into()).await;
        assert!(s.object_ddl("dbo", "kb_noop", ObjectKind::Procedure).await.unwrap().unwrap().contains("procedure dbo.kb_noop"));
        for stmt in ["drop table dbo.kb_orders", "drop table dbo.kb_customers", "drop procedure dbo.kb_noop"] {
            run(stmt.into()).await;
        }
    }
}
