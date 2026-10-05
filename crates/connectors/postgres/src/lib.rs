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

    async fn search_schema(&self, schema: &str, query: &str, limit: usize) -> Result<Vec<DbObject>> {
        let term = databrain_connector_core::search_sql_term(query);
        let rows = self
            .client
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
        let term = databrain_connector_core::search_sql_term(query);
        let rows = self
            .client
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
        let q = |sql: &'static str, oid: u32| async move { self.client.query(sql, &[&oid]).await.map_err(|e| map_err(&e)) };
        if matches!(kind, ObjectKind::Function | ObjectKind::Procedure) {
            let rows = self
                .client
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
            .client
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
        let rows = self
            .client
            .query(
                "select c.relname::text, c.relkind::text, \
                        obj_description(c.oid, 'pg_class'), c.reltuples::float8 \
                 from pg_catalog.pg_class c \
                 join pg_catalog.pg_namespace n on n.oid = c.relnamespace \
                 where n.nspname = $1 and c.relkind in ('r','p','v','m','f','S') and not c.relispartition \
                 order by c.relkind in ('v','m'), c.relkind = 'S', c.relname",
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
                    "S" => ObjectKind::Sequence,
                    _ => ObjectKind::Table,
                };
                let est: Option<f64> = r.get(3);
                let est = est.filter(|_| kind != ObjectKind::Sequence);
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

    /// One catalog aggregate for every schema: relations (oid, row version,
    /// size class), columns, constraints and comments. Any DDL creates new
    /// catalog row versions (xmin), so the value changes.
    async fn schema_fingerprints(&self) -> Result<Option<std::collections::HashMap<String, String>>> {
        let rows = self
            .client
            .query(
                "with rel as ( \
                   select c.relnamespace ns, count(*) n, \
                          sum(hashtext(c.oid::text || ':' || c.xmin::text || ':' || c.relname || ':' || \
                                       floor(log(greatest(c.reltuples, 1)::numeric))::text)) h \
                   from pg_catalog.pg_class c where c.relkind in ('r','p','v','m','f') and not c.relispartition group by 1), \
                 att as ( \
                   select c.relnamespace ns, sum(hashtext(a.attrelid::text || ':' || a.attnum || ':' || a.xmin::text)) h \
                   from pg_catalog.pg_attribute a join pg_catalog.pg_class c on c.oid = a.attrelid \
                   where c.relkind in ('r','p','v','m','f') and not c.relispartition and a.attnum > 0 group by 1), \
                 con as ( \
                   select c.relnamespace ns, sum(hashtext(k.oid::text || ':' || k.xmin::text)) h \
                   from pg_catalog.pg_constraint k join pg_catalog.pg_class c on c.oid = k.conrelid group by 1), \
                 des as ( \
                   select c.relnamespace ns, sum(hashtext(d.objoid::text || ':' || d.objsubid || ':' || md5(d.description))) h \
                   from pg_catalog.pg_description d join pg_catalog.pg_class c on c.oid = d.objoid \
                   where d.classoid = 'pg_catalog.pg_class'::regclass group by 1) \
                 select n.nspname::text, concat_ws('/', coalesce(rel.n, 0), rel.h, att.h, con.h, des.h) \
                 from pg_catalog.pg_namespace n \
                 left join rel on rel.ns = n.oid left join att on att.ns = n.oid \
                 left join con on con.ns = n.oid left join des on des.ns = n.oid",
                &[],
            )
            .await
            .map_err(|e| map_err(&e))?;
        Ok(Some(rows.iter().map(|r| (r.get::<_, String>(0), r.get::<_, String>(1))).collect()))
    }

    async fn bulk_metadata(&self, schemas: &[String]) -> Result<Vec<databrain_connector_core::SchemaMetadata>> {
        match self.bulk_metadata_batched(schemas).await {
            Ok(m) => Ok(m),
            // E.g. no privilege on one catalog view: schema by schema reports per-schema errors.
            Err(_) => databrain_connector_core::default_bulk_metadata(self, schemas).await,
        }
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
    /// Three catalog queries for the whole batch of schemas (objects,
    /// columns, foreign keys), instead of three per schema.
    async fn bulk_metadata_batched(&self, schemas: &[String]) -> Result<Vec<databrain_connector_core::SchemaMetadata>> {
        let names: Vec<String> = schemas.to_vec();
        let objects = self
            .client
            .query(
                "select n.nspname::text, c.relname::text, c.relkind::text, d.description, c.reltuples::float8 \
                 from pg_catalog.pg_class c \
                 join pg_catalog.pg_namespace n on n.oid = c.relnamespace \
                 left join pg_catalog.pg_description d on d.objoid = c.oid and d.classoid = 'pg_catalog.pg_class'::regclass and d.objsubid = 0 \
                 where n.nspname::text = any($1::text[]) and c.relkind in ('r','p','v','m','f') and not c.relispartition \
                 order by 1, 2",
                &[&names],
            )
            .await
            .map_err(|e| map_err(&e))?
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
            .collect();
        let columns = self
            .client
            .query(
                "select n.nspname::text, c.relname::text, a.attname::text, pg_catalog.format_type(a.atttypid, a.atttypmod), \
                        not a.attnotnull, coalesce(a.attnum = any(i.indkey), false), d.description \
                 from pg_catalog.pg_attribute a \
                 join pg_catalog.pg_class c on c.oid = a.attrelid \
                 join pg_catalog.pg_namespace n on n.oid = c.relnamespace \
                 left join pg_catalog.pg_index i on i.indrelid = a.attrelid and i.indisprimary \
                 left join pg_catalog.pg_description d on d.objoid = a.attrelid and d.classoid = 'pg_catalog.pg_class'::regclass and d.objsubid = a.attnum \
                 where n.nspname::text = any($1::text[]) and c.relkind in ('r','p','v','m','f') and not c.relispartition \
                   and a.attnum > 0 and not a.attisdropped \
                 order by 1, 2, a.attnum",
                &[&names],
            )
            .await
            .map_err(|e| map_err(&e))?
            .iter()
            .map(|r| {
                (
                    r.get::<_, String>(0),
                    r.get::<_, String>(1),
                    ColumnInfo { name: r.get(2), data_type: r.get(3), nullable: r.get(4), is_primary_key: r.get(5), default: None, comment: r.get(6) },
                )
            })
            .collect();
        let fks = self.foreign_keys_in(&names, None).await?;
        Ok(databrain_connector_core::assemble_metadata(schemas, objects, columns, fks))
    }

    /// Foreign keys of the tables of several schemas: (schema, table, key).
    async fn foreign_keys_in(&self, schemas: &[String], table: Option<&str>) -> Result<Vec<(String, String, ForeignKey)>> {
        let rows = self
            .client
            .query(
                "select n.nspname::text, c.relname::text, \
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
                 where k.contype = 'f' and n.nspname::text = any($1::text[]) and ($2::text is null or c.relname = $2)",
                &[&schemas, &table],
            )
            .await
            .map_err(|e| map_err(&e))?;
        Ok(rows
            .iter()
            .map(|r| {
                (r.get::<_, String>(0), r.get::<_, String>(1), ForeignKey { columns: r.get(2), ref_schema: r.get(3), ref_table: r.get(4), ref_columns: r.get(5) })
            })
            .collect())
    }

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

    async fn live_session() -> Option<Box<dyn Session>> {
        let host = std::env::var("DATABRAIN_PG_HOST").ok()?;
        use databrain_auth::{AuthMethod, InlineCredentialSource};
        let user = std::env::var("DATABRAIN_PG_USER").unwrap_or_else(|_| "postgres".into());
        let mut cfg = ConnectionConfig::new(ConnectorKind::Postgres, AuthMethod::Password { user: user.clone() });
        cfg.host = Some(host);
        cfg.port = std::env::var("DATABRAIN_PG_PORT").ok().and_then(|p| p.parse().ok());
        cfg.database = std::env::var("DATABRAIN_PG_DB").ok();
        let creds = Arc::new(InlineCredentialSource::new(AuthMethod::Password { user }, std::env::var("DATABRAIN_PG_PASSWORD").ok().map(secrecy::SecretString::from)));
        Some(PostgresConnector.connect(&cfg, creds).await.unwrap())
    }

    /// Live: full table DDL (constraints, partition key, indexes, comments) that recreates the table; function DDL.
    #[tokio::test]
    async fn live_object_ddl() {
        let Some(s) = live_session().await else {
            return eprintln!("skipping: DATABRAIN_PG_HOST not set");
        };
        let s = &s;
        let run = |sql: String| async move { s.execute(&sql, ExecOptions::default()).await.unwrap().collect().await.unwrap_or_else(|e| panic!("{sql}: {e}")) };
        for stmt in [
            "drop schema if exists kb_ddl cascade",
            "create schema kb_ddl",
            "create table kb_ddl.customers (id int primary key, email text not null unique)",
            "create table kb_ddl.orders (id bigint generated always as identity, customer_id int not null references kb_ddl.customers(id) on delete cascade, \
               status text default 'new' check (status in ('new','paid')), created_at date not null, primary key (id, created_at)) partition by range (created_at)",
            "create index orders_status_idx on kb_ddl.orders (status) where status <> 'paid'",
            "comment on table kb_ddl.orders is 'Web orders'",
            "comment on column kb_ddl.orders.status is 'it''s the state'",
            "create function kb_ddl.twice(x int) returns int language sql as 'select x * 2'",
        ] {
            run(stmt.to_string()).await;
        }
        let ddl = s.object_ddl("kb_ddl", "orders", ObjectKind::Table).await.unwrap().unwrap();
        for part in ["CREATE TABLE kb_ddl.orders", "PRIMARY KEY (id, created_at)", "FOREIGN KEY (customer_id) REFERENCES kb_ddl.customers(id) ON DELETE CASCADE", "CHECK", "PARTITION BY RANGE (created_at)", "CREATE INDEX orders_status_idx", "COMMENT ON TABLE kb_ddl.orders IS 'Web orders'", "IS 'it''s the state'"] {
            assert!(ddl.contains(part), "missing {part:?} in\n{ddl}");
        }
        // It runs: drop and recreate from the DDL.
        run("drop table kb_ddl.orders".into()).await;
        for stmt in ddl.split(";\n").map(str::trim).filter(|x| !x.is_empty()) {
            run(stmt.trim_end_matches(';').to_string()).await;
        }
        assert_eq!(s.object_ddl("kb_ddl", "orders", ObjectKind::Table).await.unwrap().unwrap(), ddl, "same DDL after recreating");
        let f = s.object_ddl("kb_ddl", "twice", ObjectKind::Function).await.unwrap().unwrap();
        assert!(f.contains("CREATE OR REPLACE FUNCTION kb_ddl.twice(x integer)"), "{f}");
        assert!(s.object_ddl("kb_ddl", "customers", ObjectKind::Table).await.unwrap().unwrap().contains("UNIQUE (email)"));
        assert!(ddl.contains("id bigint GENERATED ALWAYS AS IDENTITY NOT NULL"), "{ddl}");
        // Names that need quotes keep them.
        run(r#"create table kb_ddl."Mixed Case" ("select" int, "Name" text)"#.into()).await;
        let m = s.object_ddl("kb_ddl", "Mixed Case", ObjectKind::Table).await.unwrap().unwrap();
        assert!(m.starts_with("CREATE TABLE kb_ddl.\"Mixed Case\" (\n    \"select\" integer,\n    \"Name\" text\n)"), "{m}");
        run("drop schema kb_ddl cascade".into()).await;
    }

    /// Live: routine completion (pg_proc): search path vs a given schema, procedures, ranking.
    #[tokio::test]
    async fn live_search_routines() {
        let Some(s) = live_session().await else {
            return eprintln!("skipping: DATABRAIN_PG_HOST not set");
        };
        for stmt in [
            "drop schema if exists kb_r cascade",
            "drop function if exists public.kb_total(int)",
            "create schema kb_r",
            "create function public.kb_total(x int) returns int language sql as 'select x * 2'",
            "create function kb_r.kb_total_tax(x int) returns int language sql as 'select x'",
            "create procedure kb_r.kb_refresh() language sql as 'select 1'",
        ] {
            s.execute(stmt, ExecOptions::default()).await.unwrap().collect().await.unwrap();
        }
        let names = |v: Vec<DbObject>| v.into_iter().map(|o| format!("{}.{} {:?}", o.schema, o.name, o.kind)).collect::<Vec<_>>();
        // No schema: the search path only (kb_r is not on it); no pg_catalog functions.
        assert_eq!(names(s.search_routines(None, "KB_", 50).await.unwrap()), vec!["public.kb_total Function"]);
        assert!(s.search_routines(None, "now", 50).await.unwrap().is_empty());
        assert_eq!(names(s.search_routines(Some("kb_r"), "kb", 50).await.unwrap()), vec!["kb_r.kb_refresh Procedure", "kb_r.kb_total_tax Function"]);
        for stmt in ["drop schema kb_r cascade", "drop function public.kb_total(int)"] {
            s.execute(stmt, ExecOptions::default()).await.unwrap().collect().await.unwrap();
        }
    }

    /// Live: bulk metadata (one batch query) and schema fingerprints for incremental indexing.
    #[tokio::test]
    async fn live_bulk_metadata_and_fingerprints() {
        let Some(s) = live_session().await else {
            eprintln!("skipping: DATABRAIN_PG_HOST not set");
            return;
        };
        let run = |sql: &'static str| async {
            for stmt in sql.split(';').map(str::trim).filter(|x| !x.is_empty()) {
                s.execute(stmt, ExecOptions::default()).await.unwrap().collect().await.unwrap();
            }
        };
        run("drop schema if exists kb_a cascade; drop schema if exists kb_b cascade; drop schema if exists kb_c cascade").await;
        run("create schema kb_a; create schema kb_b; create schema kb_c; \
             create table kb_a.customers (id int primary key, email text); comment on column kb_a.customers.email is 'login'; \
             comment on table kb_a.customers is 'People'; \
             create table kb_b.orders (id int primary key, customer_id int references kb_a.customers(id), total numeric(10,2)); \
             create view kb_b.big_orders as select * from kb_b.orders where total > 100; \
             create table kb_c.untouched (x int)").await;
        let m = s.bulk_metadata(&["kb_a".into(), "kb_b".into()]).await.unwrap();
        assert_eq!(m[0].objects.iter().map(|o| o.name.as_str()).collect::<Vec<_>>(), vec!["customers"]);
        assert_eq!(m[0].objects[0].comment.as_deref(), Some("People"));
        let cust = &m[0].columns[0];
        assert!(cust.columns[0].is_primary_key && cust.columns[1].comment.as_deref() == Some("login"));
        let orders = m[1].columns.iter().find(|t| t.table == "orders").unwrap();
        assert_eq!(orders.columns.iter().map(|c| c.data_type.as_str()).collect::<Vec<_>>(), vec!["integer", "integer", "numeric(10,2)"]);
        assert_eq!((orders.foreign_keys[0].ref_schema.as_str(), orders.foreign_keys[0].ref_table.as_str()), ("kb_a", "customers"));
        assert!(m[1].objects.iter().any(|o| o.name == "big_orders" && o.kind == ObjectKind::View));
        // Same result as the per-schema path.
        assert_eq!(s.schema_columns("kb_b").await.unwrap().iter().find(|t| t.table == "orders").unwrap(), orders);

        let fp = |m: &std::collections::HashMap<String, String>, k: &str| m.get(k).cloned().unwrap();
        let f0 = s.schema_fingerprints().await.unwrap().unwrap();
        assert_eq!(f0, s.schema_fingerprints().await.unwrap().unwrap(), "stable without changes");
        run("alter table kb_a.customers add column name text").await;
        let f1 = s.schema_fingerprints().await.unwrap().unwrap();
        assert_ne!(fp(&f0, "kb_a"), fp(&f1, "kb_a"), "new column");
        assert_eq!(fp(&f0, "kb_c"), fp(&f1, "kb_c"), "other schemas unchanged");
        run("comment on column kb_b.orders.total is 'gross'").await;
        let f2 = s.schema_fingerprints().await.unwrap().unwrap();
        assert_ne!(fp(&f1, "kb_b"), fp(&f2, "kb_b"), "comment");
        run("alter table kb_b.orders rename column total to amount").await;
        let f3 = s.schema_fingerprints().await.unwrap().unwrap();
        assert_ne!(fp(&f2, "kb_b"), fp(&f3, "kb_b"), "renamed column");
        run("drop view kb_b.big_orders").await;
        assert_ne!(fp(&f3, "kb_b"), fp(&s.schema_fingerprints().await.unwrap().unwrap(), "kb_b"), "dropped view");
        run("insert into kb_c.untouched values (1); analyze kb_c.untouched").await;
        assert_eq!(fp(&f3, "kb_c"), fp(&s.schema_fingerprints().await.unwrap().unwrap(), "kb_c"), "data changes are not schema changes");
        run("drop schema kb_a cascade; drop schema kb_b cascade; drop schema kb_c cascade").await;
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
        cfg.port = std::env::var("DATABRAIN_PG_PORT").ok().and_then(|p| p.parse().ok());
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
                    ..Default::default()
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
