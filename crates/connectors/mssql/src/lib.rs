//! Microsoft SQL Server / Azure SQL connector built on `tiberius`.
//!
//! Auth: SQL login, Windows (NTLM, `DOMAIN\user`), and Microsoft Entra ID
//! tokens (browser, device code, service principal or `az login`).
//! Cancellation drops the connection (TDS attention is not supported by the
//! driver); the next statement reconnects transparently.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use databrain_auth::{
    AuthContext, AuthMethodKind, CliKind, ClientAuthStyle, Credential, CredentialSource, OAuthConfig,
};
use databrain_connector_core::value::{Column, MICROS_PER_DAY};
use databrain_connector_core::{
    BatchBuilder, Capabilities, ColType, ColumnInfo, ConnectionConfig, Connector, ConnectorError,
    ConnectorInfo, ConnectorKind, DbObject, ExecOptions, ExecSummary, FieldSpec, ForeignKey, ObjectDetail,
    ObjectKind, QueryStream, Result, SchemaInfo, Session, SslMode, StreamEvent, StreamSender, TableColumns,
    Value, quote_ident, sql::leading_keyword,
};
use futures::StreamExt;
use secrecy::ExposeSecret;
use tiberius::{AuthMethod as TdsAuth, Client, ColumnData, ColumnType, Config, EncryptionLevel, QueryItem, SqlBrowser};
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt};

type Conn = Client<Compat<TcpStream>>;

const ENTRA_SCOPE: &str = "https://database.windows.net//.default";

#[derive(Debug, Default)]
pub struct MssqlConnector;

impl MssqlConnector {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Connector for MssqlConnector {
    fn info(&self) -> ConnectorInfo {
        ConnectorInfo {
            kind: ConnectorKind::Mssql,
            display_name: "SQL Server",
            default_port: Some(1433),
            uses_file: false,
            auth_methods: vec![
                AuthMethodKind::Password,
                AuthMethodKind::OauthBrowser,
                AuthMethodKind::DeviceCode,
                AuthMethodKind::ClientCredentials,
                AuthMethodKind::CloudCli,
            ],
            capabilities: Capabilities { transactions: true, cancel: true, schemas: true, read_only_sessions: false, ssh: true },
            fields: vec![
                FieldSpec::new("host", "Host").required().placeholder("localhost or myserver.database.windows.net"),
                FieldSpec::new("port", "Port").placeholder("1433"),
                FieldSpec::new("database", "Database").placeholder("master"),
                FieldSpec::new("instance", "Instance name").placeholder("(optional, e.g. SQLEXPRESS)"),
                FieldSpec::new("tenant", "Entra tenant").placeholder("organizations").help("For Microsoft Entra ID sign-in"),
                FieldSpec::new("trust_cert", "Trust server certificate").placeholder("false"),
            ],
            note: Some("Use DOMAIN\\user for Windows (NTLM) logins. Entra ID sign-in needs an app registration client ID."),
        }
    }

    fn auth_context(&self, cfg: &ConnectionConfig) -> AuthContext {
        let p = cfg.auth.oauth_params().cloned().unwrap_or_default();
        let tenant = p.tenant.clone().or(cfg.opt("tenant").map(str::to_string)).unwrap_or_else(|| "organizations".into());
        let base = format!("https://login.microsoftonline.com/{tenant}/oauth2/v2.0");
        let mut scopes: Vec<String> = p
            .scopes
            .as_deref()
            .map(|s| s.split_whitespace().map(str::to_string).collect())
            .unwrap_or_else(|| vec![ENTRA_SCOPE.into()]);
        if !matches!(cfg.auth, databrain_auth::AuthMethod::ClientCredentials(_)) && !scopes.iter().any(|s| s == "offline_access") {
            scopes.push("offline_access".into());
        }
        AuthContext {
            oauth: Some(OAuthConfig {
                provider: "Microsoft Entra ID".into(),
                authorize_url: format!("{base}/authorize"),
                token_url: format!("{base}/token"),
                device_url: Some(format!("https://login.microsoftonline.com/{tenant}/oauth2/v2.0/devicecode")),
                revoke_url: None,
                client_id: p.client_id.unwrap_or_default(),
                client_secret: None,
                scopes,
                redirect_host: "localhost".into(),
                redirect_port: p.redirect_port,
                redirect_path: "/".into(),
                extra_authorize_params: p.user.map(|u| vec![("login_hint".to_string(), u)]).unwrap_or_default(),
                client_auth: ClientAuthStyle::Body,
            }),
            cli: Some(CliKind::Azure { resource: "https://database.windows.net/".into() }),
            google_scopes: vec![],
        }
    }

    async fn connect(&self, cfg: &ConnectionConfig, creds: Arc<dyn CredentialSource>) -> Result<Box<dyn Session>> {
        let mut c = Config::new();
        c.host(cfg.host_or_default());
        if let Some(p) = cfg.port {
            c.port(p);
        }
        if let Some(db) = cfg.database.as_deref().filter(|d| !d.is_empty()) {
            c.database(db);
        }
        if let Some(i) = cfg.opt("instance") {
            c.instance_name(i);
        }
        c.application_name("DataBrain");
        let trust = cfg.opt("trust_cert").is_some_and(|v| v.eq_ignore_ascii_case("true") || v == "1");
        match cfg.ssl_mode {
            SslMode::Disable => c.encryption(EncryptionLevel::NotSupported),
            SslMode::Prefer => {
                c.encryption(EncryptionLevel::On);
                c.trust_cert();
            }
            SslMode::Require => {
                c.encryption(EncryptionLevel::Required);
                c.trust_cert();
            }
            SslMode::VerifyFull => {
                c.encryption(EncryptionLevel::Required);
                if trust {
                    c.trust_cert();
                }
            }
        }
        if trust {
            c.trust_cert();
        }
        c.authentication(match creds.get().await? {
            #[cfg(windows)]
            Credential::Password { user, password } if user.contains('\\') => TdsAuth::windows(&user, password.expose_secret()),
            #[cfg(not(windows))]
            Credential::Password { user, .. } if user.contains('\\') => {
                return Err(ConnectorError::config("Windows (DOMAIN\\user) logins are only supported on Windows; use a SQL login or Entra ID"));
            }
            Credential::Password { user, password } => TdsAuth::sql_server(user, password.expose_secret()),
            Credential::Bearer { token, .. } => TdsAuth::aad_token(token.expose_secret()),
            _ => return Err(ConnectorError::config("unsupported authentication method for SQL Server")),
        });
        let use_browser = cfg.opt("instance").is_some() && cfg.port.is_none();
        let conn = open(&c, use_browser).await?;
        Ok(Box::new(MssqlSession { config: c, use_browser, conn: Arc::new(Mutex::new(Some(conn))) }))
    }
}

async fn open(c: &Config, use_browser: bool) -> Result<Conn> {
    let fut = async {
        let tcp = if use_browser {
            TcpStream::connect_named(c).await.map_err(map_err)?
        } else {
            TcpStream::connect(c.get_addr()).await.map_err(|e| ConnectorError::connection(e.to_string()))?
        };
        tcp.set_nodelay(true).ok();
        Client::connect(c.clone(), tcp.compat_write()).await.map_err(|e| {
            let mut e = map_err(e);
            e.kind = databrain_connector_core::ErrorKind::Connection;
            e
        })
    };
    tokio::time::timeout(Duration::from_secs(20), fut)
        .await
        .map_err(|_| ConnectorError::connection("connection timed out"))?
}

fn map_err(e: tiberius::error::Error) -> ConnectorError {
    match e {
        tiberius::error::Error::Server(t) => {
            ConnectorError::query(t.message().to_string()).with_code(format!("{} (line {})", t.code(), t.line()))
        }
        tiberius::error::Error::Io { message, .. } => ConnectorError::connection(message),
        tiberius::error::Error::Routing { host, port } => {
            ConnectorError::connection(format!("server redirected to {host}:{port}; connect to that address"))
        }
        other => ConnectorError::query(other.to_string()),
    }
}

pub struct MssqlSession {
    config: Config,
    use_browser: bool,
    conn: Arc<Mutex<Option<Conn>>>,
}

fn col_type(t: ColumnType) -> (ColType, &'static str) {
    use ColumnType as T;
    match t {
        T::Bit | T::Bitn => (ColType::Bool, "bit"),
        T::Int1 => (ColType::Int64, "tinyint"),
        T::Int2 => (ColType::Int64, "smallint"),
        T::Int4 => (ColType::Int64, "int"),
        T::Int8 => (ColType::Int64, "bigint"),
        T::Intn => (ColType::Int64, "int"),
        T::Float4 | T::Float8 | T::Floatn => (ColType::Float64, "float"),
        T::Money | T::Money4 => (ColType::Float64, "money"),
        T::Decimaln | T::Numericn => (ColType::Utf8, "decimal"),
        T::Datetime | T::Datetime4 | T::Datetimen | T::Datetime2 => (ColType::Timestamp, "datetime"),
        T::Daten => (ColType::Date, "date"),
        T::Timen => (ColType::Time, "time"),
        T::DatetimeOffsetn => (ColType::TimestampTz, "datetimeoffset"),
        T::BigVarBin | T::BigBinary | T::Image => (ColType::Binary, "varbinary"),
        T::Guid => (ColType::Utf8, "uniqueidentifier"),
        T::Xml => (ColType::Utf8, "xml"),
        T::NVarchar | T::NChar | T::NText => (ColType::Utf8, "nvarchar"),
        _ => (ColType::Utf8, "varchar"),
    }
}

const DAYS_1900: i64 = -25_567; // 1900-01-01
const DAYS_0001: i64 = -719_162; // 0001-01-01

fn time_micros(t: tiberius::time::Time) -> i64 {
    let inc = t.increments() as i128;
    let scale = t.scale() as u32;
    (inc * 1_000_000 / 10i128.pow(scale)) as i64
}

fn convert(v: ColumnData<'static>) -> Value {
    use ColumnData as D;
    match v {
        D::U8(x) => x.map(|v| Value::Int(v as i64)).unwrap_or(Value::Null),
        D::I16(x) => x.map(|v| Value::Int(v as i64)).unwrap_or(Value::Null),
        D::I32(x) => x.map(|v| Value::Int(v as i64)).unwrap_or(Value::Null),
        D::I64(x) => x.map(Value::Int).unwrap_or(Value::Null),
        D::F32(x) => x.map(|v| Value::Float(v as f64)).unwrap_or(Value::Null),
        D::F64(x) => x.map(Value::Float).unwrap_or(Value::Null),
        D::Bit(x) => x.map(Value::Bool).unwrap_or(Value::Null),
        D::String(x) => x.map(|s| Value::Text(s.into_owned())).unwrap_or(Value::Null),
        D::Guid(x) => x.map(|g| Value::Text(g.to_string().to_uppercase())).unwrap_or(Value::Null),
        D::Binary(x) => x.map(|b| Value::Bytes(b.into_owned())).unwrap_or(Value::Null),
        D::Numeric(x) => x.map(|n| Value::Text(n.to_string())).unwrap_or(Value::Null),
        D::Xml(x) => x.map(|x| Value::Text(x.into_owned().into_string())).unwrap_or(Value::Null),
        D::DateTime(x) => x
            .map(|d| {
                let micros = (d.seconds_fragments() as i64 * 1_000_000) / 300;
                Value::Timestamp((DAYS_1900 + d.days() as i64) * MICROS_PER_DAY + micros)
            })
            .unwrap_or(Value::Null),
        D::SmallDateTime(x) => x
            .map(|d| {
                Value::Timestamp((DAYS_1900 + d.days() as i64) * MICROS_PER_DAY + d.seconds_fragments() as i64 * 60_000_000)
            })
            .unwrap_or(Value::Null),
        D::Time(x) => x.map(|t| Value::Time(time_micros(t))).unwrap_or(Value::Null),
        D::Date(x) => x.map(|d| Value::Date((DAYS_0001 + d.days() as i64) as i32)).unwrap_or(Value::Null),
        D::DateTime2(x) => x
            .map(|d| Value::Timestamp((DAYS_0001 + d.date().days() as i64) * MICROS_PER_DAY + time_micros(d.time())))
            .unwrap_or(Value::Null),
        D::DateTimeOffset(x) => x
            .map(|d| {
                // Stored as UTC plus an offset.
                let dt = d.datetime2();
                Value::TimestampTz((DAYS_0001 + dt.date().days() as i64) * MICROS_PER_DAY + time_micros(dt.time()))
            })
            .unwrap_or(Value::Null),
    }
}

fn is_dml(sql: &str) -> bool {
    let kw = leading_keyword(sql);
    let writes = matches!(
        kw.as_str(),
        "INSERT" | "UPDATE" | "DELETE" | "MERGE" | "CREATE" | "ALTER" | "DROP" | "TRUNCATE" | "GRANT" | "REVOKE"
    );
    writes
        && !sql
            .split(|c: char| !c.is_ascii_alphanumeric())
            .any(|w| w.eq_ignore_ascii_case("OUTPUT"))
}

async fn run(conn: &mut Conn, sql: &str, opts: &ExecOptions, tx: &StreamSender) -> Result<()> {
    if is_dml(sql) {
        let r = conn.execute(sql, &[]).await.map_err(map_err)?;
        tx.send(StreamEvent::Done(ExecSummary { rows_affected: Some(r.rows_affected().iter().sum()) })).await;
        return Ok(());
    }
    let mut stream = conn.simple_query(sql).await.map_err(map_err)?;
    let mut builder: Option<BatchBuilder> = None;
    let mut extra_sets = 0usize;
    while let Some(item) = stream.next().await {
        match item.map_err(map_err)? {
            QueryItem::Metadata(meta) => {
                if builder.is_some() {
                    extra_sets += 1;
                    continue;
                }
                let cols: Vec<Column> = meta
                    .columns()
                    .iter()
                    .map(|c| {
                        let (t, name) = col_type(c.column_type());
                        Column::new(c.name(), t, name)
                    })
                    .collect();
                let b = BatchBuilder::new(&cols, opts.batch_size.max(1));
                if !tx.send(StreamEvent::Schema(b.schema())).await {
                    return Ok(());
                }
                builder = Some(b);
            }
            QueryItem::Row(row) => {
                if row.result_index() > 0 {
                    continue;
                }
                let Some(b) = builder.as_mut() else { continue };
                b.push_row(row.into_iter().map(convert));
                if b.is_full() && !tx.send(StreamEvent::Batch(b.finish()?)).await {
                    return Ok(());
                }
            }
        }
    }
    if let Some(b) = builder.as_mut() {
        if !b.is_empty() && !tx.send(StreamEvent::Batch(b.finish()?)).await {
            return Ok(());
        }
    }
    if extra_sets > 0 {
        tx.send(StreamEvent::Notice(format!("{extra_sets} additional result set(s) not shown"))).await;
    }
    tx.send(StreamEvent::Done(ExecSummary::default())).await;
    Ok(())
}

impl MssqlSession {
    async fn with_conn<T>(&self, f: impl for<'c> FnOnce(&'c mut Conn) -> futures::future::BoxFuture<'c, Result<T>>) -> Result<T> {
        let mut g = self.conn.lock().await;
        if g.is_none() {
            *g = Some(open(&self.config, self.use_browser).await?);
        }
        let r = f(g.as_mut().expect("connected")).await;
        if matches!(&r, Err(e) if e.kind == databrain_connector_core::ErrorKind::Connection) {
            *g = None;
        }
        r
    }

    async fn rows(&self, sql: &'static str, params: Vec<String>) -> Result<Vec<tiberius::Row>> {
        self.with_conn(move |c| {
            Box::pin(async move {
                let p: Vec<&dyn tiberius::ToSql> = params.iter().map(|s| s as &dyn tiberius::ToSql).collect();
                c.query(sql, &p).await.map_err(map_err)?.into_first_result().await.map_err(map_err)
            })
        })
        .await
    }

    async fn foreign_keys(&self, schema: &str, table: Option<&str>) -> Result<Vec<(String, ForeignKey)>> {
        let rows = self
            .rows(
                "select object_name(fk.parent_object_id), fk.name, pc.name, rs.name, rt.name, rc.name \
                 from sys.foreign_keys fk \
                 join sys.foreign_key_columns fkc on fkc.constraint_object_id = fk.object_id \
                 join sys.columns pc on pc.object_id = fkc.parent_object_id and pc.column_id = fkc.parent_column_id \
                 join sys.tables rt on rt.object_id = fkc.referenced_object_id \
                 join sys.schemas rs on rs.schema_id = rt.schema_id \
                 join sys.columns rc on rc.object_id = fkc.referenced_object_id and rc.column_id = fkc.referenced_column_id \
                 where schema_name(fk.schema_id) = @P1 and (@P2 = '' or object_name(fk.parent_object_id) = @P2) \
                 order by 1, fk.name, fkc.constraint_column_id",
                vec![schema.into(), table.unwrap_or("").into()],
            )
            .await?;
        let mut out: Vec<(String, String, ForeignKey)> = Vec::new();
        for r in rows {
            let s = |i: usize| r.get::<&str, _>(i).unwrap_or_default().to_string();
            let (t, name) = (s(0), s(1));
            match out.iter_mut().find(|(tt, n, _)| *tt == t && *n == name) {
                Some((_, _, fk)) => {
                    fk.columns.push(s(2));
                    fk.ref_columns.push(s(5));
                }
                None => out.push((t, name, ForeignKey { columns: vec![s(2)], ref_schema: s(3), ref_table: s(4), ref_columns: vec![s(5)] })),
            }
        }
        Ok(out.into_iter().map(|(t, _, f)| (t, f)).collect())
    }
}

const COLUMNS_SQL: &str = "select o.name, c.name, \
    t.name + case when t.name in ('varchar','char','varbinary','binary') then '(' + case when c.max_length = -1 then 'max' else cast(c.max_length as varchar(10)) end + ')' \
                  when t.name in ('nvarchar','nchar') then '(' + case when c.max_length = -1 then 'max' else cast(c.max_length / 2 as varchar(10)) end + ')' \
                  when t.name in ('decimal','numeric') then '(' + cast(c.precision as varchar(10)) + ',' + cast(c.scale as varchar(10)) + ')' else '' end, \
    c.is_nullable, \
    cast(case when exists (select 1 from sys.index_columns ic join sys.indexes i on i.object_id = ic.object_id and i.index_id = ic.index_id \
         where i.is_primary_key = 1 and ic.object_id = c.object_id and ic.column_id = c.column_id) then 1 else 0 end as bit), \
    object_definition(c.default_object_id), cast(ep.value as nvarchar(4000)) \
  from sys.columns c join sys.objects o on o.object_id = c.object_id join sys.types t on t.user_type_id = c.user_type_id \
  left join sys.extended_properties ep on ep.major_id = c.object_id and ep.minor_id = c.column_id and ep.name = 'MS_Description' \
  where schema_name(o.schema_id) = @P1 and o.type in ('U','V') and (@P2 = '' or o.name = @P2) \
  order by o.name, c.column_id";

fn column_from(r: &tiberius::Row) -> (String, ColumnInfo) {
    let s = |i: usize| r.get::<&str, _>(i).map(str::to_string);
    (
        s(0).unwrap_or_default(),
        ColumnInfo {
            name: s(1).unwrap_or_default(),
            data_type: s(2).unwrap_or_default(),
            nullable: r.get::<bool, _>(3).unwrap_or(true),
            is_primary_key: r.get::<bool, _>(4).unwrap_or(false),
            default: s(5),
            comment: s(6),
        },
    )
}

#[async_trait]
impl Session for MssqlSession {
    fn kind(&self) -> ConnectorKind {
        ConnectorKind::Mssql
    }

    async fn server_version(&self) -> Result<String> {
        let r = self.rows("select @@version", vec![]).await?;
        Ok(r.first()
            .and_then(|r| r.get::<&str, _>(0))
            .map(|s| s.lines().next().unwrap_or(s).trim().to_string())
            .unwrap_or_default())
    }

    async fn ping(&self) -> Result<()> {
        self.rows("select 1", vec![]).await.map(|_| ())
    }

    async fn execute(&self, sql: &str, opts: ExecOptions) -> Result<QueryStream> {
        let (tx, stream) = QueryStream::channel(4);
        let conn = self.conn.clone();
        let (config, browser) = (self.config.clone(), self.use_browser);
        let sql = sql.to_string();
        tokio::spawn(async move {
            let mut g = conn.lock().await;
            if g.is_none() {
                match open(&config, browser).await {
                    Ok(c) => *g = Some(c),
                    Err(e) => {
                        tx.send_err(e).await;
                        return;
                    }
                }
            }
            let c = g.as_mut().expect("connected");
            let res = tokio::select! {
                r = run(c, &sql, &opts, &tx) => r,
                _ = opts.cancel.cancelled() => Err(ConnectorError::cancelled()),
            };
            if let Err(e) = res {
                if e.is_cancelled() || e.kind == databrain_connector_core::ErrorKind::Connection {
                    // Connection state is unknown after an aborted read.
                    *g = None;
                }
                tx.send_err(e).await;
            }
        });
        Ok(stream)
    }

    async fn list_schemas(&self) -> Result<Vec<SchemaInfo>> {
        let rows = self
            .rows(
                "select s.name, cast(case when s.name = schema_name() then 1 else 0 end as bit) from sys.schemas s \
                 where s.name not like 'db[_]%' and s.name not in ('sys','INFORMATION_SCHEMA','guest') \
                 order by case when s.name = schema_name() then 0 else 1 end, s.name",
                vec![],
            )
            .await?;
        Ok(rows
            .iter()
            .map(|r| SchemaInfo {
                name: r.get::<&str, _>(0).unwrap_or_default().to_string(),
                is_default: r.get::<bool, _>(1).unwrap_or(false),
            })
            .collect())
    }

    async fn list_objects(&self, schema: &str) -> Result<Vec<DbObject>> {
        let rows = self
            .rows(
                "select o.name, rtrim(o.type), cast(ep.value as nvarchar(4000)), \
                   (select sum(p.rows) from sys.partitions p where p.object_id = o.object_id and p.index_id in (0,1)) \
                 from sys.objects o \
                 left join sys.extended_properties ep on ep.major_id = o.object_id and ep.minor_id = 0 and ep.name = 'MS_Description' \
                 where schema_name(o.schema_id) = @P1 and o.type in ('U','V','P','FN','IF','TF') and o.is_ms_shipped = 0 \
                 order by case rtrim(o.type) when 'U' then 0 when 'V' then 1 else 2 end, o.name",
                vec![schema.into()],
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
                    _ => ObjectKind::Function,
                },
                comment: r.get::<&str, _>(2).map(str::to_string),
                row_estimate: r.get::<i64, _>(3),
            })
            .collect())
    }

    async fn describe(&self, schema: &str, name: &str) -> Result<ObjectDetail> {
        let objs = self.list_objects(schema).await?;
        let object = objs
            .into_iter()
            .find(|o| o.name == name)
            .ok_or_else(|| ConnectorError::query(format!("object not found: {schema}.{name}")))?;
        let columns: Vec<ColumnInfo> = self
            .rows(COLUMNS_SQL, vec![schema.into(), name.into()])
            .await?
            .iter()
            .map(|r| column_from(r).1)
            .collect();
        let full = format!("{}.{}", quote_ident(ConnectorKind::Mssql, schema), quote_ident(ConnectorKind::Mssql, name));
        let ddl = match object.kind {
            ObjectKind::Table => Some(table_ddl(&full, &columns)),
            _ => self
                .rows("select object_definition(object_id(@P1))", vec![full.clone()])
                .await?
                .first()
                .and_then(|r| r.get::<&str, _>(0).map(str::to_string)),
        };
        let foreign_keys = self.foreign_keys(schema, Some(name)).await?.into_iter().map(|(_, f)| f).collect();
        Ok(ObjectDetail { object, columns, ddl, foreign_keys })
    }

    async fn schema_columns(&self, schema: &str) -> Result<Vec<TableColumns>> {
        let rows = self.rows(COLUMNS_SQL, vec![schema.into(), String::new()]).await?;
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

fn table_ddl(full: &str, columns: &[ColumnInfo]) -> String {
    let mut lines: Vec<String> = columns
        .iter()
        .map(|c| {
            let mut l = format!("    {} {}", quote_ident(ConnectorKind::Mssql, &c.name), c.data_type);
            if !c.nullable {
                l.push_str(" NOT NULL");
            }
            if let Some(d) = &c.default {
                l.push_str(" DEFAULT ");
                l.push_str(d);
            }
            l
        })
        .collect();
    let pk: Vec<String> = columns.iter().filter(|c| c.is_primary_key).map(|c| quote_ident(ConnectorKind::Mssql, &c.name)).collect();
    if !pk.is_empty() {
        lines.push(format!("    PRIMARY KEY ({})", pk.join(", ")));
    }
    format!("CREATE TABLE {full} (\n{}\n);", lines.join(",\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use databrain_connector_core::value::days_from_civil;

    #[test]
    fn converts_dates() {
        use databrain_connector_core::value::{format_date, format_timestamp};
        let d = convert(ColumnData::DateTime(Some(tiberius::time::DateTime::new(45_000, 300 * 3661))));
        let Value::Timestamp(t) = d else { panic!() };
        assert_eq!(format_timestamp(t), "2023-03-17 01:01:01");
        let Value::Date(d) = convert(ColumnData::Date(Some(tiberius::time::Date::new(738_000)))) else { panic!() };
        assert_eq!(format_date(d as i64), "2021-07-30");
        assert_eq!(days_from_civil(1900, 1, 1), DAYS_1900);
        assert_eq!(days_from_civil(1, 1, 1), DAYS_0001);
    }

    #[test]
    fn dml_detection() {
        assert!(is_dml("update t set a = 1"));
        assert!(!is_dml("insert into t output inserted.id values (1)"));
        assert!(!is_dml("select 1"));
        assert!(!is_dml("exec sp_who"));
    }

    /// Live: DATABRAIN_MSSQL_HOST, _USER, _PASSWORD (e.g. the mcr.microsoft.com/mssql/server image).
    #[tokio::test]
    async fn live_roundtrip() {
        let Ok(host) = std::env::var("DATABRAIN_MSSQL_HOST") else {
            eprintln!("skipping: DATABRAIN_MSSQL_HOST not set");
            return;
        };
        use databrain_auth::{AuthMethod, InlineCredentialSource};
        let user = std::env::var("DATABRAIN_MSSQL_USER").unwrap_or_else(|_| "sa".into());
        let mut cfg = ConnectionConfig::new(ConnectorKind::Mssql, AuthMethod::Password { user: user.clone() });
        cfg.host = Some(host);
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
        let st = s
            .execute("waitfor delay '00:00:30'", ExecOptions { batch_size: 10, cancel: cancel.clone() })
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        cancel.cancel();
        assert!(st.collect().await.unwrap_err().is_cancelled());
        assert_eq!(s.execute("select 2", ExecOptions::default()).await.unwrap().collect().await.unwrap().num_rows(), 1);
    }
}
