//! MySQL / MariaDB connector built on `mysql_async` (text protocol).

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use databrain_auth::{AuthMethodKind, Credential, CredentialSource};
use databrain_connector_core::value::{Column, parse_date, parse_datetime};
use databrain_connector_core::{
    BatchBuilder, Capabilities, ColType, ColumnInfo, ConnectionConfig, Connector, ConnectorError,
    ConnectorInfo, ConnectorKind, DbObject, ErrorKind, ExecOptions, ExecSummary, FieldSpec, ForeignKey,
    ObjectDetail, ObjectKind, QueryStream, Result, SchemaInfo, Session, SslMode, StreamEvent, StreamSender,
    TableColumns, Value, quote_ident,
};
use mysql_async::consts::{ColumnFlags, ColumnType};
use mysql_async::prelude::*;
use mysql_async::{Conn, Opts, OptsBuilder, SslOpts};
use secrecy::ExposeSecret;
use tokio::sync::Mutex;

/// Character set id MySQL uses for binary strings.
const BINARY_CHARSET: u16 = 63;

#[derive(Debug, Default)]
pub struct MysqlConnector;

impl MysqlConnector {
    pub fn new() -> Self {
        Self
    }
}

fn build_opts(cfg: &ConnectionConfig, user: String, password: String, ssl: Option<SslOpts>) -> Opts {
    let mut init = vec!["SET NAMES utf8mb4".to_string()];
    if cfg.read_only {
        init.push("SET SESSION TRANSACTION READ ONLY".to_string());
    }
    OptsBuilder::default()
        .ip_or_hostname(cfg.host_or_default())
        .tcp_port(cfg.port.unwrap_or(3306))
        .user(Some(user))
        .pass(Some(password).filter(|p| !p.is_empty()))
        .db_name(cfg.database.clone().filter(|d| !d.is_empty()))
        .prefer_socket(false)
        .init(init)
        .ssl_opts(ssl)
        .into()
}

fn ssl_opts(mode: SslMode) -> Option<SslOpts> {
    match mode {
        SslMode::Disable => None,
        SslMode::Prefer | SslMode::Require => Some(
            SslOpts::default()
                .with_danger_accept_invalid_certs(true)
                .with_danger_skip_domain_validation(true),
        ),
        SslMode::VerifyFull => Some(SslOpts::default()),
    }
}

async fn connect_with_timeout(opts: Opts) -> Result<Conn> {
    match tokio::time::timeout(Duration::from_secs(15), Conn::new(opts)).await {
        Ok(r) => r.map_err(|e| {
            let mut ce = map_err(e);
            if ce.kind == ErrorKind::Query {
                ce.kind = ErrorKind::Connection;
            }
            ce
        }),
        Err(_) => Err(ConnectorError::connection("connection timed out")),
    }
}

#[async_trait]
impl Connector for MysqlConnector {
    fn info(&self) -> ConnectorInfo {
        ConnectorInfo {
            kind: ConnectorKind::Mysql,
            display_name: "MySQL / MariaDB",
            default_port: Some(3306),
            uses_file: false,
            auth_methods: vec![AuthMethodKind::Password],
            capabilities: Capabilities {
                transactions: true,
                cancel: true,
                schemas: true,
                read_only_sessions: true,
                ssh: true,
            },
            fields: vec![
                FieldSpec::new("host", "Host").required().placeholder("localhost"),
                FieldSpec::new("port", "Port").placeholder("3306"),
                FieldSpec::new("database", "Database").placeholder("(optional)"),
            ],
            note: None,
        }
    }

    async fn connect(
        &self,
        cfg: &ConnectionConfig,
        creds: Arc<dyn CredentialSource>,
    ) -> Result<Box<dyn Session>> {
        let (user, password) = match creds.get().await? {
            Credential::Password { user, password } => (user, password.expose_secret().to_string()),
            _ => return Err(ConnectorError::config("MySQL requires user/password authentication")),
        };
        let opts = build_opts(cfg, user.clone(), password.clone(), ssl_opts(cfg.ssl_mode));
        let conn = match connect_with_timeout(opts.clone()).await {
            Ok(c) => (c, opts),
            // "Prefer": fall back to plaintext if the server has no TLS.
            Err(e) if cfg.ssl_mode == SslMode::Prefer && e.kind == ErrorKind::Connection => {
                let plain = build_opts(cfg, user, password, None);
                (connect_with_timeout(plain.clone()).await?, plain)
            }
            Err(e) => return Err(e),
        };
        let (conn, opts) = conn;
        Ok(Box::new(MysqlSession {
            conn_id: conn.id(),
            conn: Arc::new(Mutex::new(conn)),
            opts,
        }))
    }
}

fn map_err(e: mysql_async::Error) -> ConnectorError {
    match e {
        mysql_async::Error::Server(se) => {
            if se.code == 1317 {
                // ER_QUERY_INTERRUPTED
                return ConnectorError::cancelled();
            }
            ConnectorError::query(se.message).with_code(format!("{} ({})", se.code, se.state))
        }
        mysql_async::Error::Io(io) => ConnectorError::connection(io.to_string()),
        other => ConnectorError::query(other.to_string()),
    }
}

pub struct MysqlSession {
    conn: Arc<Mutex<Conn>>,
    conn_id: u32,
    opts: Opts,
}

fn col_type(c: &mysql_async::Column) -> ColType {
    let unsigned = c.flags().contains(ColumnFlags::UNSIGNED_FLAG);
    let binary = c.character_set() == BINARY_CHARSET;
    use ColumnType::*;
    match c.column_type() {
        MYSQL_TYPE_TINY | MYSQL_TYPE_SHORT | MYSQL_TYPE_LONG | MYSQL_TYPE_INT24 | MYSQL_TYPE_YEAR => {
            ColType::Int64
        }
        MYSQL_TYPE_LONGLONG if unsigned => ColType::UInt64,
        MYSQL_TYPE_LONGLONG => ColType::Int64,
        MYSQL_TYPE_FLOAT | MYSQL_TYPE_DOUBLE => ColType::Float64,
        MYSQL_TYPE_DATE | MYSQL_TYPE_NEWDATE => ColType::Date,
        MYSQL_TYPE_DATETIME | MYSQL_TYPE_TIMESTAMP | MYSQL_TYPE_DATETIME2
        | MYSQL_TYPE_TIMESTAMP2 => ColType::Timestamp,
        MYSQL_TYPE_BIT => ColType::Binary,
        MYSQL_TYPE_TINY_BLOB | MYSQL_TYPE_MEDIUM_BLOB | MYSQL_TYPE_LONG_BLOB | MYSQL_TYPE_BLOB
        | MYSQL_TYPE_VAR_STRING | MYSQL_TYPE_STRING | MYSQL_TYPE_VARCHAR
        | MYSQL_TYPE_GEOMETRY
            if binary =>
        {
            ColType::Binary
        }
        // DECIMAL stays text to keep exact precision; TIME can exceed 24h.
        _ => ColType::Utf8,
    }
}

fn type_name(c: &mysql_async::Column) -> String {
    let t = format!("{:?}", c.column_type());
    let mut s = t.trim_start_matches("MYSQL_TYPE_").to_ascii_lowercase();
    if c.flags().contains(ColumnFlags::UNSIGNED_FLAG) {
        s.push_str(" unsigned");
    }
    s
}

fn convert(t: ColType, v: mysql_async::Value) -> Value {
    use mysql_async::Value as M;
    match v {
        M::NULL => Value::Null,
        M::Int(i) => Value::Int(i),
        M::UInt(u) => Value::UInt(u),
        M::Float(f) => Value::Float(f as f64),
        M::Double(f) => Value::Float(f),
        M::Bytes(b) => match t {
            ColType::Binary => Value::Bytes(b),
            _ => {
                let s = String::from_utf8(b)
                    .unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned());
                match t {
                    ColType::Date => parse_date(&s).map(Value::Date).unwrap_or(Value::Text(s)),
                    ColType::Timestamp => parse_datetime(&s)
                        .map(Value::Timestamp)
                        .unwrap_or(Value::Text(s)),
                    _ => Value::Text(s),
                }
            }
        },
        other => Value::Text(other.as_sql(true).trim_matches('\'').to_string()),
    }
}

async fn run(conn: Arc<Mutex<Conn>>, sql: String, opts: ExecOptions, tx: StreamSender) -> Result<()> {
    let mut guard = conn.lock().await;
    let mut result = guard.query_iter(sql).await.map_err(map_err)?;

    let columns = result.columns().unwrap_or_else(|| Arc::from(Vec::new()));
    if columns.is_empty() {
        let affected = result.affected_rows();
        result.drop_result().await.map_err(map_err)?;
        tx.send(StreamEvent::Done(ExecSummary {
            rows_affected: Some(affected),
        }))
        .await;
        return Ok(());
    }

    let cols: Vec<Column> = columns
        .iter()
        .map(|c| Column::new(c.name_str().into_owned(), col_type(c), type_name(c)))
        .collect();
    let types: Vec<ColType> = cols.iter().map(|c| c.col_type).collect();
    let batch_size = opts.batch_size.max(1);
    let mut builder = BatchBuilder::new(&cols, batch_size);
    if !tx.send(StreamEvent::Schema(builder.schema())).await {
        return Ok(());
    }

    while let Some(row) = result.next().await.map_err(map_err)? {
        let vals = row.unwrap();
        builder.push_row(
            vals.into_iter()
                .zip(types.iter())
                .map(|(v, t)| convert(*t, v)),
        );
        if builder.is_full() && !tx.send(StreamEvent::Batch(builder.finish()?)).await {
            break;
        }
        if opts.cancel.is_cancelled() {
            return Err(ConnectorError::cancelled());
        }
    }
    // Drain any additional result sets (e.g. from CALL) so the connection
    // is ready for the next statement.
    result.drop_result().await.map_err(map_err)?;
    if !builder.is_empty() && !tx.send(StreamEvent::Batch(builder.finish()?)).await {
        return Ok(());
    }
    if builder.coercion_failures() > 0 {
        tx.send(StreamEvent::Notice(format!(
            "{} value(s) could not be converted (e.g. zero dates) and are shown as NULL",
            builder.coercion_failures()
        )))
        .await;
    }
    tx.send(StreamEvent::Done(ExecSummary::default())).await;
    Ok(())
}

impl MysqlSession {
    /// Kill the running query using a short-lived side connection.
    async fn kill_query(opts: Opts, id: u32) {
        if let Ok(Ok(mut c)) = tokio::time::timeout(Duration::from_secs(5), Conn::new(opts)).await {
            let _ = c.query_drop(format!("KILL QUERY {id}")).await;
            let _ = c.disconnect().await;
        }
    }

    async fn query_rows<T: FromRow + Send + 'static>(
        &self,
        sql: &str,
        params: Vec<mysql_async::Value>,
    ) -> Result<Vec<T>> {
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

    async fn ping(&self) -> Result<()> {
        self.conn.lock().await.ping().await.map_err(map_err)
    }

    async fn execute(&self, sql: &str, opts: ExecOptions) -> Result<QueryStream> {
        let (tx, stream) = QueryStream::channel(4);
        let conn = self.conn.clone();
        let (kill_opts, id) = (self.opts.clone(), self.conn_id);
        let sql = sql.to_string();
        tokio::spawn(async move {
            let cancel = opts.cancel.clone();
            let work = run(conn, sql, opts, tx.clone());
            tokio::pin!(work);
            let res = tokio::select! {
                r = &mut work => r,
                _ = cancel.cancelled() => {
                    Self::kill_query(kill_opts, id).await;
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
        let rows: Vec<(String, Option<String>)> = self
            .query_rows(
                "select schema_name, database() from information_schema.schemata order by schema_name",
                vec![],
            )
            .await?;
        let mut out: Vec<SchemaInfo> = rows
            .into_iter()
            .map(|(name, cur)| SchemaInfo {
                is_default: cur.as_deref() == Some(name.as_str()),
                name,
                catalog: None,
            })
            .collect();
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
                kind: if t.contains("VIEW") {
                    ObjectKind::View
                } else {
                    ObjectKind::Table
                },
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
            kind: if t == "PROCEDURE" {
                ObjectKind::Procedure
            } else {
                ObjectKind::Function
            },
            comment: None,
            row_estimate: None,
        }));
        Ok(out)
    }

    async fn table_layout(&self, schema: &str, name: &str) -> Result<databrain_connector_core::TableLayout> {
        use databrain_connector_core::{group_indexes, query_text, quote_literal, truthy, TableLayout};
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
        {
            if let Some(r) = p.first() {
                l.partition_kind = r[0].as_deref().map(|m| m.to_ascii_lowercase());
                l.partition_by = r[1].as_deref().map(|e| databrain_connector_core::split_key_list(e).into_iter().map(|x| databrain_connector_core::unquote_ident(&x)).collect()).unwrap_or_default();
                if let Some(n) = &r[2] {
                    l.notes.push(format!("{n} partitions"));
                }
            }
        }
        if let Ok(r) = query_text(self, &format!("select table_rows from information_schema.tables where table_schema = {s} and table_name = {t}")).await {
            l.row_estimate = r.first().and_then(|r| r[0].as_deref()).and_then(|x| x.parse().ok());
        }
        Ok(l)
    }

    async fn describe(&self, schema: &str, name: &str) -> Result<ObjectDetail> {
        let meta: Vec<(String, Option<String>, Option<u64>)> = self
            .query_rows(
                "select table_type, table_comment, table_rows from information_schema.tables \
                 where table_schema = ? and table_name = ?",
                vec![schema.into(), name.into()],
            )
            .await?;
        let (ttype, comment, rows) = meta
            .into_iter()
            .next()
            .ok_or_else(|| ConnectorError::query(format!("object not found: {schema}.{name}")))?;
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
                kind: if is_view {
                    ObjectKind::View
                } else {
                    ObjectKind::Table
                },
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

impl MysqlSession {
    async fn bulk_metadata_batched(&self, schemas: &[String]) -> Result<Vec<databrain_connector_core::SchemaMetadata>> {
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
                (schema, table, ColumnInfo { name, data_type: t, nullable: nullable == "YES", is_primary_key: key == "PRI", default: None, comment: comment.filter(|c| !c.is_empty()) })
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
        Ok(databrain_connector_core::assemble_metadata(schemas, objects, columns, fks))
    }

    async fn foreign_keys(&self, schema: &str, table: Option<&str>) -> Result<Vec<(String, ForeignKey)>> {
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

    #[test]
    fn converts_text_values() {
        assert_eq!(
            convert(ColType::Date, mysql_async::Value::Bytes(b"2024-01-02".to_vec())),
            Value::Date(parse_date("2024-01-02").unwrap())
        );
        assert_eq!(
            convert(ColType::Date, mysql_async::Value::Bytes(b"0000-00-00".to_vec())),
            Value::Text("0000-00-00".into())
        );
        assert_eq!(
            convert(ColType::Utf8, mysql_async::Value::Bytes(b"12.50".to_vec())),
            Value::Text("12.50".into())
        );
    }

    /// Live: bulk metadata (one batch query set) and schema fingerprints.
    /// DATABRAIN_MYSQL_HOST (+ _PORT, _USER, _PASSWORD).
    #[tokio::test]
    async fn live_bulk_metadata_and_fingerprints() {
        let Ok(host) = std::env::var("DATABRAIN_MYSQL_HOST") else {
            eprintln!("skipping: DATABRAIN_MYSQL_HOST not set");
            return;
        };
        use databrain_auth::{AuthMethod, InlineCredentialSource};
        let user = std::env::var("DATABRAIN_MYSQL_USER").unwrap_or_else(|_| "root".into());
        let mut cfg = ConnectionConfig::new(ConnectorKind::Mysql, AuthMethod::Password { user: user.clone() });
        cfg.host = Some(host);
        cfg.port = std::env::var("DATABRAIN_MYSQL_PORT").ok().and_then(|p| p.parse().ok());
        let creds = Arc::new(InlineCredentialSource::new(AuthMethod::Password { user }, std::env::var("DATABRAIN_MYSQL_PASSWORD").ok().map(secrecy::SecretString::from)));
        let s = MysqlConnector.connect(&cfg, creds).await.unwrap();
        let run = |sql: &'static str| {
            let s = &s;
            async move {
                for stmt in sql.split(';').map(str::trim).filter(|x| !x.is_empty()) {
                    s.execute(stmt, ExecOptions::default()).await.unwrap().collect().await.unwrap();
                }
            }
        };
        run("drop database if exists kb_a; drop database if exists kb_b; drop database if exists kb_c; \
             create database kb_a; create database kb_b; create database kb_c; \
             create table kb_a.customers (id int primary key, email varchar(100) comment 'login') comment 'People'; \
             create table kb_b.orders (id int primary key, customer_id int, total decimal(10,2), \
               constraint fk_c foreign key (customer_id) references kb_a.customers(id)); \
             create view kb_b.big_orders as select * from kb_b.orders where total > 100; \
             create table kb_c.untouched (x int)").await;
        let m = s.bulk_metadata(&["kb_a".into(), "kb_b".into()]).await.unwrap();
        assert_eq!(m[0].objects[0].comment.as_deref(), Some("People"));
        let cust = &m[0].columns[0];
        assert!(cust.columns[0].is_primary_key && cust.columns[1].comment.as_deref() == Some("login"));
        let orders = m[1].columns.iter().find(|t| t.table == "orders").unwrap();
        assert_eq!(orders.columns.iter().map(|c| c.data_type.as_str()).collect::<Vec<_>>(), vec!["int", "int", "decimal(10,2)"]);
        assert_eq!((orders.foreign_keys[0].ref_schema.as_str(), orders.foreign_keys[0].ref_table.as_str()), ("kb_a", "customers"));
        assert!(m[1].objects.iter().any(|o| o.name == "big_orders" && o.kind == ObjectKind::View));
        assert_eq!(&s.schema_columns("kb_b").await.unwrap().into_iter().find(|t| t.table == "orders").unwrap(), orders, "same as per-schema path");

        let fps = || async { s.schema_fingerprints().await.unwrap().unwrap() };
        let f0 = fps().await;
        assert_eq!(f0, fps().await, "stable");
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

    /// Live test: set DATABRAIN_MYSQL_HOST (+ _USER, _PASSWORD, _DB).
    #[tokio::test]
    async fn live_roundtrip() {
        let Ok(host) = std::env::var("DATABRAIN_MYSQL_HOST") else {
            eprintln!("skipping: DATABRAIN_MYSQL_HOST not set");
            return;
        };
        use databrain_auth::{AuthMethod, InlineCredentialSource};
        let user = std::env::var("DATABRAIN_MYSQL_USER").unwrap_or_else(|_| "root".into());
        let mut cfg = ConnectionConfig::new(ConnectorKind::Mysql, AuthMethod::Password { user: user.clone() });
        cfg.host = Some(host);
        cfg.database = std::env::var("DATABRAIN_MYSQL_DB").ok();
        let creds = Arc::new(InlineCredentialSource::new(
            AuthMethod::Password { user },
            std::env::var("DATABRAIN_MYSQL_PASSWORD").ok().map(secrecy::SecretString::from),
        ));
        let s = MysqlConnector.connect(&cfg, creds).await.unwrap();
        let r = s
            .execute("select 1 as a, 'x' as b, now() as c, 1.5 as d", ExecOptions::default())
            .await
            .unwrap()
            .collect()
            .await
            .unwrap();
        assert_eq!(r.num_rows(), 1);
        let cancel = databrain_connector_core::CancellationToken::new();
        let stream = s
            .execute(
                "select sleep(30)",
                ExecOptions {
                    batch_size: 10,
                    cancel: cancel.clone(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
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
        }
    }
}
