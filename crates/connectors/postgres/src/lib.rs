//! PostgreSQL connector built on `tokio-postgres`.
//!
//! Execution strategy: the statement is first prepared to learn the result
//! column types, then executed with the simple-query (text) protocol. Text
//! results make every Postgres type displayable exactly as `psql` shows it
//! (numeric, arrays, json, ranges, ...), while the prepared column types let
//! common types become typed Arrow columns for sorting and filtering.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use databrain_auth::{AuthMethodKind, Credential, CredentialSource};
use databrain_connector_core::value::{Column, parse_date, parse_datetime, parse_time};
use databrain_connector_core::{
    BatchBuilder, Capabilities, ColType, ColumnInfo, ConnectionConfig, Connector, ConnectorError,
    ConnectorInfo, ConnectorKind, DbObject, ExecOptions, ExecSummary, FieldSpec, ForeignKey, ObjectDetail,
    ObjectKind, QueryStream, Result, SchemaInfo, Session, SslMode, StreamEvent, StreamSender, TableColumns,
    Value,
};
use futures::StreamExt;
use postgres_native_tls::MakeTlsConnector;
use secrecy::ExposeSecret;
use tokio_postgres::error::ErrorPosition;
use tokio_postgres::types::Type;
use tokio_postgres::{Client, SimpleQueryMessage};

#[derive(Debug, Default)]
pub struct PostgresConnector;

impl PostgresConnector {
    pub fn new() -> Self {
        Self
    }
}

fn tls_connector(mode: SslMode) -> Result<MakeTlsConnector> {
    let mut b = native_tls::TlsConnector::builder();
    // libpq semantics: prefer/require encrypt but do not verify the server
    // certificate; verify-full verifies chain and host name.
    if mode != SslMode::VerifyFull {
        b.danger_accept_invalid_certs(true);
        b.danger_accept_invalid_hostnames(true);
    }
    let c = b
        .build()
        .map_err(|e| ConnectorError::config(format!("TLS setup failed: {e}")))?;
    Ok(MakeTlsConnector::new(c))
}

#[async_trait]
impl Connector for PostgresConnector {
    fn info(&self) -> ConnectorInfo {
        ConnectorInfo {
            kind: ConnectorKind::Postgres,
            display_name: "PostgreSQL",
            default_port: Some(5432),
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
                FieldSpec::new("port", "Port").placeholder("5432"),
                FieldSpec::new("database", "Database").placeholder("postgres"),
            ],
            note: None,
        }
    }

    async fn connect(
        &self,
        cfg: &ConnectionConfig,
        creds: Arc<dyn CredentialSource>,
    ) -> Result<Box<dyn Session>> {
        let mut pg = tokio_postgres::Config::new();
        pg.host(cfg.host_or_default())
            .port(cfg.port.unwrap_or(5432))
            .dbname(cfg.database.as_deref().filter(|d| !d.is_empty()).unwrap_or("postgres"))
            .application_name("DataBrain")
            .connect_timeout(Duration::from_secs(15))
            .keepalives(true);

        match creds.get().await? {
            Credential::Password { user, password } => {
                pg.user(&user);
                if !password.expose_secret().is_empty() {
                    pg.password(password.expose_secret());
                }
            }
            Credential::None => {
                return Err(ConnectorError::config("PostgreSQL requires a user name"));
            }
            Credential::Bearer { token, user } => {
                // e.g. cloud IAM tokens used as the password.
                pg.user(user.as_deref().unwrap_or_default());
                pg.password(token.expose_secret());
            }
            _ => {
                return Err(ConnectorError::config("unsupported authentication method for PostgreSQL"));
            }
        }

        // ISO dates make text results parseable regardless of server config.
        let mut options = String::from("-c DateStyle=ISO,MDY");
        if cfg.read_only {
            options.push_str(" -c default_transaction_read_only=on");
        }
        pg.options(&options);

        pg.ssl_mode(match cfg.ssl_mode {
            SslMode::Disable => tokio_postgres::config::SslMode::Disable,
            SslMode::Prefer => tokio_postgres::config::SslMode::Prefer,
            SslMode::Require | SslMode::VerifyFull => tokio_postgres::config::SslMode::Require,
        });
        let tls = tls_connector(cfg.ssl_mode)?;
        let (client, connection) = pg.connect(tls.clone()).await.map_err(|e| {
            let mut ce = map_err(&e);
            ce.kind = databrain_connector_core::ErrorKind::Connection;
            ce
        })?;
        tokio::spawn(async move {
            // Ends when the client is dropped or the socket closes.
            let _ = connection.await;
        });
        Ok(Box::new(PgSession {
            client: Arc::new(client),
            tls,
        }))
    }
}

fn map_err(e: &tokio_postgres::Error) -> ConnectorError {
    if let Some(db) = e.as_db_error() {
        let mut msg = db.message().to_string();
        if let Some(d) = db.detail() {
            msg.push_str("\nDETAIL: ");
            msg.push_str(d);
        }
        if let Some(h) = db.hint() {
            msg.push_str("\nHINT: ");
            msg.push_str(h);
        }
        let pos = match db.position() {
            Some(ErrorPosition::Original(p)) => Some(*p),
            _ => None,
        };
        let code = db.code().code();
        if code == "57014" {
            return ConnectorError::cancelled();
        }
        return ConnectorError::query(msg)
            .with_code(code)
            .with_position(pos);
    }
    if e.is_closed() {
        return ConnectorError::connection("connection closed");
    }
    // Include the source chain (e.g. "connection refused").
    let mut msg = e.to_string();
    let mut src = std::error::Error::source(e);
    while let Some(s) = src {
        msg.push_str(": ");
        msg.push_str(&s.to_string());
        src = s.source();
    }
    ConnectorError::connection(msg)
}

pub struct PgSession {
    client: Arc<Client>,
    tls: MakeTlsConnector,
}

fn col_type(t: &Type) -> ColType {
    match *t {
        Type::BOOL => ColType::Bool,
        Type::INT2 | Type::INT4 | Type::INT8 | Type::OID => ColType::Int64,
        Type::FLOAT4 | Type::FLOAT8 => ColType::Float64,
        Type::BYTEA => ColType::Binary,
        Type::DATE => ColType::Date,
        Type::TIMESTAMP => ColType::Timestamp,
        Type::TIMESTAMPTZ => ColType::TimestampTz,
        Type::TIME => ColType::Time,
        _ => ColType::Utf8,
    }
}

/// Parse a text-protocol value for the given column type.
fn parse_text(t: ColType, s: &str) -> Value {
    match t {
        ColType::Bool => Value::Bool(s == "t"),
        ColType::Int64 => s.parse().map(Value::Int).unwrap_or_else(|_| Value::Text(s.into())),
        ColType::Float64 => s
            .parse()
            .map(Value::Float)
            .unwrap_or_else(|_| Value::Text(s.into())),
        ColType::Binary => decode_bytea(s)
            .map(Value::Bytes)
            .unwrap_or_else(|| Value::Text(s.into())),
        ColType::Date => parse_date(s)
            .filter(|_| !s.ends_with("BC"))
            .map(Value::Date)
            .unwrap_or_else(|| Value::Text(s.into())),
        ColType::Timestamp => parse_datetime(s)
            .filter(|_| !s.ends_with("BC"))
            .map(Value::Timestamp)
            .unwrap_or_else(|| Value::Text(s.into())),
        ColType::TimestampTz => parse_timestamptz(s)
            .map(Value::TimestampTz)
            .unwrap_or_else(|| Value::Text(s.into())),
        ColType::Time => parse_time(s)
            .map(Value::Time)
            .unwrap_or_else(|| Value::Text(s.into())),
        _ => Value::Text(s.to_string()),
    }
}

fn decode_bytea(s: &str) -> Option<Vec<u8>> {
    let hex = s.strip_prefix("\\x")?;
    if hex.len() % 2 != 0 {
        return None;
    }
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).ok())
        .collect()
}

/// Parse `YYYY-MM-DD HH:MM:SS[.f]+HH[:MM[:SS]]` into UTC microseconds.
fn parse_timestamptz(s: &str) -> Option<i64> {
    if s.ends_with("BC") {
        return None;
    }
    let idx = s
        .char_indices()
        .skip(11)
        .find(|(_, c)| *c == '+' || *c == '-')
        .map(|(i, _)| i)?;
    let (dt, off) = s.split_at(idx);
    let local = parse_datetime(dt)?;
    let sign = if off.starts_with('-') { -1 } else { 1 };
    let mut parts = off[1..].split(':');
    let h: i64 = parts.next()?.parse().ok()?;
    let m: i64 = parts.next().unwrap_or("0").parse().ok()?;
    let sec: i64 = parts.next().unwrap_or("0").parse().ok()?;
    let offset_us = sign * ((h * 60 + m) * 60 + sec) * 1_000_000;
    Some(local - offset_us)
}

async fn run(
    client: Arc<Client>,
    sql: String,
    opts: ExecOptions,
    tx: StreamSender,
) -> Result<()> {
    let stmt = client.prepare(&sql).await.map_err(|e| map_err(&e))?;

    if stmt.columns().is_empty() {
        let msgs = client.simple_query(&sql).await.map_err(|e| map_err(&e))?;
        let affected = msgs.iter().rev().find_map(|m| match m {
            SimpleQueryMessage::CommandComplete(n) => Some(*n),
            _ => None,
        });
        tx.send(StreamEvent::Done(ExecSummary {
            rows_affected: affected,
        }))
        .await;
        return Ok(());
    }

    let columns: Vec<Column> = stmt
        .columns()
        .iter()
        .map(|c| Column::new(c.name(), col_type(c.type_()), c.type_().name()))
        .collect();
    let types: Vec<ColType> = columns.iter().map(|c| c.col_type).collect();
    drop(stmt);

    let batch_size = opts.batch_size.max(1);
    let mut builder = BatchBuilder::new(&columns, batch_size);
    if !tx.send(StreamEvent::Schema(builder.schema())).await {
        return Ok(());
    }

    let stream = client.simple_query_raw(&sql).await.map_err(|e| map_err(&e))?;
    futures::pin_mut!(stream);
    let mut seen_first_set = false;
    while let Some(msg) = stream.next().await {
        match msg.map_err(|e| map_err(&e))? {
            SimpleQueryMessage::RowDescription(_) => {
                if seen_first_set {
                    // Only the first result set is shown.
                    break;
                }
                seen_first_set = true;
            }
            SimpleQueryMessage::Row(row) => {
                let vals = types.iter().enumerate().map(|(i, t)| match row.get(i) {
                    None => Value::Null,
                    Some(s) => parse_text(*t, s),
                });
                builder.push_row(vals);
                if builder.is_full() && !tx.send(StreamEvent::Batch(builder.finish()?)).await {
                    return Ok(());
                }
            }
            SimpleQueryMessage::CommandComplete(_) => {}
            _ => {}
        }
        if opts.cancel.is_cancelled() {
            return Err(ConnectorError::cancelled());
        }
    }
    if !builder.is_empty() && !tx.send(StreamEvent::Batch(builder.finish()?)).await {
        return Ok(());
    }
    if builder.coercion_failures() > 0 {
        tx.send(StreamEvent::Notice(format!(
            "{} value(s) could not be converted (e.g. infinity dates) and are shown as NULL",
            builder.coercion_failures()
        )))
        .await;
    }
    tx.send(StreamEvent::Done(ExecSummary::default())).await;
    Ok(())
}

#[async_trait]
impl Session for PgSession {
    fn kind(&self) -> ConnectorKind {
        ConnectorKind::Postgres
    }

    async fn server_version(&self) -> Result<String> {
        let row = self
            .client
            .query_one("select version()", &[])
            .await
            .map_err(|e| map_err(&e))?;
        Ok(row.get(0))
    }

    async fn ping(&self) -> Result<()> {
        self.client
            .simple_query("select 1")
            .await
            .map(|_| ())
            .map_err(|e| map_err(&e))
    }

    async fn execute(&self, sql: &str, opts: ExecOptions) -> Result<QueryStream> {
        let (tx, stream) = QueryStream::channel(4);
        let client = self.client.clone();
        let cancel_token = client.cancel_token();
        let tls = self.tls.clone();
        let sql = sql.to_string();
        tokio::spawn(async move {
            let cancel = opts.cancel.clone();
            let work = run(client, sql, opts, tx.clone());
            tokio::pin!(work);
            let res = tokio::select! {
                r = &mut work => r,
                _ = cancel.cancelled() => {
                    // Ask the server to cancel, then let the query finish with
                    // its cancellation error (bounded wait).
                    let _ = cancel_token.cancel_query(tls).await;
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
            .client
            .query(
                "select nspname::text, nspname = current_schema() \
                 from pg_catalog.pg_namespace \
                 where nspname !~ '^pg_(toast|temp_|toast_temp_)' \
                 order by nspname = current_schema() desc, nspname like 'pg_%', nspname = 'information_schema', nspname",
                &[],
            )
            .await
            .map_err(|e| map_err(&e))?;
        Ok(rows
            .iter()
            .map(|r| SchemaInfo {
                name: r.get(0),
                is_default: r.get::<_, Option<bool>>(1).unwrap_or(false),
                catalog: None,
            })
            .collect())
    }

    async fn schema_object_counts(&self) -> Result<Option<std::collections::HashMap<String, usize>>> {
        let rows = self
            .client
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
        let term = databrain_connector_core::search_sql_term(query);
        let rows = self
            .client
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

    async fn list_objects(&self, schema: &str) -> Result<Vec<DbObject>> {
        let rows = self
            .client
            .query(
                "select c.relname::text, c.relkind::text, \
                        obj_description(c.oid, 'pg_class'), c.reltuples::float8 \
                 from pg_catalog.pg_class c \
                 join pg_catalog.pg_namespace n on n.oid = c.relnamespace \
                 where n.nspname = $1 and c.relkind in ('r','p','v','m','f') and not c.relispartition \
                 order by c.relkind in ('v','m'), c.relname",
                &[&schema],
            )
            .await
            .map_err(|e| map_err(&e))?;
        let mut out: Vec<DbObject> = rows
            .iter()
            .map(|r| {
                let kind = match r.get::<_, String>(1).as_str() {
                    "v" => ObjectKind::View,
                    "m" => ObjectKind::MaterializedView,
                    "f" => ObjectKind::ForeignTable,
                    _ => ObjectKind::Table,
                };
                let est: Option<f64> = r.get(3);
                DbObject {
                    schema: schema.to_string(),
                    name: r.get(0),
                    kind,
                    comment: r.get(2),
                    row_estimate: est.filter(|e| *e >= 0.0).map(|e| e as i64),
                }
            })
            .collect();

        let funcs = self
            .client
            .query(
                "select p.proname::text, p.prokind::text \
                 from pg_catalog.pg_proc p join pg_catalog.pg_namespace n on n.oid = p.pronamespace \
                 where n.nspname = $1 and p.prokind in ('f','p') \
                 and n.nspname not in ('pg_catalog','information_schema') \
                 group by 1, 2 order by 1",
                &[&schema],
            )
            .await
            .map_err(|e| map_err(&e))?;
        out.extend(funcs.iter().map(|r| DbObject {
            schema: schema.to_string(),
            name: r.get(0),
            kind: if r.get::<_, String>(1) == "p" {
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
        use databrain_connector_core::{group_indexes, parse_call_list, query_text, quote_literal, truthy, TableLayout};
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
                        (select count(*) from pg_catalog.pg_inherits h where h.inhparent = c.oid) \
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
        }
        Ok(l)
    }

    async fn describe(&self, schema: &str, name: &str) -> Result<ObjectDetail> {
        let obj = self
            .client
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
            .client
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
            .map(|r| ColumnInfo {
                name: r.get(0),
                data_type: r.get(1),
                nullable: r.get(2),
                is_primary_key: r.get(3),
                default: r.get(4),
                comment: r.get(5),
            })
            .collect();

        let ddl = match kind {
            ObjectKind::View | ObjectKind::MaterializedView => {
                let def: Option<String> = self
                    .client
                    .query_one("select pg_catalog.pg_get_viewdef($1::oid, true)", &[&oid])
                    .await
                    .map_err(|e| map_err(&e))?
                    .get(0);
                let kw = if kind == ObjectKind::View {
                    "VIEW"
                } else {
                    "MATERIALIZED VIEW"
                };
                def.map(|d| {
                    format!(
                        "CREATE {kw} {}.{} AS\n{}",
                        quote(schema),
                        quote(name),
                        d.trim_end()
                    )
                })
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

    async fn schema_columns(&self, schema: &str) -> Result<Vec<TableColumns>> {
        let rows = self
            .client
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
            let col = ColumnInfo {
                name: r.get(1),
                data_type: r.get(2),
                nullable: r.get(3),
                is_primary_key: r.get(4),
                default: None,
                comment: r.get(5),
            };
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

impl PgSession {
    /// Foreign keys of one table (or all tables of the schema).
    async fn foreign_keys(&self, schema: &str, table: Option<&str>) -> Result<Vec<(String, ForeignKey)>> {
        let rows = self
            .client
            .query(
                "select c.relname::text, \
                        array(select a.attname::text from unnest(k.conkey) with ordinality u(n, o) \
                              join pg_catalog.pg_attribute a on a.attrelid = k.conrelid and a.attnum = u.n order by u.o), \
                        rn.nspname::text, rc.relname::text, \
                        array(select a.attname::text from unnest(k.confkey) with ordinality u(n, o) \
                              join pg_catalog.pg_attribute a on a.attrelid = k.confrelid and a.attnum = u.n order by u.o) \
                 from pg_catalog.pg_constraint k \
                 join pg_catalog.pg_class c on c.oid = k.conrelid \
                 join pg_catalog.pg_namespace n on n.oid = c.relnamespace \
                 join pg_catalog.pg_class rc on rc.oid = k.confrelid \
                 join pg_catalog.pg_namespace rn on rn.oid = rc.relnamespace \
                 where k.contype = 'f' and n.nspname = $1 and ($2::text is null or c.relname = $2)",
                &[&schema, &table],
            )
            .await
            .map_err(|e| map_err(&e))?;
        Ok(rows
            .iter()
            .map(|r| {
                (
                    r.get::<_, String>(0),
                    ForeignKey { columns: r.get(1), ref_schema: r.get(2), ref_table: r.get(3), ref_columns: r.get(4) },
                )
            })
            .collect())
    }
}

fn quote(s: &str) -> String {
    databrain_connector_core::quote_ident(ConnectorKind::Postgres, s)
}

/// Approximate CREATE TABLE statement from column metadata.
fn table_ddl(schema: &str, name: &str, columns: &[ColumnInfo]) -> String {
    let mut lines: Vec<String> = columns
        .iter()
        .map(|c| {
            let mut l = format!("    {} {}", quote(&c.name), c.data_type);
            if let Some(d) = &c.default {
                l.push_str(" DEFAULT ");
                l.push_str(d);
            }
            if !c.nullable {
                l.push_str(" NOT NULL");
            }
            l
        })
        .collect();
    let pk: Vec<String> = columns
        .iter()
        .filter(|c| c.is_primary_key)
        .map(|c| quote(&c.name))
        .collect();
    if !pk.is_empty() {
        lines.push(format!("    PRIMARY KEY ({})", pk.join(", ")));
    }
    format!(
        "CREATE TABLE {}.{} (\n{}\n);",
        quote(schema),
        quote(name),
        lines.join(",\n")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_timestamptz_offsets() {
        let base = parse_datetime("2024-03-05 06:04:05").unwrap();
        assert_eq!(parse_timestamptz("2024-03-05 13:04:05+07"), Some(base));
        assert_eq!(
            parse_timestamptz("2024-03-05 00:34:05-05:30"),
            Some(base)
        );
        assert_eq!(parse_timestamptz("infinity"), None);
    }

    #[test]
    fn parses_text_values() {
        assert_eq!(parse_text(ColType::Bool, "t"), Value::Bool(true));
        assert_eq!(parse_text(ColType::Int64, "42"), Value::Int(42));
        assert_eq!(
            parse_text(ColType::Binary, "\\x00ff"),
            Value::Bytes(vec![0, 255])
        );
        assert_eq!(
            parse_text(ColType::Date, "infinity"),
            Value::Text("infinity".into())
        );
    }

    /// Runs against a live server when `DATABRAIN_PG_URL`-style env vars are set:
    /// DATABRAIN_PG_HOST, DATABRAIN_PG_USER, DATABRAIN_PG_PASSWORD, DATABRAIN_PG_DB.
    #[tokio::test]
    async fn live_roundtrip() {
        let Ok(host) = std::env::var("DATABRAIN_PG_HOST") else {
            eprintln!("skipping: DATABRAIN_PG_HOST not set");
            return;
        };
        use databrain_auth::{AuthMethod, InlineCredentialSource};
        let user = std::env::var("DATABRAIN_PG_USER").unwrap_or_else(|_| "postgres".into());
        let pw = std::env::var("DATABRAIN_PG_PASSWORD").ok();
        let mut cfg = ConnectionConfig::new(ConnectorKind::Postgres, AuthMethod::Password { user: user.clone() });
        cfg.host = Some(host);
        cfg.database = std::env::var("DATABRAIN_PG_DB").ok();
        let creds = Arc::new(InlineCredentialSource::new(
            AuthMethod::Password { user },
            pw.map(secrecy::SecretString::from),
        ));
        let s = PostgresConnector.connect(&cfg, creds).await.unwrap();
        let r = s
            .execute(
                "select 1::int as a, 'x'::text as b, now() as c, 1.5::numeric as d, null::date as e",
                ExecOptions::default(),
            )
            .await
            .unwrap()
            .collect()
            .await
            .unwrap();
        assert_eq!(r.num_rows(), 1);
        let cancel = databrain_connector_core::CancellationToken::new();
        let stream = s
            .execute(
                "select pg_sleep(30)",
                ExecOptions {
                    batch_size: 10,
                    cancel: cancel.clone(),
                },
            )
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        cancel.cancel();
        assert!(stream.collect().await.unwrap_err().is_cancelled());
        assert!(!s.list_schemas().await.unwrap().is_empty());
        // Indexes + partition key for query hints.
        let run = |sql: &'static str| s.execute(sql, ExecOptions::default());
        run("drop table if exists databrain_layout_t").await.unwrap().collect().await.unwrap();
        run("create table databrain_layout_t (id int, day date not null, cust text, primary key (id, day)) partition by range (day)").await.unwrap().collect().await.unwrap();
        run("create index databrain_layout_i on databrain_layout_t (cust, lower(cust))").await.unwrap().collect().await.unwrap();
        let l = s.table_layout("public", "databrain_layout_t").await.unwrap();
        run("drop table databrain_layout_t").await.unwrap().collect().await.unwrap();
        assert_eq!(l.partition_by, vec!["day"], "{l:?}");
        assert_eq!(l.partition_kind.as_deref(), Some("range"));
        assert!(l.indexes[0].primary && l.indexes[0].columns == vec!["id", "day"], "{l:?}");
        assert_eq!(l.indexes[1].columns, vec!["cust", "lower(cust)"], "{l:?}");
    }
}
