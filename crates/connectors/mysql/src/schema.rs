//! Batched schema introspection and foreign keys.

use crate::session::MysqlSession;
use databrain_connector_core::ColumnInfo;
use databrain_connector_core::DbObject;
use databrain_connector_core::ForeignKey;
use databrain_connector_core::ObjectKind;
use databrain_connector_core::Result;

impl MysqlSession {
    pub(crate) async fn bulk_metadata_batched(&self, schemas: &[String]) -> Result<Vec<databrain_connector_core::SchemaMetadata>> {
        if schemas.is_empty() {
            return Ok(vec![]);
        }
        let marks = vec!["?"; schemas.len()].join(",");
        let params = || schemas.iter().map(|s| mysql_async::Value::from(s.as_str())).collect::<Vec<_>>();
        #[allow(clippy::type_complexity)]
        let tables: Vec<(String, String, String, Option<String>, Option<u64>)> = self
            .query_rows(
                &format!("select table_schema, table_name, table_type, table_comment, table_rows from information_schema.tables where table_schema in ({marks}) order by 1, 2"),
                params(),
            )
            .await?;
        let objects = tables
            .into_iter()
            .map(|(schema, name, t, comment, rows)| DbObject {
                schema,
                name,
                kind: if t.contains("VIEW") { ObjectKind::View } else { ObjectKind::Table },
                comment: comment.filter(|c| !c.is_empty()),
                row_estimate: rows.map(|r| r as i64),
            })
            .collect();
        #[allow(clippy::type_complexity)]
        let cols: Vec<(String, String, String, String, String, String, Option<String>)> = self
            .query_rows(
                &format!(
                    "select table_schema, table_name, column_name, column_type, is_nullable, column_key, column_comment \
                     from information_schema.columns where table_schema in ({marks}) order by 1, 2, ordinal_position"
                ),
                params(),
            )
            .await?;
        let columns = cols
            .into_iter()
            .map(|(schema, table, name, t, nullable, key, comment)| {
                (
                    schema,
                    table,
                    ColumnInfo {
                        name,
                        data_type: t,
                        nullable: nullable == "YES",
                        is_primary_key: key == "PRI",
                        default: None,
                        comment: comment.filter(|c| !c.is_empty()),
                    },
                )
            })
            .collect();
        #[allow(clippy::type_complexity)]
        let fk_rows: Vec<(String, String, String, String, String, String, String)> = self
            .query_rows(
                &format!(
                    "select table_schema, table_name, constraint_name, column_name, referenced_table_schema, referenced_table_name, referenced_column_name \
                     from information_schema.key_column_usage where table_schema in ({marks}) and referenced_table_name is not null \
                     order by 1, 2, 3, ordinal_position"
                ),
                params(),
            )
            .await?;
        // Rows of one constraint are adjacent.
        let mut fks: Vec<(String, String, ForeignKey)> = Vec::new();
        let mut last: Option<(String, String, String)> = None;
        for (s, t, c, col, rs, rt, rc) in fk_rows {
            let key = (s, t, c);
            if last.as_ref() == Some(&key) {
                if let Some((_, _, fk)) = fks.last_mut() {
                    fk.columns.push(col);
                    fk.ref_columns.push(rc);
                }
            } else {
                fks.push((key.0.clone(), key.1.clone(), ForeignKey { columns: vec![col], ref_schema: rs, ref_table: rt, ref_columns: vec![rc] }));
                last = Some(key);
            }
        }
        Ok(databrain_connector_core::assemble_metadata(schemas, objects, columns, Some(fks)))
    }

    pub(crate) async fn foreign_keys(&self, schema: &str, table: Option<&str>) -> Result<Vec<(String, ForeignKey)>> {
        let rows: Vec<(String, String, String, String, String, String)> = self
            .query_rows(
                "select table_name, constraint_name, column_name, referenced_table_schema, \
                        referenced_table_name, referenced_column_name \
                 from information_schema.key_column_usage \
                 where table_schema = ? and referenced_table_name is not null and (? = '' or table_name = ?) \
                 order by table_name, constraint_name, ordinal_position",
                vec![schema.into(), table.unwrap_or("").into(), table.unwrap_or("").into()],
            )
            .await?;
        let mut out: Vec<(String, String, ForeignKey)> = Vec::new();
        for (t, cname, col, rs, rt, rc) in rows {
            match out.iter_mut().find(|(tt, cn, _)| *tt == t && *cn == cname) {
                Some((_, _, fk)) => {
                    fk.columns.push(col);
                    fk.ref_columns.push(rc);
                }
                None => out.push((t, cname, ForeignKey { columns: vec![col], ref_schema: rs, ref_table: rt, ref_columns: vec![rc] })),
            }
        }
        Ok(out.into_iter().map(|(t, _, f)| (t, f)).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MysqlConnector;
    use databrain_connector_core::ConnectionConfig;
    use databrain_connector_core::Connector;
    use databrain_connector_core::ConnectorKind;
    use databrain_connector_core::ExecOptions;
    use std::sync::Arc;

    /// Live: bulk metadata (one batch query set) and schema fingerprints.
    /// DATABRAIN_MYSQL_HOST (+ _PORT, _USER, _PASSWORD).
    #[ignore = "live: needs DATABRAIN_MYSQL_HOST (cargo test -- --ignored)"]
    #[tokio::test]
    async fn live_bulk_metadata_and_fingerprints() {
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
        // Show DDL: table, view, function, procedure.
        for stmt in [
            "drop database if exists kb_ddl",
            "create database kb_ddl",
            "create table kb_ddl.t (id int primary key, n varchar(10))",
            "create view kb_ddl.v as select id from kb_ddl.t",
            "create function kb_ddl.twice(x int) returns int deterministic return x * 2",
            "create procedure kb_ddl.noop() begin end",
        ] {
            s.execute(databrain_connector_core::Statement::internal(stmt), ExecOptions::default()).await.unwrap().collect().await.unwrap();
        }
        let ddl = |n: &'static str, k: ObjectKind| {
            let s = &s;
            async move { s.object_ddl("kb_ddl", n, k).await.unwrap().unwrap_or_default() }
        };
        assert!(ddl("t", ObjectKind::Table).await.starts_with("CREATE TABLE `t`"));
        assert!(ddl("v", ObjectKind::View).await.contains("VIEW `kb_ddl`.`v` AS"));
        assert!(ddl("twice", ObjectKind::Function).await.contains("FUNCTION `twice`"));
        assert!(ddl("noop", ObjectKind::Procedure).await.contains("PROCEDURE `noop`"));
        s.execute(databrain_connector_core::Statement::internal("drop database kb_ddl"), ExecOptions::default()).await.unwrap().collect().await.unwrap();
        let run = |sql: &'static str| {
            let s = &s;
            async move {
                for stmt in sql.split(';').map(str::trim).filter(|x| !x.is_empty()) {
                    s.execute(databrain_connector_core::Statement::internal(stmt), ExecOptions::default()).await.unwrap().collect().await.unwrap();
                }
            }
        };
        run("drop database if exists kb_b; drop database if exists kb_a; drop database if exists kb_c; \
             create database kb_a; create database kb_b; create database kb_c; \
             create table kb_a.customers (id int primary key, email varchar(100) comment 'login') comment 'People'; \
             create table kb_b.orders (id int primary key, customer_id int, total decimal(10,2), \
               constraint fk_c foreign key (customer_id) references kb_a.customers(id)); \
             create view kb_b.big_orders as select * from kb_b.orders where total > 100; \
             create table kb_c.untouched (x int)")
        .await;
        let m = s.bulk_metadata(&["kb_a".into(), "kb_b".into()]).await.unwrap();
        assert_eq!(m[0].objects[0].comment.as_deref(), Some("People"));
        let cust = &m[0].columns[0];
        assert!(cust.columns[0].is_primary_key && cust.columns[1].comment.as_deref() == Some("login"));
        let orders = m[1].columns.iter().find(|t| t.table == "orders").unwrap();
        assert_eq!(orders.columns.iter().map(|c| c.data_type.as_str()).collect::<Vec<_>>(), vec!["int", "int", "decimal(10,2)"]);
        assert_eq!((orders.foreign_keys[0].ref_schema.as_str(), orders.foreign_keys[0].ref_table.as_str()), ("kb_a", "customers"));
        assert!(m[1].objects.iter().any(|o| o.name == "big_orders" && o.kind == ObjectKind::View));
        assert_eq!(&s.schema_columns("kb_b").await.unwrap().into_iter().find(|t| t.table == "orders").unwrap(), orders, "same as per-schema path");

        let fps = || async { s.explorer_fingerprints().await.unwrap().unwrap() };
        let f0 = fps().await;
        // Only this test's schemas: tests running alongside change theirs.
        let ours = |f: &std::collections::HashMap<String, String>| {
            f.iter().filter(|(k, _)| k.starts_with("kb_")).map(|(k, v)| (k.clone(), v.clone())).collect::<std::collections::BTreeMap<_, _>>()
        };
        assert_eq!(ours(&f0), ours(&fps().await), "stable");
        run("alter table kb_a.customers add column name text").await;
        let f1 = fps().await;
        assert_ne!(f0["kb_a"], f1["kb_a"], "new column");
        assert_eq!(f0["kb_c"], f1["kb_c"], "other schemas unchanged");
        run("alter table kb_b.orders modify total decimal(10,2) comment 'gross'").await;
        let f2 = fps().await;
        assert_ne!(f1["kb_b"], f2["kb_b"], "column comment");
        run("drop view kb_b.big_orders").await;
        let f3 = fps().await;
        assert_ne!(f2["kb_b"], f3["kb_b"], "dropped view");
        run("insert into kb_c.untouched values (1); analyze table kb_c.untouched").await;
        assert_eq!(f3["kb_c"], fps().await["kb_c"], "data changes are not schema changes");
        run("drop database kb_b; drop database kb_a; drop database kb_c").await;
    }
}
