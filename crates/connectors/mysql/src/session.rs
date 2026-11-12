//! The MySQL session: query execution and schema introspection.

use crate::error::map_err;
use crate::exec::Ctx;
use crate::exec::run;
use crate::exec::settle_kill;
use async_trait::async_trait;
use databrain_connector_core::ColumnInfo;
use databrain_connector_core::ConnState;
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
use mysql_async::Conn;
use mysql_async::Opts;
use mysql_async::consts::StatusFlags;
use mysql_async::prelude::*;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::sync::Mutex;

pub struct MysqlSession {
    pub(crate) conn: Arc<Mutex<Conn>>,
    /// Thread id of `conn` (it changes when the connection is replaced).
    pub(crate) conn_id: Arc<AtomicU32>,
    pub(crate) opts: Opts,
    /// What the connection carries (transactions, SET, USE, temp tables).
    pub(crate) state: Arc<ConnState>,
    /// Read-only connection: read-only is set again before each statement
    /// (a stored function could have changed it for the session).
    pub(crate) read_only: bool,
}

impl MysqlSession {
    /// Kill the running query using a short-lived side connection.
    pub(crate) async fn kill_query(opts: Opts, id: u32) {
        if let Ok(Ok(mut c)) = tokio::time::timeout(Duration::from_secs(5), Conn::new(opts)).await {
            let _ = c.query_drop(format!("KILL QUERY {id}")).await;
            let _ = c.disconnect().await;
        }
    }

    pub(crate) async fn query_rows<T: FromRow + Send + 'static>(&self, sql: &str, params: Vec<mysql_async::Value>) -> Result<Vec<T>> {
        let mut c = self.conn.lock().await;
        c.exec(sql, params).await.map_err(map_err)
    }
}

#[async_trait]
impl Session for MysqlSession {
    fn kind(&self) -> ConnectorKind {
        ConnectorKind::Mysql
    }

    async fn server_version(&self) -> Result<String> {
        let mut c = self.conn.lock().await;
        let v: Option<String> = c.query_first("select version()").await.map_err(map_err)?;
        Ok(v.unwrap_or_default())
    }

    fn transaction_state(&self) -> databrain_connector_core::TxState {
        use databrain_connector_core::TxState;
        // Also one a procedure or `SET autocommit = 0` opened (the server
        // reports it after each statement).
        if self.state.in_transaction()
            || self.conn.try_lock().is_ok_and(|c| c.last_ok_packet().is_some_and(|p| p.status_flags().contains(StatusFlags::SERVER_STATUS_IN_TRANS)))
        {
            TxState::Open
        } else {
            TxState::None
        }
    }

    async fn ping(&self) -> Result<()> {
        self.conn.lock().await.ping().await.map_err(map_err)
    }

    async fn execute(&self, sql: &str, opts: ExecOptions) -> Result<QueryStream> {
        let (tx, stream) = QueryStream::channel(4);
        let ctx = Ctx {
            conn: self.conn.clone(),
            conn_id: self.conn_id.clone(),
            opts: self.opts.clone(),
            state: self.state.clone(),
            finished: Arc::new(AtomicBool::new(false)),
            read_only: self.read_only,
        };
        let sql = sql.to_string();
        tokio::spawn(async move {
            let cancel = opts.cancel.clone();
            let work = run(&ctx, sql, opts, tx.clone());
            tokio::pin!(work);
            let res = tokio::select! {
                r = &mut work => r,
                _ = cancel.cancelled() => {
                    // Nothing to stop when the statement is done with the
                    // server (e.g. stopped at the row limit just now).
                    let kill = !ctx.finished.load(Ordering::SeqCst);
                    if kill {
                        Self::kill_query(ctx.opts.clone(), ctx.conn_id.load(Ordering::SeqCst)).await;
                    }
                    let r = match tokio::time::timeout(Duration::from_secs(10), &mut work).await {
                        Ok(Ok(())) => Ok(()),
                        _ => Err(ConnectorError::cancelled()),
                    };
                    if kill {
                        settle_kill(&mut *ctx.conn.lock().await).await;
                    }
                    r
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
        let rows: Vec<(String, Option<String>)> =
            self.query_rows("select schema_name, database() from information_schema.schemata order by schema_name", vec![]).await?;
        let mut out: Vec<SchemaInfo> =
            rows.into_iter().map(|(name, cur)| SchemaInfo { is_default: cur.as_deref() == Some(name.as_str()), name, catalog: None }).collect();
        out.sort_by_key(|s| !s.is_default);
        Ok(out)
    }

    async fn list_objects(&self, schema: &str) -> Result<Vec<DbObject>> {
        let rows: Vec<(String, String, Option<String>, Option<u64>)> = self
            .query_rows(
                "select table_name, table_type, table_comment, table_rows \
                 from information_schema.tables where table_schema = ? \
                 order by table_type <> 'BASE TABLE', table_name",
                vec![schema.into()],
            )
            .await?;
        let mut out: Vec<DbObject> = rows
            .into_iter()
            .map(|(name, t, comment, rows)| DbObject {
                schema: schema.to_string(),
                name,
                kind: if t.contains("VIEW") { ObjectKind::View } else { ObjectKind::Table },
                comment: comment.filter(|c| !c.is_empty()),
                row_estimate: rows.map(|r| r as i64),
            })
            .collect();
        let routines: Vec<(String, String)> = self
            .query_rows(
                "select routine_name, routine_type from information_schema.routines \
                 where routine_schema = ? order by routine_name",
                vec![schema.into()],
            )
            .await?;
        out.extend(routines.into_iter().map(|(name, t)| DbObject {
            schema: schema.to_string(),
            name,
            kind: if t == "PROCEDURE" { ObjectKind::Procedure } else { ObjectKind::Function },
            comment: None,
            row_estimate: None,
        }));
        Ok(out)
    }

    async fn table_layout(&self, schema: &str, name: &str) -> Result<databrain_connector_core::TableLayout> {
        use databrain_connector_core::{TableLayout, group_indexes, query_text, quote_literal, truthy};
        let (s, t) = (quote_literal(schema), quote_literal(name));
        let idx = query_text(
            self,
            &format!(
                "select index_name, coalesce(column_name, '(expression)'), case when non_unique = 0 then 1 else 0 end, \
                        case when index_name = 'PRIMARY' then 1 else 0 end, index_type \
                 from information_schema.statistics where table_schema = {s} and table_name = {t} \
                 order by index_name = 'PRIMARY' desc, index_name, seq_in_index"
            ),
        )
        .await?;
        let mut l = TableLayout {
            indexes: group_indexes(idx.into_iter().map(|r| {
                let m = r[4].clone().map(|m| m.to_ascii_lowercase()).filter(|m| m != "btree");
                (r[0].clone().unwrap_or_default(), r[1].clone().unwrap_or_default(), truthy(&r[2]), truthy(&r[3]), m)
            })),
            ..Default::default()
        };
        if let Ok(p) = query_text(
            self,
            &format!(
                "select partition_method, partition_expression, count(*) from information_schema.partitions \
                 where table_schema = {s} and table_name = {t} and partition_name is not null group by 1, 2"
            ),
        )
        .await
            && let Some(r) = p.first()
        {
            l.partition_kind = r[0].as_deref().map(|m| m.to_ascii_lowercase());
            l.partition_by = r[1]
                .as_deref()
                .map(|e| databrain_connector_core::split_key_list(e).into_iter().map(|x| databrain_connector_core::unquote_ident(&x)).collect())
                .unwrap_or_default();
            if let Some(n) = &r[2] {
                l.notes.push(format!("{n} partitions"));
            }
        }
        if let Ok(r) = query_text(
            self,
            &format!("select table_rows, data_length + index_length from information_schema.tables where table_schema = {s} and table_name = {t}"),
        )
        .await
        {
            l.row_estimate = r.first().and_then(|r| r[0].as_deref()).and_then(|x| x.parse().ok());
            l.size_bytes = r.first().and_then(|r| r.get(1).cloned().flatten()).and_then(|x| x.parse().ok());
        }
        Ok(l)
    }

    /// Tables and views: SHOW CREATE (via describe). Routines: SHOW CREATE FUNCTION/PROCEDURE.
    async fn object_ddl(&self, schema: &str, name: &str, kind: ObjectKind) -> Result<Option<String>> {
        let what = match kind {
            ObjectKind::Function => "FUNCTION",
            ObjectKind::Procedure => "PROCEDURE",
            k if k.is_relation() => return Ok(self.describe(schema, name).await?.ddl),
            _ => return Ok(None),
        };
        let show = format!("SHOW CREATE {what} {}.{}", quote_ident(ConnectorKind::Mysql, schema), quote_ident(ConnectorKind::Mysql, name));
        let mut c = self.conn.lock().await;
        let row: Option<mysql_async::Row> = c.query_first(show).await.map_err(map_err)?;
        // Columns: name, sql_mode, Create Function|Procedure, …
        Ok(row.and_then(|mut r| r.take::<Option<String>, _>(2).flatten()).map(|d| format!("{d};")))
    }

    async fn describe(&self, schema: &str, name: &str) -> Result<ObjectDetail> {
        let meta: Vec<(String, Option<String>, Option<u64>)> = self
            .query_rows(
                "select table_type, table_comment, table_rows from information_schema.tables \
                 where table_schema = ? and table_name = ?",
                vec![schema.into(), name.into()],
            )
            .await?;
        let (ttype, comment, rows) = meta.into_iter().next().ok_or_else(|| ConnectorError::query(format!("object not found: {schema}.{name}")))?;
        let is_view = ttype.contains("VIEW");

        #[allow(clippy::type_complexity)]
        let cols: Vec<(String, String, String, String, Option<String>, Option<String>)> = self
            .query_rows(
                "select column_name, column_type, is_nullable, column_key, column_default, column_comment \
                 from information_schema.columns where table_schema = ? and table_name = ? \
                 order by ordinal_position",
                vec![schema.into(), name.into()],
            )
            .await?;
        let columns = cols
            .into_iter()
            .map(|(n, t, nullable, key, default, comment)| ColumnInfo {
                name: n,
                data_type: t,
                nullable: nullable == "YES",
                is_primary_key: key == "PRI",
                default,
                comment: comment.filter(|c| !c.is_empty()),
            })
            .collect();

        let show = format!(
            "SHOW CREATE {} {}.{}",
            if is_view { "VIEW" } else { "TABLE" },
            quote_ident(ConnectorKind::Mysql, schema),
            quote_ident(ConnectorKind::Mysql, name)
        );
        let ddl: Option<String> = {
            let mut c = self.conn.lock().await;
            let row: Option<mysql_async::Row> = c.query_first(show).await.map_err(map_err)?;
            row.and_then(|mut r| r.take::<String, _>(1))
        };

        let foreign_keys = self.foreign_keys(schema, Some(name)).await?.into_iter().map(|(_, f)| f).collect();
        Ok(ObjectDetail {
            foreign_keys,
            object: DbObject {
                schema: schema.to_string(),
                name: name.to_string(),
                kind: if is_view { ObjectKind::View } else { ObjectKind::Table },
                comment: comment.filter(|c| !c.is_empty()),
                row_estimate: rows.map(|r| r as i64),
            },
            columns,
            ddl,
        })
    }

    /// Per-schema checksum of table names/types/comments, column definitions
    /// and foreign keys: three aggregate queries over information_schema.
    /// Row counts are left out, so data changes do not count.
    /// [`Session::schema_fingerprints`] plus routines (the explorer lists them).
    async fn explorer_fingerprints(&self) -> Result<Option<std::collections::HashMap<String, String>>> {
        let Some(mut out) = self.schema_fingerprints().await? else { return Ok(None) };
        let rows: Vec<(String, String)> = self
            .query_rows(
                "select routine_schema, cast(concat('r', count(*), ':', sum(crc32(concat_ws(':', routine_name, routine_type, created, last_altered)))) as char) \
                 from information_schema.routines group by routine_schema",
                vec![],
            )
            .await?;
        for (sch, f) in rows {
            let e = out.entry(sch).or_default();
            e.push('/');
            e.push_str(&f);
        }
        Ok(Some(out))
    }

    async fn object_versions(&self, schema: &str) -> Result<Option<std::collections::HashMap<String, String>>> {
        let rows: Vec<(String, String)> = self
            .query_rows(
                "select concat(if(t.table_type like '%VIEW%', 'view', 'table'), ':', t.table_name), \
                        cast(concat_ws(':', coalesce(t.create_time, ''), crc32(coalesce(t.table_comment, '')), coalesce(c.h, 0), coalesce(k.h, 0)) as char) \
                 from information_schema.tables t \
                 left join (select table_name, sum(crc32(concat_ws(':', column_name, ordinal_position, column_type, is_nullable, column_key, \
                                   column_comment, coalesce(column_default, '')))) h \
                            from information_schema.columns where table_schema = ? group by table_name) c on c.table_name = t.table_name \
                 left join (select table_name, sum(crc32(concat_ws(':', constraint_name, column_name, coalesce(referenced_table_name, ''), \
                                   coalesce(referenced_column_name, '')))) h \
                            from information_schema.key_column_usage where table_schema = ? group by table_name) k on k.table_name = t.table_name \
                 where t.table_schema = ? \
                 union all \
                 select concat(if(routine_type = 'PROCEDURE', 'procedure', 'function'), ':', routine_name), cast(concat_ws(':', created, last_altered) as char) \
                 from information_schema.routines where routine_schema = ?",
                vec![schema.into(), schema.into(), schema.into(), schema.into()],
            )
            .await?;
        Ok(Some(rows.into_iter().collect()))
    }

    async fn schema_fingerprints(&self) -> Result<Option<std::collections::HashMap<String, String>>> {
        let mut out: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        for sql in [
            "select table_schema, concat('t', count(*), ':', coalesce(sum(crc32(concat_ws(':', table_name, table_type, table_comment))), 0)) \
             from information_schema.tables group by table_schema",
            "select table_schema, concat('c', count(*), ':', coalesce(sum(crc32(concat_ws(':', table_name, column_name, ordinal_position, column_type, is_nullable, column_key, column_comment))), 0)) \
             from information_schema.columns group by table_schema",
            "select table_schema, concat('f', count(*), ':', coalesce(sum(crc32(concat_ws(':', table_name, constraint_name, column_name, referenced_table_schema, referenced_table_name, referenced_column_name))), 0)) \
             from information_schema.key_column_usage where referenced_table_name is not null group by table_schema",
        ] {
            let rows: Vec<(String, String)> = self.query_rows(sql, vec![]).await?;
            for (s, f) in rows {
                let e = out.entry(s).or_default();
                e.push('/');
                e.push_str(&f);
            }
        }
        Ok(Some(out))
    }

    /// Three information_schema queries for the whole batch of schemas.
    async fn bulk_metadata(&self, schemas: &[String]) -> Result<Vec<databrain_connector_core::SchemaMetadata>> {
        match self.bulk_metadata_batched(schemas).await {
            Ok(m) => Ok(m),
            Err(_) => databrain_connector_core::default_bulk_metadata(self, schemas).await,
        }
    }

    async fn schema_columns(&self, schema: &str) -> Result<Vec<TableColumns>> {
        #[allow(clippy::type_complexity)]
        let rows: Vec<(String, String, String, String, String, Option<String>)> = self
            .query_rows(
                "select table_name, column_name, column_type, is_nullable, column_key, column_comment \
                 from information_schema.columns where table_schema = ? order by table_name, ordinal_position",
                vec![schema.into()],
            )
            .await?;
        let mut out: Vec<TableColumns> = Vec::new();
        for (table, name, t, nullable, key, comment) in rows {
            let col = ColumnInfo {
                name,
                data_type: t,
                nullable: nullable == "YES",
                is_primary_key: key == "PRI",
                default: None,
                comment: comment.filter(|c| !c.is_empty()),
            };
            match out.last_mut() {
                Some(x) if x.table == table => x.columns.push(col),
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
    use crate::MysqlConnector;
    use databrain_connector_core::ConnectionConfig;
    use databrain_connector_core::Connector;

    /// Live: explorer fingerprints include routines; per-object versions.
    #[ignore = "live: needs DATABRAIN_MYSQL_HOST (cargo test -- --ignored)"]
    #[tokio::test]
    async fn live_explorer_versions() {
        let Ok(host) = std::env::var("DATABRAIN_MYSQL_HOST") else {
            panic!("live test: DATABRAIN_MYSQL_HOST not set (run with --ignored)");
        };
        use databrain_auth::{AuthMethod, InlineCredentialSource};
        let user = std::env::var("DATABRAIN_MYSQL_USER").unwrap_or_else(|_| "root".into());
        let mut cfg = ConnectionConfig::new(ConnectorKind::Mysql, AuthMethod::Password { user: user.clone() });
        cfg.host = Some(host);
        cfg.port = std::env::var("DATABRAIN_MYSQL_PORT").ok().and_then(|p| p.parse().ok());
        let creds = Arc::new(InlineCredentialSource::new(
            AuthMethod::Password { user },
            std::env::var("DATABRAIN_MYSQL_PASSWORD").ok().map(secrecy::SecretString::from),
        ));
        let s = MysqlConnector.connect(&cfg, creds).await.unwrap();
        let run = |sql: &'static str| {
            let s = &s;
            async move { s.execute(sql, ExecOptions::default()).await.unwrap().collect().await.unwrap() }
        };
        run("drop database if exists ex_a").await;
        run("create database ex_a").await;
        run("create table ex_a.t1 (id int primary key)").await;
        run("create table ex_a.t2 (x int)").await;
        run("create view ex_a.v1 as select id from ex_a.t1").await;
        let fp = |m: std::collections::HashMap<String, String>| m.get("ex_a").cloned().unwrap();
        let f0 = fp(s.explorer_fingerprints().await.unwrap().unwrap());
        let v0 = s.object_versions("ex_a").await.unwrap().unwrap();
        let mut keys: Vec<&str> = v0.keys().map(String::as_str).collect();
        keys.sort();
        assert_eq!(keys, vec!["table:t1", "table:t2", "view:v1"]);
        let mut listed: Vec<String> = s.list_objects("ex_a").await.unwrap().iter().map(|o| databrain_connector_core::version_key(o.kind, &o.name)).collect();
        listed.sort();
        assert_eq!(listed, keys);
        run("create function ex_a.f(a int) returns int deterministic return a").await;
        assert_ne!(f0, fp(s.explorer_fingerprints().await.unwrap().unwrap()), "new function");
        run("alter table ex_a.t2 add column z text comment 'zed'").await;
        let v1 = s.object_versions("ex_a").await.unwrap().unwrap();
        assert!(v1.contains_key("function:f"));
        assert_ne!(v0["table:t2"], v1["table:t2"]);
        assert_eq!(v0["table:t1"], v1["table:t1"]);
        run("insert into ex_a.t2 values (1, 'a')").await;
        assert_eq!(v1, s.object_versions("ex_a").await.unwrap().unwrap(), "data changes are not definition changes");
        run("drop database ex_a").await;
    }

    /// Live test: set DATABRAIN_MYSQL_HOST (+ _USER, _PASSWORD, _DB).
    #[ignore = "live: needs DATABRAIN_MYSQL_HOST (cargo test -- --ignored)"]
    #[tokio::test]
    async fn live_roundtrip() {
        let Ok(host) = std::env::var("DATABRAIN_MYSQL_HOST") else {
            panic!("live test: DATABRAIN_MYSQL_HOST not set (run with --ignored)");
        };
        use databrain_auth::{AuthMethod, InlineCredentialSource};
        let user = std::env::var("DATABRAIN_MYSQL_USER").unwrap_or_else(|_| "root".into());
        let mut cfg = ConnectionConfig::new(ConnectorKind::Mysql, AuthMethod::Password { user: user.clone() });
        cfg.host = Some(host);
        cfg.database = std::env::var("DATABRAIN_MYSQL_DB").ok();
        cfg.port = std::env::var("DATABRAIN_MYSQL_PORT").ok().and_then(|p| p.parse().ok());
        let creds = Arc::new(InlineCredentialSource::new(
            AuthMethod::Password { user },
            std::env::var("DATABRAIN_MYSQL_PASSWORD").ok().map(secrecy::SecretString::from),
        ));
        let s = MysqlConnector.connect(&cfg, creds).await.unwrap();
        let r = s.execute("select 1 as a, 'x' as b, now() as c, 1.5 as d", ExecOptions::default()).await.unwrap().collect().await.unwrap();
        assert_eq!(r.num_rows(), 1);
        let cancel = databrain_connector_core::CancellationToken::new();
        let stream = s.execute("select sleep(30)", ExecOptions { batch_size: 10, cancel: cancel.clone(), ..Default::default() }).await.unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        cancel.cancel();
        let _ = stream.collect().await;
        assert!(!s.list_schemas().await.unwrap().is_empty());
        if let Ok(db) = std::env::var("DATABRAIN_MYSQL_DB") {
            let run = |sql: &'static str| s.execute(sql, ExecOptions::default());
            run("drop table if exists databrain_layout_t").await.unwrap().collect().await.unwrap();
            run("create table databrain_layout_t (id int, day date, cust varchar(20), primary key (id, day), key i_cust (cust, day)) partition by range columns (day) (partition p0 values less than ('2030-01-01'))")
                .await.unwrap().collect().await.unwrap();
            let l = s.table_layout(&db, "databrain_layout_t").await.unwrap();
            run("drop table databrain_layout_t").await.unwrap().collect().await.unwrap();
            assert!(l.indexes[0].primary && l.indexes[0].columns == vec!["id", "day"], "{l:?}");
            assert_eq!(l.indexes[1].columns, vec!["cust", "day"]);
            assert_eq!(l.partition_by, vec!["day"], "{l:?}");
            assert!(l.size_bytes.is_some_and(|b| b > 0), "InnoDB pages count even when empty: {l:?}");
        }
    }
}
