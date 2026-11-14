use async_trait::async_trait;
use databrain_connector_core::{
    ColumnInfo, ConnectorError, ConnectorKind, DbObject, ExecOptions, ObjectDetail, ObjectKind, QueryStream, Result, SchemaInfo, Session, TableColumns,
};
use postgres_native_tls::MakeTlsConnector;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio_postgres::Client;

use crate::connect::Link;
use crate::ddl::{quote, table_ddl};
use crate::error::map_err;
use crate::exec::run;

pub struct PgSession {
    pub(crate) link: Arc<Link>,
    pub(crate) tls: MakeTlsConnector,
    /// Read-only connection: every statement starts read-only, also after
    /// something (`set_config(…)` in a function) turned the default off.
    pub(crate) read_only: bool,
    /// Amazon Redshift (PostgreSQL 8.0 catalog): its own metadata queries.
    pub(crate) redshift: bool,
}

impl PgSession {
    pub(crate) fn client(&self) -> Arc<Client> {
        self.link.client()
    }
}

#[async_trait]
impl Session for PgSession {
    fn kind(&self) -> ConnectorKind {
        ConnectorKind::Postgres
    }

    async fn server_version(&self) -> Result<String> {
        let row = self.client().query_one("select version()", &[]).await.map_err(|e| map_err(&e))?;
        Ok(row.get(0))
    }

    fn transaction_state(&self) -> databrain_connector_core::TxState {
        self.link.state.tx_state()
    }

    async fn ping(&self) -> Result<()> {
        self.client().simple_query("select 1").await.map(|_| ()).map_err(|e| map_err(&e))
    }

    async fn execute(&self, stmt: databrain_connector_core::Statement<'_>, opts: ExecOptions) -> Result<QueryStream> {
        let sql = stmt.sql();
        let (tx, stream) = QueryStream::channel(4);
        let link = self.link.clone();
        let cancel_token = link.client().cancel_token();
        let tls = self.tls.clone();
        let sql = sql.to_string();
        let finished = Arc::new(AtomicBool::new(false));
        let read_only = self.read_only && !self.link.state.in_transaction();
        tokio::spawn(async move {
            let cancel = opts.cancel.clone();
            if read_only {
                // A transaction that began read-only can't be made writable
                // once it ran a query, so this holds for the whole statement.
                if let Err(e) = link.client().batch_execute("SET SESSION CHARACTERISTICS AS TRANSACTION READ ONLY").await {
                    tx.send_err(map_err(&e)).await;
                    return;
                }
            }
            let work = run(link, sql, opts, tx.clone(), finished.clone());
            tokio::pin!(work);
            let res = tokio::select! {
                r = &mut work => r,
                _ = cancel.cancelled() => {
                    // Ask the server to cancel, then let the query finish with
                    // its cancellation error (bounded wait). Not when the
                    // statement is done with the server (e.g. stopped at the
                    // row limit just now): the cancel could hit the next one.
                    if !finished.load(Ordering::SeqCst) {
                        let _ = cancel_token.cancel_query(tls).await;
                    }
                    match tokio::time::timeout(Duration::from_secs(10), &mut work).await {
                        Ok(Ok(())) => Ok(()),
                        _ => Err(ConnectorError::cancelled()),
                    }
                }
            };
            if let Err(e) = res {
                let e = if cancel.is_cancelled() { ConnectorError::cancelled() } else { e };
                tx.send_err(e).await;
            }
        });
        Ok(stream)
    }

    async fn list_schemas(&self) -> Result<Vec<SchemaInfo>> {
        let rows = self
            .client()
            .query(
                "select nspname::text, nspname = current_schema() \
                 from pg_catalog.pg_namespace \
                 where nspname !~ '^pg_(toast|temp_|toast_temp_)' \
                 order by nspname = current_schema() desc, nspname like 'pg_%', nspname = 'information_schema', nspname",
                &[],
            )
            .await
            .map_err(|e| map_err(&e))?;
        Ok(rows.iter().map(|r| SchemaInfo { name: r.get(0), is_default: r.get::<_, Option<bool>>(1).unwrap_or(false), catalog: None }).collect())
    }

    async fn schema_object_counts(&self) -> Result<Option<std::collections::HashMap<String, usize>>> {
        if self.redshift {
            return Ok(None);
        }
        let rows = self
            .client()
            .query(
                "select n.nspname::text, count(*) from pg_catalog.pg_class c join pg_catalog.pg_namespace n on n.oid = c.relnamespace \
                 where c.relkind in ('r','p','v','m','f') and not c.relispartition group by 1",
                &[],
            )
            .await
            .map_err(|e| map_err(&e))?;
        Ok(Some(rows.iter().map(|r| (r.get::<_, String>(0), r.get::<_, i64>(1) as usize)).collect()))
    }

    async fn search_objects(&self, query: &str, limit: usize) -> Result<Vec<DbObject>> {
        if self.redshift {
            let term = databrain_connector_core::search_sql_term(query);
            let mut hits: Vec<DbObject> =
                self.rs_search_objects(&term).await?.into_iter().filter(|o| databrain_connector_core::object_matches(query, &o.schema, &o.name)).collect();
            databrain_connector_core::rank_matches(query, &mut hits, limit);
            return Ok(hits);
        }
        let term = databrain_connector_core::search_sql_term(query);
        let rows = self
            .client()
            .query(
                "select n.nspname::text, c.relname::text, c.relkind::text, \
                        obj_description(c.oid, 'pg_class'), c.reltuples::float8 \
                 from pg_catalog.pg_class c \
                 join pg_catalog.pg_namespace n on n.oid = c.relnamespace \
                 where c.relkind in ('r','p','v','m','f') and not c.relispartition \
                   and n.nspname !~ '^pg_' and n.nspname <> 'information_schema' \
                   and strpos(lower(c.relname), $1) > 0 \
                 limit 2000",
                &[&term],
            )
            .await
            .map_err(|e| map_err(&e))?;
        let mut hits: Vec<DbObject> = rows
            .iter()
            .map(|r| {
                let est: Option<f64> = r.get(4);
                DbObject {
                    schema: r.get(0),
                    name: r.get(1),
                    kind: match r.get::<_, String>(2).as_str() {
                        "v" => ObjectKind::View,
                        "m" => ObjectKind::MaterializedView,
                        "f" => ObjectKind::ForeignTable,
                        _ => ObjectKind::Table,
                    },
                    comment: r.get(3),
                    row_estimate: est.filter(|e| *e >= 0.0).map(|e| e as i64),
                }
            })
            .filter(|o| databrain_connector_core::object_matches(query, &o.schema, &o.name))
            .collect();
        databrain_connector_core::rank_matches(query, &mut hits, limit);
        Ok(hits)
    }

    async fn search_schema(&self, schema: &str, query: &str, limit: usize) -> Result<Vec<DbObject>> {
        if self.redshift {
            return self.rs_search_schema(schema, &databrain_connector_core::search_sql_term(query), limit).await;
        }
        let term = databrain_connector_core::search_sql_term(query);
        let rows = self
            .client()
            .query(
                "select c.relname::text, c.relkind::text, obj_description(c.oid, 'pg_class'), c.reltuples::float8 \
                 from pg_catalog.pg_class c \
                 join pg_catalog.pg_namespace n on n.oid = c.relnamespace \
                 where n.nspname = $1 and c.relkind in ('r','p','v','m','f') and not c.relispartition \
                   and strpos(lower(c.relname), $2) > 0 \
                 order by strpos(lower(c.relname), $2) <> 1, length(c.relname), c.relname \
                 limit $3",
                &[&schema, &term, &(limit.clamp(1, 1000) as i64)],
            )
            .await
            .map_err(|e| map_err(&e))?;
        Ok(rows
            .iter()
            .map(|r| {
                let est: Option<f64> = r.get(3);
                DbObject {
                    schema: schema.to_string(),
                    name: r.get(0),
                    kind: match r.get::<_, String>(1).as_str() {
                        "v" => ObjectKind::View,
                        "m" => ObjectKind::MaterializedView,
                        "f" => ObjectKind::ForeignTable,
                        _ => ObjectKind::Table,
                    },
                    comment: r.get(2),
                    row_estimate: est.filter(|e| *e >= 0.0).map(|e| e as i64),
                }
            })
            .collect())
    }

    /// One pg_proc query: the given schema, else every schema on the
    /// search path (system schemas left out; their functions are keywords).
    async fn search_routines(&self, schema: Option<&str>, query: &str, limit: usize) -> Result<Vec<DbObject>> {
        if self.redshift {
            let mut objs = self.rs_search_routines(schema, &databrain_connector_core::search_sql_term(query)).await?;
            databrain_connector_core::rank_matches(query, &mut objs, limit.clamp(1, 1000));
            return Ok(objs);
        }
        let term = databrain_connector_core::search_sql_term(query);
        let rows = self
            .client()
            .query(
                "select distinct on (n.nspname, p.proname) n.nspname::text, p.proname::text, p.prokind::text \
                 from pg_catalog.pg_proc p join pg_catalog.pg_namespace n on n.oid = p.pronamespace \
                 where p.prokind in ('f','p') and strpos(lower(p.proname), $1) > 0 \
                   and case when $2::text is null then n.nspname = any(current_schemas(false)) else n.nspname = $2 end \
                   and n.nspname not in ('pg_catalog','information_schema')",
                &[&term, &schema],
            )
            .await
            .map_err(|e| map_err(&e))?;
        let mut objs: Vec<DbObject> = rows
            .iter()
            .map(|r| {
                let kind = if r.get::<_, String>(2) == "p" { ObjectKind::Procedure } else { ObjectKind::Function };
                DbObject { schema: r.get(0), name: r.get(1), kind, comment: None, row_estimate: None }
            })
            .collect();
        databrain_connector_core::rank_matches(query, &mut objs, limit.clamp(1, 1000));
        Ok(objs)
    }

    /// Tables: columns, every constraint (pg_get_constraintdef), partition
    /// key, other indexes and comments. Views as before. Functions and
    /// procedures: pg_get_functiondef (all overloads).
    async fn object_ddl(&self, schema: &str, name: &str, kind: ObjectKind) -> Result<Option<String>> {
        if self.redshift {
            return self.rs_object_ddl(schema, name, kind).await;
        }
        let q = |sql: &'static str, oid: u32| async move { self.client().query(sql, &[&oid]).await.map_err(|e| map_err(&e)) };
        if matches!(kind, ObjectKind::Function | ObjectKind::Procedure) {
            let rows = self
                .client()
                .query(
                    "select pg_catalog.pg_get_functiondef(p.oid) from pg_catalog.pg_proc p \
                     join pg_catalog.pg_namespace n on n.oid = p.pronamespace \
                     where n.nspname = $1 and p.proname = $2 and p.prokind in ('f','p','w') order by p.oid",
                    &[&schema, &name],
                )
                .await
                .map_err(|e| map_err(&e))?;
            let defs: Vec<String> = rows.iter().filter_map(|r| r.get::<_, Option<String>>(0)).map(|d| format!("{};", d.trim_end())).collect();
            return Ok((!defs.is_empty()).then(|| defs.join("\n\n")));
        }
        if !kind.is_relation() {
            return Ok(None);
        }
        let d = self.describe(schema, name).await?;
        if d.object.kind != ObjectKind::Table {
            return Ok(d.ddl);
        }
        // Names quoted the way Postgres prints them (only when needed).
        let head = self
            .client()
            .query_one(
                "select c.oid, format('%I.%I', n.nspname, c.relname) from pg_catalog.pg_class c \
                 join pg_catalog.pg_namespace n on n.oid = c.relnamespace where n.nspname = $1 and c.relname = $2",
                &[&schema, &name],
            )
            .await
            .map_err(|e| map_err(&e))?;
        let (oid, full): (u32, String) = (head.get(0), head.get(1));
        let cols = q(
            "select quote_ident(a.attname), pg_catalog.format_type(a.atttypid, a.atttypmod), a.attnotnull, \
                    pg_catalog.pg_get_expr(d.adbin, d.adrelid), a.attidentity::text, a.attgenerated::text, \
                    col_description(a.attrelid, a.attnum) \
             from pg_catalog.pg_attribute a left join pg_catalog.pg_attrdef d on d.adrelid = a.attrelid and d.adnum = a.attnum \
             where a.attrelid = $1 and a.attnum > 0 and not a.attisdropped order by a.attnum",
            oid,
        )
        .await?;
        let mut comments: Vec<(String, String)> = Vec::new();
        let mut lines: Vec<String> = cols
            .iter()
            .map(|r| {
                let col: String = r.get(0);
                let mut l = format!("    {col} {}", r.get::<_, String>(1));
                let expr: Option<String> = r.get(3);
                match (r.get::<_, String>(4).as_str(), r.get::<_, String>(5).as_str(), expr) {
                    ("a", _, _) => l.push_str(" GENERATED ALWAYS AS IDENTITY"),
                    ("d", _, _) => l.push_str(" GENERATED BY DEFAULT AS IDENTITY"),
                    (_, "s", Some(e)) => l.push_str(&format!(" GENERATED ALWAYS AS ({e}) STORED")),
                    (_, "v", Some(e)) => l.push_str(&format!(" GENERATED ALWAYS AS ({e}) VIRTUAL")),
                    (_, _, Some(e)) => l.push_str(&format!(" DEFAULT {e}")),
                    _ => {}
                }
                if r.get::<_, bool>(2) {
                    l.push_str(" NOT NULL");
                }
                if let Some(c) = r.get::<_, Option<String>>(6).filter(|c| !c.is_empty()) {
                    comments.push((col, c));
                }
                l
            })
            .collect();
        // Constraints (PK first; NOT NULL ones are inline above).
        for r in q(
            "select quote_ident(conname), pg_catalog.pg_get_constraintdef(oid, true) from pg_catalog.pg_constraint \
             where conrelid = $1 and contype <> 'n' order by case contype when 'p' then 0 when 'u' then 1 when 'f' then 2 else 3 end, conname",
            oid,
        )
        .await?
        {
            lines.push(format!("    CONSTRAINT {} {}", r.get::<_, String>(0), r.get::<_, String>(1)));
        }
        let mut ddl = format!("CREATE TABLE {full} (\n{}\n)", lines.join(",\n"));
        if let Some(pk) = q("select pg_catalog.pg_get_partkeydef($1)", oid).await?.first().and_then(|r| r.get::<_, Option<String>>(0)) {
            ddl.push_str(&format!("\nPARTITION BY {pk}"));
        }
        ddl.push(';');
        // Indexes not created by a constraint.
        for r in q(
            "select pg_catalog.pg_get_indexdef(i.indexrelid) from pg_catalog.pg_index i \
             where i.indrelid = $1 and not exists (select 1 from pg_catalog.pg_constraint c where c.conindid = i.indexrelid) \
             order by i.indexrelid",
            oid,
        )
        .await?
        {
            // Partitioned tables print `ON ONLY` (parent only); the DDL is
            // meant to recreate the table with its partitions' indexes.
            ddl.push_str(&format!("\n{};", r.get::<_, String>(0).replacen(" ON ONLY ", " ON ", 1)));
        }
        let lit = databrain_connector_core::quote_literal;
        if let Some(c) = d.object.comment.as_deref().filter(|c| !c.is_empty()) {
            ddl.push_str(&format!("\n\nCOMMENT ON TABLE {full} IS {};", lit(c)));
        }
        for (col, cm) in &comments {
            ddl.push_str(&format!("\nCOMMENT ON COLUMN {full}.{col} IS {};", lit(cm)));
        }
        Ok(Some(ddl))
    }

    async fn list_objects(&self, schema: &str) -> Result<Vec<DbObject>> {
        self.list_objects_filtered(schema, None).await
    }

    async fn list_objects_named(&self, schema: &str, names: &[String]) -> Result<Vec<DbObject>> {
        self.list_objects_filtered(schema, Some(names)).await
    }

    /// Everything `list_objects` lists, per schema: relations and sequences
    /// (row version + columns + constraints + comments) and routines (all
    /// overloads), so any DDL on any of them changes the value. Row
    /// estimates count coarsely, by power of two (`reltuples` changes with
    /// every ANALYZE): a table that doubles or halves changes it, so the
    /// listing's and the AI's estimates are read again then.
    async fn explorer_fingerprints(&self) -> Result<Option<std::collections::HashMap<String, String>>> {
        // Redshift: no xmin / hashtext on its catalog; schemas are listed again.
        if self.redshift {
            return Ok(None);
        }
        let rows = self
            .client()
            .query(
                "with rel as (select c.oid, c.relnamespace ns, c.xmin, c.relname, c.reltuples from pg_catalog.pg_class c \
                              where c.relkind in ('r','p','v','m','f','S') and not c.relispartition), \
                 h as ( \
                   select ns, hashtext(oid::text || ':' || xmin::text || ':' || relname) h from rel \
                   union all select ns, hashtext(oid::text || ':rows:' || floor(log(2, greatest(reltuples, 0)::numeric + 1))::text) from rel \
                   union all select r.ns, hashtext(a.attrelid::text || ':' || a.attnum || ':' || a.xmin::text) \
                     from pg_catalog.pg_attribute a join rel r on r.oid = a.attrelid where a.attnum > 0 \
                   union all select r.ns, hashtext(k.oid::text || ':' || k.xmin::text) from pg_catalog.pg_constraint k join rel r on r.oid = k.conrelid \
                   union all select r.ns, hashtext(d.objoid::text || ':' || d.objsubid || ':' || md5(d.description)) \
                     from pg_catalog.pg_description d join rel r on r.oid = d.objoid where d.classoid = 'pg_catalog.pg_class'::regclass \
                   union all select p.pronamespace, hashtext(p.oid::text || ':' || p.xmin::text) from pg_catalog.pg_proc p where p.prokind in ('f','p')) \
                 select n.nspname::text, count(h.h) || '/' || coalesce(sum(h.h), 0) \
                 from pg_catalog.pg_namespace n left join h on h.ns = n.oid group by n.nspname",
                &[],
            )
            .await
            .map_err(|e| map_err(&e))?;
        Ok(Some(rows.iter().map(|r| (r.get::<_, String>(0), r.get::<_, String>(1))).collect()))
    }

    async fn object_versions(&self, schema: &str) -> Result<Option<std::collections::HashMap<String, String>>> {
        if self.redshift {
            let _ = schema;
            return Ok(None);
        }
        let rows = self
            .client()
            .query(
                "select case c.relkind when 'v' then 'view' when 'm' then 'materialized_view' when 'f' then 'foreign_table' \
                          when 'S' then 'sequence' else 'table' end || ':' || c.relname, \
                        concat_ws(':', c.xmin::text, \
                          (select sum(hashtext(a.attnum || ':' || a.xmin::text)) from pg_catalog.pg_attribute a where a.attrelid = c.oid and a.attnum > 0), \
                          (select sum(hashtext(k.oid::text || ':' || k.xmin::text)) from pg_catalog.pg_constraint k where k.conrelid = c.oid), \
                          (select sum(hashtext(d.objsubid || ':' || md5(d.description))) from pg_catalog.pg_description d \
                             where d.objoid = c.oid and d.classoid = 'pg_catalog.pg_class'::regclass)) \
                 from pg_catalog.pg_class c join pg_catalog.pg_namespace n on n.oid = c.relnamespace \
                 where n.nspname = $1 and c.relkind in ('r','p','v','m','f','S') and not c.relispartition \
                 union all \
                 select case p.prokind when 'p' then 'procedure' else 'function' end || ':' || p.proname, \
                        sum(hashtext(p.oid::text || ':' || p.xmin::text))::text \
                 from pg_catalog.pg_proc p join pg_catalog.pg_namespace n on n.oid = p.pronamespace \
                 where n.nspname = $1 and p.prokind in ('f','p') and n.nspname not in ('pg_catalog','information_schema') \
                 group by p.proname, p.prokind",
                &[&schema],
            )
            .await
            .map_err(|e| map_err(&e))?;
        Ok(Some(rows.iter().map(|r| (r.get::<_, String>(0), r.get::<_, String>(1))).collect()))
    }

    async fn table_layout(&self, schema: &str, name: &str) -> Result<databrain_connector_core::TableLayout> {
        if self.redshift {
            return self.rs_table_layout(schema, name).await;
        }
        use databrain_connector_core::{TableLayout, group_indexes, parse_call_list, query_text, quote_literal, truthy};
        let (s, t) = (quote_literal(schema), quote_literal(name));
        let idx = query_text(
            self,
            &format!(
                "select i.relname::text, coalesce(a.attname::text, pg_catalog.pg_get_indexdef(ix.indexrelid, k.n::int, true)), \
                        ix.indisunique, ix.indisprimary, am.amname::text \
                 from pg_catalog.pg_index ix \
                 join pg_catalog.pg_class c on c.oid = ix.indrelid \
                 join pg_catalog.pg_namespace ns on ns.oid = c.relnamespace \
                 join pg_catalog.pg_class i on i.oid = ix.indexrelid \
                 join pg_catalog.pg_am am on am.oid = i.relam \
                 cross join lateral unnest(ix.indkey::int2[]) with ordinality as k(attnum, n) \
                 left join pg_catalog.pg_attribute a on a.attrelid = c.oid and a.attnum = k.attnum and k.attnum > 0 \
                 where ns.nspname = {s} and c.relname = {t} and k.n <= ix.indnkeyatts \
                 order by ix.indisprimary desc, i.relname, k.n"
            ),
        )
        .await?;
        let mut l = TableLayout {
            indexes: group_indexes(idx.into_iter().map(|r| {
                let m = r[4].clone().filter(|m| m != "btree");
                (r[0].clone().unwrap_or_default(), r[1].clone().unwrap_or_default(), truthy(&r[2]), truthy(&r[3]), m)
            })),
            ..Default::default()
        };
        let meta = query_text(
            self,
            &format!(
                "select case when c.relkind = 'p' then pg_catalog.pg_get_partkeydef(c.oid) end, c.reltuples::bigint, \
                        (select count(*) from pg_catalog.pg_inherits h where h.inhparent = c.oid), \
                        pg_catalog.pg_total_relation_size(c.oid) \
                 from pg_catalog.pg_class c join pg_catalog.pg_namespace n on n.oid = c.relnamespace \
                 where n.nspname = {s} and c.relname = {t}"
            ),
        )
        .await?;
        if let Some(r) = meta.first() {
            if let Some(def) = &r[0] {
                let (kind, cols) = parse_call_list(def);
                l.partition_by = cols;
                l.partition_kind = (!kind.is_empty()).then_some(kind);
                if let Some(n) = &r[2] {
                    l.notes.push(format!("{n} partitions"));
                }
            }
            l.row_estimate = r[1].as_deref().and_then(|x| x.parse().ok()).filter(|n: &i64| *n >= 0);
            l.size_bytes = r.get(3).and_then(|x| x.as_deref()).and_then(|x| x.parse().ok()).filter(|n: &i64| *n > 0);
        }
        Ok(l)
    }

    async fn describe(&self, schema: &str, name: &str) -> Result<ObjectDetail> {
        if self.redshift {
            return self.rs_describe(schema, name).await;
        }
        let obj = self
            .client()
            .query_opt(
                "select c.oid, c.relkind::text, obj_description(c.oid, 'pg_class'), c.reltuples::float8 \
                 from pg_catalog.pg_class c join pg_catalog.pg_namespace n on n.oid = c.relnamespace \
                 where n.nspname = $1 and c.relname = $2",
                &[&schema, &name],
            )
            .await
            .map_err(|e| map_err(&e))?
            .ok_or_else(|| ConnectorError::query(format!("object not found: {schema}.{name}")))?;
        let oid: u32 = obj.get(0);
        let relkind: String = obj.get(1);
        let kind = match relkind.as_str() {
            "v" => ObjectKind::View,
            "m" => ObjectKind::MaterializedView,
            "f" => ObjectKind::ForeignTable,
            _ => ObjectKind::Table,
        };

        let cols = self
            .client()
            .query(
                "select a.attname::text, pg_catalog.format_type(a.atttypid, a.atttypmod), \
                        not a.attnotnull, \
                        coalesce(a.attnum = any(i.indkey), false), \
                        pg_catalog.pg_get_expr(d.adbin, d.adrelid), \
                        col_description(a.attrelid, a.attnum) \
                 from pg_catalog.pg_attribute a \
                 left join pg_catalog.pg_attrdef d on d.adrelid = a.attrelid and d.adnum = a.attnum \
                 left join pg_catalog.pg_index i on i.indrelid = a.attrelid and i.indisprimary \
                 where a.attrelid = $1 and a.attnum > 0 and not a.attisdropped \
                 order by a.attnum",
                &[&oid],
            )
            .await
            .map_err(|e| map_err(&e))?;
        let columns: Vec<ColumnInfo> = cols
            .iter()
            .map(|r| ColumnInfo { name: r.get(0), data_type: r.get(1), nullable: r.get(2), is_primary_key: r.get(3), default: r.get(4), comment: r.get(5) })
            .collect();

        let ddl = match kind {
            ObjectKind::View | ObjectKind::MaterializedView => {
                let def: Option<String> =
                    self.client().query_one("select pg_catalog.pg_get_viewdef($1::oid, true)", &[&oid]).await.map_err(|e| map_err(&e))?.get(0);
                let kw = if kind == ObjectKind::View { "VIEW" } else { "MATERIALIZED VIEW" };
                def.map(|d| format!("CREATE {kw} {}.{} AS\n{}", quote(schema), quote(name), d.trim_end()))
            }
            _ => Some(table_ddl(schema, name, &columns)),
        };

        let est: Option<f64> = obj.get(3);
        let foreign_keys = self.foreign_keys(schema, Some(name)).await?.into_iter().map(|(_, f)| f).collect();
        Ok(ObjectDetail {
            object: DbObject {
                schema: schema.to_string(),
                name: name.to_string(),
                kind,
                comment: obj.get(2),
                row_estimate: est.filter(|e| *e >= 0.0).map(|e| e as i64),
            },
            columns,
            ddl,
            foreign_keys,
        })
    }

    async fn bulk_metadata(&self, schemas: &[String]) -> Result<Vec<databrain_connector_core::SchemaMetadata>> {
        if self.redshift {
            return databrain_connector_core::default_bulk_metadata(self, schemas).await;
        }
        match self.bulk_metadata_batched(schemas).await {
            Ok(m) => Ok(m),
            // E.g. no privilege on one catalog view: schema by schema reports per-schema errors.
            Err(_) => databrain_connector_core::default_bulk_metadata(self, schemas).await,
        }
    }

    async fn schema_columns(&self, schema: &str) -> Result<Vec<TableColumns>> {
        if self.redshift {
            return self.rs_schema_columns(schema).await;
        }
        let rows = self
            .client()
            .query(
                "select c.relname::text, a.attname::text, pg_catalog.format_type(a.atttypid, a.atttypmod), \
                        not a.attnotnull, coalesce(a.attnum = any(i.indkey), false), \
                        col_description(a.attrelid, a.attnum) \
                 from pg_catalog.pg_attribute a \
                 join pg_catalog.pg_class c on c.oid = a.attrelid \
                 join pg_catalog.pg_namespace n on n.oid = c.relnamespace \
                 left join pg_catalog.pg_index i on i.indrelid = a.attrelid and i.indisprimary \
                 where n.nspname = $1 and c.relkind in ('r','p','v','m','f') and not c.relispartition \
                   and a.attnum > 0 and not a.attisdropped \
                 order by c.relname, a.attnum",
                &[&schema],
            )
            .await
            .map_err(|e| map_err(&e))?;
        let mut out: Vec<TableColumns> = Vec::new();
        for r in rows {
            let table: String = r.get(0);
            let col = ColumnInfo { name: r.get(1), data_type: r.get(2), nullable: r.get(3), is_primary_key: r.get(4), default: None, comment: r.get(5) };
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
mod tests;
