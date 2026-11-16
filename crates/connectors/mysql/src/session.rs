//! The MySQL session: query execution and schema introspection.

use crate::config::Opener;
use crate::error::map_err;
use crate::exec::Ctx;
use crate::exec::replace_conn;
use crate::exec::run;
use crate::exec::settle_kill;
use crate::exec::stuck_cancel;
use async_trait::async_trait;
use databrain_connector_core::ColumnInfo;
use databrain_connector_core::ConnState;
use databrain_connector_core::ConnectorError;
use databrain_connector_core::ConnectorKind;
use databrain_connector_core::DbObject;
use databrain_connector_core::ErrorKind;
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
    /// Opens the side connections (stop a statement, replace the
    /// connection), reading the credential each time.
    pub(crate) opener: Arc<Opener>,
    /// What the connection carries (transactions, SET, USE, temp tables).
    pub(crate) state: Arc<ConnState>,
    /// Read-only connection: read-only is set again before each statement
    /// (a stored function could have changed it for the session).
    pub(crate) read_only: bool,
}

impl MysqlSession {
    /// Tables and columns per schema (the explorer's fingerprint adds routines).
    async fn table_fingerprints(&self) -> Result<Option<std::collections::HashMap<String, String>>> {
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

    /// Kill the running query using a short-lived side connection. Its
    /// side connection is given 5 s; a statement it didn't stop is handled
    /// by the caller (see `execute`).
    pub(crate) async fn kill_query(opener: &Opener, id: u32) {
        if let Some(mut c) = opener.open(Duration::from_secs(5)).await {
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

    async fn execute(&self, stmt: databrain_connector_core::Statement<'_>, opts: ExecOptions) -> Result<QueryStream> {
        let sql = stmt.sql();
        let (tx, stream) = QueryStream::channel(4);
        let ctx = Ctx {
            conn: self.conn.clone(),
            conn_id: self.conn_id.clone(),
            opener: self.opener.clone(),
            state: self.state.clone(),
            finished: Arc::new(AtomicBool::new(false)),
            read_only: self.read_only,
        };
        let sql = sql.to_string();
        tokio::spawn(async move {
            let cancel = opts.cancel.clone();
            // Boxed so it can be dropped while it holds the connection.
            let mut work = Box::pin(run(&ctx, sql, opts, tx.clone()));
            let res = tokio::select! {
                r = &mut work => r,
                _ = cancel.cancelled() => {
                    // Nothing to stop when the statement is done with the
                    // server (e.g. stopped at the row limit just now).
                    let kill = !ctx.finished.load(Ordering::SeqCst);
                    if kill {
                        Self::kill_query(&ctx.opener, ctx.conn_id.load(Ordering::SeqCst)).await;
                    }
                    match tokio::time::timeout(Duration::from_secs(10), &mut work).await {
                        Ok(r) => {
                            if kill {
                                settle_kill(&mut *ctx.conn.lock().await).await;
                            }
                            match r {
                                Ok(()) => Ok(()),
                                Err(_) => Err(ConnectorError::cancelled()),
                            }
                        }
                        Err(_) => {
                            // The kill didn't stop it (it failed, or the
                            // server is slow to notice). The statement holds
                            // the connection: dropping it lets go, and a new
                            // connection takes the stuck one's place (a KILL
                            // from it ends the old one), so the session
                            // answers again instead of waiting on it for good.
                            drop(work);
                            let replaced = match tokio::time::timeout(Duration::from_secs(5), ctx.conn.lock()).await {
                                Ok(mut c) => replace_conn(&ctx, &mut c).await,
                                Err(_) => false,
                            };
                            Err(stuck_cancel(replaced))
                        }
                    }
                }
            };
            if let Err(e) = res {
                let e = if cancel.is_cancelled() && e.kind != ErrorKind::Cancelled { ConnectorError::cancelled() } else { e };
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
        use databrain_connector_core::{TableLayout, group_indexes, query_text, truthy};
        // MySQL reads `\` as an escape: a name holding `\'` must not end the literal.
        let lit = |v: &str| databrain_connector_core::sql::quote_string(v, ConnectorKind::Mysql);
        let (s, t) = (lit(schema), lit(name));
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
    /// Tables and columns (`table_fingerprints`) plus routines.
    async fn explorer_fingerprints(&self) -> Result<Option<std::collections::HashMap<String, String>>> {
        let Some(mut out) = self.table_fingerprints().await? else { return Ok(None) };
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
            async move { s.execute(databrain_connector_core::Statement::internal(sql), ExecOptions::default()).await.unwrap().collect().await.unwrap() }
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

    /// Live: the transaction state follows the server. DDL commits the open
    /// transaction by itself (BEGIN's text said one was open); a failed
    /// statement inside one, or a read, leaves it open.
    #[ignore = "live: needs DATABRAIN_MYSQL_HOST (cargo test -- --ignored)"]
    #[tokio::test]
    async fn live_transaction_state_follows_the_server() {
        use databrain_connector_core::TxState;
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
            async move {
                match s.execute(databrain_connector_core::Statement::internal(sql), ExecOptions::default()).await {
                    Ok(st) => st.collect().await.is_ok(),
                    Err(_) => false,
                }
            }
        };
        for sql in ["drop database if exists kb_tx", "create database kb_tx", "create table kb_tx.t (id int primary key) engine=innodb", "begin"] {
            assert!(run(sql).await, "{sql}");
        }
        // Still open: a write, a read and a failed statement keep it.
        assert!(run("insert into kb_tx.t values (1)").await);
        assert!(run("select * from kb_tx.t").await);
        assert!(!run("select * from kb_tx.missing").await);
        assert_eq!(s.transaction_state(), TxState::Open);
        // DDL commits it.
        assert!(run("create table kb_tx.u (x int)").await);
        assert_eq!(s.transaction_state(), TxState::None, "DDL committed the transaction");
        // A failed statement that names one: what the server says.
        assert!(!run("start transaction bogus").await);
        assert_eq!(s.transaction_state(), TxState::None);
        // Autocommit off: a write opens one, DDL ends it.
        assert!(run("set autocommit = 0").await);
        assert!(run("insert into kb_tx.t values (2)").await);
        assert_eq!(s.transaction_state(), TxState::Open);
        assert!(run("drop table kb_tx.u").await);
        assert_eq!(s.transaction_state(), TxState::None);
        assert!(run("set autocommit = 1").await);
        assert!(run("drop database kb_tx").await);
    }

    /// A live session as `user` (root's password from the environment when None).
    async fn live_as(user: Option<(&str, &str)>) -> Box<dyn Session> {
        use databrain_auth::{AuthMethod, InlineCredentialSource};
        let host = std::env::var("DATABRAIN_MYSQL_HOST").expect("live test: DATABRAIN_MYSQL_HOST not set (run with --ignored)");
        let (name, pw) = match user {
            Some((u, p)) => (u.to_string(), Some(p.to_string())),
            None => (std::env::var("DATABRAIN_MYSQL_USER").unwrap_or_else(|_| "root".into()), std::env::var("DATABRAIN_MYSQL_PASSWORD").ok()),
        };
        let mut cfg = ConnectionConfig::new(ConnectorKind::Mysql, AuthMethod::Password { user: name.clone() });
        cfg.host = Some(host);
        cfg.port = std::env::var("DATABRAIN_MYSQL_PORT").ok().and_then(|p| p.parse().ok());
        let creds = Arc::new(InlineCredentialSource::new(AuthMethod::Password { user: name }, pw.map(secrecy::SecretString::from)));
        MysqlConnector.connect(&cfg, creds).await.unwrap()
    }

    async fn live_run(s: &dyn Session, sql: &str, opts: ExecOptions) -> Result<databrain_connector_core::Collected> {
        s.execute(databrain_connector_core::Statement::internal(sql), opts).await?.collect().await
    }

    /// The root password from the environment, counting how often it is read.
    struct Counting(databrain_auth::InlineCredentialSource, std::sync::atomic::AtomicUsize);

    #[async_trait]
    impl databrain_auth::CredentialSource for Counting {
        async fn get(&self) -> std::result::Result<databrain_auth::Credential, databrain_auth::AuthError> {
            self.1.fetch_add(1, Ordering::SeqCst);
            self.0.get().await
        }
    }

    fn live_cfg() -> ConnectionConfig {
        let host = std::env::var("DATABRAIN_MYSQL_HOST").expect("live test: DATABRAIN_MYSQL_HOST not set (run with --ignored)");
        let user = std::env::var("DATABRAIN_MYSQL_USER").unwrap_or_else(|_| "root".into());
        let mut cfg = ConnectionConfig::new(ConnectorKind::Mysql, databrain_auth::AuthMethod::Password { user });
        cfg.host = Some(host);
        cfg.port = std::env::var("DATABRAIN_MYSQL_PORT").ok().and_then(|p| p.parse().ok());
        cfg
    }

    fn counting(cfg: &ConnectionConfig) -> Arc<Counting> {
        let user = match &cfg.auth {
            databrain_auth::AuthMethod::Password { user } => user.clone(),
            _ => unreachable!(),
        };
        let pw = std::env::var("DATABRAIN_MYSQL_PASSWORD").ok().map(secrecy::SecretString::from);
        Arc::new(Counting(databrain_auth::InlineCredentialSource::new(databrain_auth::AuthMethod::Password { user }, pw), Default::default()))
    }

    /// The credential isn't kept for the connections the session opens
    /// later: replacing a connection still receiving a result cut at the
    /// row limit reads it from its source again.
    #[ignore = "live: needs DATABRAIN_MYSQL_HOST (cargo test -- --ignored)"]
    #[tokio::test]
    async fn live_new_connections_read_the_credential_again() {
        let cfg = live_cfg();
        let creds = counting(&cfg);
        let s = MysqlConnector.connect(&cfg, creds.clone()).await.unwrap();
        assert_eq!(creds.1.load(Ordering::SeqCst), 1);
        let opts = ExecOptions { max_rows: Some(10), ..Default::default() };
        // Millions of rows, without changing the session (it stays replaceable).
        let sql = "select a.ordinal_position from information_schema.columns a, information_schema.columns b";
        let c = tokio::time::timeout(Duration::from_secs(30), live_run(&*s, sql, opts)).await.expect("in time").unwrap();
        assert_eq!(c.batches.iter().map(|b| b.num_rows()).sum::<usize>(), 10);
        assert!(creds.1.load(Ordering::SeqCst) >= 2, "the replacement connection read the credential");
        // And it works.
        tokio::time::timeout(Duration::from_secs(10), live_run(&*s, "select 1", ExecOptions::default())).await.expect("in time").unwrap();
    }

    /// A CA file: with the server's CA, Require connects (the chain is
    /// checked) and so does Verify (the chain and host name); with another
    /// CA both are refused, Prefer too (no plaintext retry on a certificate
    /// error). `DATABRAIN_MYSQL_CA`: the CA that signed the server's
    /// certificate, which names the host connected to (127.0.0.1);
    /// `DATABRAIN_MYSQL_OTHER_CA`: another one. (macOS refuses a server
    /// certificate valid longer than Apple allows for added CAs, like
    /// MySQL's generated 10-year one: use one of a year.)
    #[ignore = "live: needs DATABRAIN_MYSQL_HOST, DATABRAIN_MYSQL_CA and DATABRAIN_MYSQL_OTHER_CA (cargo test -- --ignored)"]
    #[tokio::test]
    async fn live_ca_file_verifies_the_server() {
        let ca = std::env::var("DATABRAIN_MYSQL_CA").expect("DATABRAIN_MYSQL_CA");
        let other = std::env::var("DATABRAIN_MYSQL_OTHER_CA").expect("DATABRAIN_MYSQL_OTHER_CA");
        let open = |mode: databrain_connector_core::SslMode, file: String| async move {
            let mut cfg = live_cfg();
            cfg.ssl_mode = mode;
            cfg.options.insert(crate::config::SSL_ROOT_CERT.into(), file);
            let creds = counting(&cfg);
            tokio::time::timeout(Duration::from_secs(20), MysqlConnector.connect(&cfg, creds)).await.expect("in time")
        };
        use databrain_connector_core::SslMode::*;
        let s = open(Require, ca.clone()).await.expect("Require with the server's CA");
        let cipher = live_run(&*s, "show session status like 'Ssl_cipher'", ExecOptions::default()).await.unwrap();
        let v = databrain_connector_core::arrow::util::display::array_value_to_string(cipher.batches[0].column(1), 0).unwrap();
        assert!(!v.is_empty(), "encrypted");
        assert!(open(VerifyFull, ca).await.is_ok(), "Verify with the server's CA and host");
        assert!(open(Require, other.clone()).await.is_err(), "Require with another CA refused");
        assert!(open(VerifyFull, other.clone()).await.is_err(), "Verify with another CA refused");
        // Prefer with a CA checks the chain too: no plaintext retry on a certificate error.
        assert!(open(Prefer, other).await.is_err(), "Prefer with another CA refused");
    }

    /// The row limit stops a read, but a procedure (not a read) runs to its
    /// end: its write after the rows happens, and a notice says the rest of
    /// its rows were discarded.
    #[ignore = "live: needs DATABRAIN_MYSQL_HOST (cargo test -- --ignored)"]
    #[tokio::test]
    async fn live_row_limit_cuts_reads_not_procedures() {
        let s = live_as(None).await;
        let ok = |sql: &'static str| {
            let s = &*s;
            async move { live_run(s, sql, ExecOptions::default()).await.unwrap_or_else(|e| panic!("{sql}: {e}")) }
        };
        ok("drop database if exists kb_stop").await;
        ok("create database kb_stop").await;
        ok("create table kb_stop.log (id int primary key auto_increment, note varchar(20))").await;
        ok("create procedure kb_stop.rows_then_write() begin \
              with recursive n(i) as (select 1 union all select i + 1 from n where i < 500) select i from n; \
              insert into kb_stop.log (note) values ('done'); end")
        .await;
        ok("set cte_max_recursion_depth = 10000000").await;
        let limit = ExecOptions { max_rows: Some(11), batch_size: 4, ..Default::default() };
        // A read: the first 11 rows.
        let r = live_run(&*s, "with recursive n(i) as (select 1 union all select i + 1 from n where i < 500000) select i from n", limit.clone()).await.unwrap();
        assert_eq!(r.num_rows(), 11);
        assert!(r.notices.is_empty(), "{:?}", r.notices);
        // A procedure: its rows cut at 11, its write done.
        let r = live_run(&*s, "call kb_stop.rows_then_write()", limit).await.unwrap();
        assert_eq!(r.num_rows(), 11);
        assert!(r.notices.iter().any(|n| n.contains("ran to its end")), "{:?}", r.notices);
        let r = ok("select count(*) from kb_stop.log where note = 'done'").await;
        assert_eq!(
            databrain_connector_core::arrow::util::display::array_value_to_string(r.batches[0].column(0), 0).unwrap(),
            "1",
            "the procedure's insert ran"
        );
        ok("drop database kb_stop").await;
    }

    /// Stop while rows stream sends KILL QUERY (the statement ends on the
    /// server) inside a transaction, and the session answers at once; when
    /// the kill can't be sent, the session is given a new connection after
    /// 10 s instead of waiting on the stuck one for good.
    #[ignore = "live: needs DATABRAIN_MYSQL_HOST (cargo test -- --ignored)"]
    #[tokio::test]
    async fn live_stop_ends_the_statement_and_never_blocks() {
        /// `f`, failing the test with `what` when it takes longer than `secs`
        /// (a session that blocks after Stop would otherwise hang the test).
        async fn step<T>(what: &str, secs: u64, f: impl std::future::Future<Output = T>) -> T {
            tokio::time::timeout(Duration::from_secs(secs), f).await.unwrap_or_else(|_| panic!("blocked for {secs} s: {what}"))
        }
        async fn ok(s: &dyn Session, sql: String) -> databrain_connector_core::Collected {
            step(&sql.clone(), 20, live_run(s, &sql, ExecOptions::default())).await.unwrap_or_else(|e| panic!("{sql}: {e}"))
        }
        fn cell(r: &databrain_connector_core::Collected) -> String {
            databrain_connector_core::arrow::util::display::array_value_to_string(r.batches[0].column(0), 0).unwrap()
        }
        /// Statements whose text holds `marker` running on the server.
        async fn running(s: &dyn Session, marker: &str) -> String {
            cell(&ok(s, format!("select count(*) from information_schema.processlist where info like '%{marker}%' and info not like '%processlist%'")).await)
        }
        let root = live_as(None).await;

        // Stop mid-stream, in a transaction (the connection is kept): KILL QUERY sent.
        let s = live_as(None).await;
        ok(&*s, "set cte_max_recursion_depth = 10000000".into()).await;
        ok(&*s, "begin".into()).await;
        let cancel = databrain_connector_core::CancellationToken::new();
        let mut st = s
            .execute(
                databrain_connector_core::Statement::internal("select /*kb_stream*/ i, sleep(0.001) from (with recursive n(i) as (select 1 union all select i + 1 from n where i < 2000000) select i from n) t"),
                ExecOptions { batch_size: 2, cancel: cancel.clone(), ..Default::default() },
            )
            .await
            .unwrap();
        let got = step("the first rows", 20, async {
            while let Some(ev) = st.next().await {
                if matches!(ev.as_ref().unwrap_or_else(|e| panic!("{e}")), databrain_connector_core::StreamEvent::Batch(_)) {
                    return true;
                }
            }
            false
        })
        .await;
        assert!(got, "rows were streaming");
        assert_eq!(running(&*root, "kb_stream").await, "1", "running on the server");
        cancel.cancel();
        let t = std::time::Instant::now();
        step("the stream ending after Stop", 20, async { while st.next().await.is_some() {} }).await;
        assert!(t.elapsed() < Duration::from_secs(5), "stopped in {:?}", t.elapsed());
        assert_eq!(running(&*root, "kb_stream").await, "0", "ended on the server");
        assert_eq!(cell(&ok(&*s, "select 7".into()).await), "7");
        assert_eq!(s.transaction_state(), databrain_connector_core::TxState::Open, "the transaction is kept");
        ok(&*s, "rollback".into()).await;

        // The statement itself notices Stop between rows (before the outer
        // cancel does): it ends the statement on the server too. `run`
        // directly, with no outer cancel to send the kill.
        {
            let cfg = live_cfg();
            let opener = Arc::new(Opener { creds: counting(&cfg), cfg, tls: false });
            let conn = Conn::new(opener.opts().await.unwrap()).await.unwrap();
            let ctx = crate::exec::Ctx {
                conn_id: Arc::new(AtomicU32::new(conn.id())),
                conn: Arc::new(Mutex::new(conn)),
                opener,
                state: Default::default(),
                finished: Arc::new(AtomicBool::new(false)),
                read_only: false,
            };
            ctx.conn.lock().await.query_drop("set cte_max_recursion_depth = 10000000").await.unwrap();
            ctx.conn.lock().await.query_drop("begin").await.unwrap();
            let cancel = databrain_connector_core::CancellationToken::new();
            let (tx, mut stream) = QueryStream::channel(4);
            let c2 = cancel.clone();
            let reader = tokio::spawn(async move {
                while let Some(ev) = stream.next().await {
                    if matches!(ev, Ok(databrain_connector_core::StreamEvent::Batch(_))) {
                        c2.cancel();
                    }
                }
            });
            // Before the fix `run` read the rest of the result here (DO 0 after
            // the error): 2 million rows, half an hour.
            let r = step(
                "the statement returning after Stop between rows",
                20,
                run(
                    &ctx,
                    "select /*kb_inner*/ i, sleep(0.001) from (with recursive n(i) as (select 1 union all select i + 1 from n where i < 2000000) select i from n) t".into(),
                    ExecOptions { batch_size: 2, cancel: cancel.clone(), ..Default::default() },
                    tx,
                ),
            )
            .await;
            step("the reader ending", 20, reader).await.unwrap();
            assert_eq!(r.unwrap_err().kind, ErrorKind::Cancelled);
            assert_eq!(running(&*root, "kb_inner").await, "0", "ended on the server");
            let one: Option<i64> = step("the connection answering", 20, async { ctx.conn.lock().await.query_first("select 1").await.unwrap() }).await;
            assert_eq!(one, Some(1), "the connection answers");
        }

        // The kill can't be sent (no connection to send it from): after 10 s
        // the stuck connection is replaced and the session answers.
        ok(&*root, "drop user if exists 'kb_two'@'%'".into()).await;
        ok(&*root, "create user 'kb_two'@'%' identified by 'kb-two-pw' with max_user_connections 2".into()).await;
        let s = live_as(Some(("kb_two", "kb-two-pw"))).await;
        let blocker = live_as(Some(("kb_two", "kb-two-pw"))).await;
        let cancel = databrain_connector_core::CancellationToken::new();
        let st = s
            .execute(
                databrain_connector_core::Statement::internal("select /*kb_stuck*/ sleep(120)"),
                ExecOptions { cancel: cancel.clone(), ..Default::default() },
            )
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;
        cancel.cancel();
        // Both connections the user may have are taken until then.
        tokio::time::sleep(Duration::from_secs(3)).await;
        drop(blocker);
        let t = std::time::Instant::now();
        let e = step("the stuck statement's stream ending", 30, st.collect()).await.expect_err("cancelled");
        assert!(t.elapsed() < Duration::from_secs(20), "returned after {:?}", t.elapsed());
        assert_eq!(e.kind, ErrorKind::Cancelled);
        assert!(e.message.contains("opened a new one"), "{}", e.message);
        // The KILL from the new connection ends it on the server a moment later.
        let mut left = String::new();
        for _ in 0..50 {
            left = running(&*root, "kb_stuck").await;
            if left == "0" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        assert_eq!(left, "0", "the stuck statement's connection was ended");
        let r = tokio::time::timeout(Duration::from_secs(5), live_run(&*s, "select 8", ExecOptions::default())).await.expect("the session answers");
        assert_eq!(cell(&r.unwrap()), "8");
        drop(s);
        ok(&*root, "drop user 'kb_two'@'%'".into()).await;
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
        let r = s
            .execute(databrain_connector_core::Statement::internal("select 1 as a, 'x' as b, now() as c, 1.5 as d"), ExecOptions::default())
            .await
            .unwrap()
            .collect()
            .await
            .unwrap();
        assert_eq!(r.num_rows(), 1);
        let cancel = databrain_connector_core::CancellationToken::new();
        let stream = s
            .execute(
                databrain_connector_core::Statement::internal("select sleep(30)"),
                ExecOptions { batch_size: 10, cancel: cancel.clone(), ..Default::default() },
            )
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        cancel.cancel();
        let _ = stream.collect().await;
        assert!(!s.list_schemas().await.unwrap().is_empty());
        if let Ok(db) = std::env::var("DATABRAIN_MYSQL_DB") {
            let run = |sql: &'static str| s.execute(databrain_connector_core::Statement::internal(sql), ExecOptions::default());
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
