//! SQLite connector (bundled libsqlite3, runs on blocking threads).

use std::path::Path;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use databrain_auth::{AuthMethodKind, CredentialSource};
use databrain_connector_core::value::Column;
use databrain_connector_core::{
    BatchBuilder, Capabilities, ColType, ColumnInfo, ConnectionConfig, Connector, ConnectorError,
    ConnectorInfo, ConnectorKind, DbObject, ErrorKind, ExecOptions, ExecSummary, FieldSpec, ForeignKey, ObjectDetail,
    ObjectKind, QueryStream, Result, SchemaInfo, Session, StreamEvent, StreamSender, Value,
    quote_ident,
};
use rusqlite::types::ValueRef;
use rusqlite::{OpenFlags, ffi};

#[derive(Debug, Default)]
pub struct SqliteConnector;

impl SqliteConnector {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Connector for SqliteConnector {
    fn info(&self) -> ConnectorInfo {
        ConnectorInfo {
            kind: ConnectorKind::Sqlite,
            display_name: "SQLite",
            default_port: None,
            uses_file: true,
            auth_methods: vec![AuthMethodKind::None],
            capabilities: Capabilities {
                transactions: true,
                cancel: true,
                schemas: true,
                read_only_sessions: true,
                ssh: false,
            },
            fields: vec![FieldSpec::new("file_path", "Database file").required().placeholder("/path/to/database.db")],
            note: None,
        }
    }

    async fn connect(
        &self,
        cfg: &ConnectionConfig,
        _creds: Arc<dyn CredentialSource>,
    ) -> Result<Box<dyn Session>> {
        let path = cfg
            .file_path
            .clone()
            .filter(|p| !p.trim().is_empty())
            .ok_or_else(|| ConnectorError::config("SQLite connection needs a database file path"))?;
        let read_only = cfg.read_only;
        let conn = tokio::task::spawn_blocking(move || open(&path, read_only))
            .await
            .map_err(|e| ConnectorError::internal(e.to_string()))??;
        Ok(Box::new(SqliteSession {
            interrupt: Arc::new(conn.get_interrupt_handle()),
            conn: Arc::new(Mutex::new(conn)),
        }))
    }
}

fn open(path: &str, read_only: bool) -> Result<rusqlite::Connection> {
    let is_memory = path == ":memory:";
    if !is_memory && read_only && !Path::new(path).exists() {
        return Err(ConnectorError::connection(format!(
            "database file not found: {path}"
        )));
    }
    let mut flags = OpenFlags::SQLITE_OPEN_URI | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    flags |= if read_only {
        OpenFlags::SQLITE_OPEN_READ_ONLY
    } else {
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE
    };
    let conn = rusqlite::Connection::open_with_flags(path, flags).map_err(map_err)?;
    conn.busy_timeout(std::time::Duration::from_secs(5))
        .map_err(map_err)?;
    Ok(conn)
}

fn map_err(e: rusqlite::Error) -> ConnectorError {
    match &e {
        rusqlite::Error::SqliteFailure(fe, msg) => {
            if fe.code == ffi::ErrorCode::OperationInterrupted {
                return ConnectorError::cancelled();
            }
            let text = msg.clone().unwrap_or_else(|| fe.to_string());
            ConnectorError::query(text).with_code(fe.extended_code.to_string())
        }
        rusqlite::Error::SqlInputError {
            error, msg, offset, ..
        } => ConnectorError::query(msg.clone())
            .with_code(error.extended_code.to_string())
            .with_position(u32::try_from(*offset + 1).ok()),
        _ => ConnectorError::query(e.to_string()),
    }
}

pub struct SqliteSession {
    conn: Arc<Mutex<rusqlite::Connection>>,
    interrupt: Arc<rusqlite::InterruptHandle>,
}

impl SqliteSession {
    async fn with_conn<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&rusqlite::Connection) -> Result<T> + Send + 'static,
    {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let guard = conn
                .lock()
                .map_err(|_| ConnectorError::internal("connection lock poisoned"))?;
            f(&guard)
        })
        .await
        .map_err(|e| ConnectorError::internal(e.to_string()))?
    }
}

/// Map an SQLite declared type to a column type using SQLite affinity rules.
fn affinity(decl: &str) -> Option<ColType> {
    let d = decl.to_ascii_uppercase();
    if d.is_empty() {
        None
    } else if d.contains("INT") {
        Some(ColType::Int64)
    } else if d.contains("CHAR") || d.contains("CLOB") || d.contains("TEXT") {
        Some(ColType::Utf8)
    } else if d.contains("BLOB") {
        Some(ColType::Binary)
    } else if d.contains("REAL") || d.contains("FLOA") || d.contains("DOUB") {
        Some(ColType::Float64)
    } else if d.starts_with("BOOL") {
        Some(ColType::Bool)
    } else {
        // NUMERIC affinity (DECIMAL, DATE, DATETIME...): values vary; infer.
        None
    }
}

/// Infer a column type from sample values when there is no usable affinity.
fn infer(values: impl Iterator<Item = ValueKind>) -> ColType {
    let (mut int, mut real, mut text, mut blob) = (false, false, false, false);
    for v in values {
        match v {
            ValueKind::Null => {}
            ValueKind::Int => int = true,
            ValueKind::Real => real = true,
            ValueKind::Text => text = true,
            ValueKind::Blob => blob = true,
        }
    }
    match (int, real, text, blob) {
        (_, _, false, true) if !int && !real => ColType::Binary,
        (true, false, false, false) => ColType::Int64,
        (_, true, false, false) => ColType::Float64,
        _ => ColType::Utf8,
    }
}

#[derive(Clone, Copy)]
enum ValueKind {
    Null,
    Int,
    Real,
    Text,
    Blob,
}

fn to_value(v: ValueRef<'_>) -> (Value, ValueKind) {
    match v {
        ValueRef::Null => (Value::Null, ValueKind::Null),
        ValueRef::Integer(i) => (Value::Int(i), ValueKind::Int),
        ValueRef::Real(f) => (Value::Float(f), ValueKind::Real),
        ValueRef::Text(t) => (
            Value::Text(String::from_utf8_lossy(t).into_owned()),
            ValueKind::Text,
        ),
        ValueRef::Blob(b) => (Value::Bytes(b.to_vec()), ValueKind::Blob),
    }
}

/// Runs on a blocking thread; streams rows through `tx`.
fn run_query(
    conn: &rusqlite::Connection,
    sql: &str,
    opts: &ExecOptions,
    tx: &StreamSender,
) -> Result<()> {
    let mut stmt = conn.prepare(sql).map_err(map_err)?;
    let ncols = stmt.column_count();
    if ncols == 0 {
        let n = stmt.raw_execute().map_err(map_err)?;
        tx.blocking_send(Ok(StreamEvent::Done(ExecSummary {
            rows_affected: Some(n as u64),
        })));
        return Ok(());
    }

    let meta: Vec<(String, String)> = stmt
        .columns()
        .iter()
        .map(|c| (c.name().to_string(), c.decl_type().unwrap_or("").to_string()))
        .collect();

    let batch_size = opts.batch_size.max(1);
    let mut rows = stmt.raw_query();

    // Buffer the first batch to infer types for columns without affinity.
    let mut first: Vec<Vec<Value>> = Vec::with_capacity(batch_size);
    let mut kinds: Vec<Vec<ValueKind>> = vec![Vec::new(); ncols];
    let mut exhausted = false;
    while first.len() < batch_size {
        if opts.cancel.is_cancelled() {
            return Err(ConnectorError::cancelled());
        }
        match rows.next().map_err(map_err)? {
            Some(row) => {
                let mut r = Vec::with_capacity(ncols);
                for (i, k) in kinds.iter_mut().enumerate() {
                    let (v, kind) = to_value(row.get_ref(i).map_err(map_err)?);
                    k.push(kind);
                    r.push(v);
                }
                first.push(r);
            }
            None => {
                exhausted = true;
                break;
            }
        }
    }

    let columns: Vec<Column> = meta
        .iter()
        .zip(kinds.iter())
        .map(|((name, decl), k)| {
            let t = affinity(decl).unwrap_or_else(|| infer(k.iter().copied()));
            let db_type = if decl.is_empty() { "ANY" } else { decl };
            Column::new(name.clone(), t, db_type)
        })
        .collect();

    let mut builder = BatchBuilder::new(&columns, batch_size);
    if !tx.blocking_send(Ok(StreamEvent::Schema(builder.schema()))) {
        return Ok(());
    }
    for r in first {
        builder.push_row(r);
    }

    if !exhausted {
        loop {
            if builder.is_full() && !tx.blocking_send(Ok(StreamEvent::Batch(builder.finish()?))) {
                return Ok(());
            }
            if opts.cancel.is_cancelled() {
                return Err(ConnectorError::cancelled());
            }
            match rows.next().map_err(map_err)? {
                Some(row) => {
                    let mut r = Vec::with_capacity(ncols);
                    for i in 0..ncols {
                        r.push(to_value(row.get_ref(i).map_err(map_err)?).0);
                    }
                    builder.push_row(r);
                }
                None => break,
            }
        }
    }
    if !builder.is_empty() && !tx.blocking_send(Ok(StreamEvent::Batch(builder.finish()?))) {
        return Ok(());
    }
    if builder.coercion_failures() > 0 {
        tx.blocking_send(Ok(StreamEvent::Notice(format!(
            "{} value(s) did not match the column type and are shown as NULL",
            builder.coercion_failures()
        ))));
    }
    tx.blocking_send(Ok(StreamEvent::Done(ExecSummary::default())));
    Ok(())
}

#[async_trait]
impl Session for SqliteSession {
    fn kind(&self) -> ConnectorKind {
        ConnectorKind::Sqlite
    }

    async fn server_version(&self) -> Result<String> {
        Ok(format!("SQLite {}", rusqlite::version()))
    }

    async fn ping(&self) -> Result<()> {
        self.with_conn(|c| {
            c.query_row("select 1", [], |_| Ok(()))
                .map_err(map_err)
        })
        .await
    }

    async fn execute(&self, sql: &str, opts: ExecOptions) -> Result<QueryStream> {
        let (tx, stream) = QueryStream::channel(4);
        let conn = self.conn.clone();
        let sql = sql.to_string();

        // Interrupt the running statement when cancellation is requested.
        let interrupt_token = opts.cancel.clone();
        let done = tokio_util::sync::CancellationToken::new();
        let done_guard = done.clone();
        let interrupt = self.interrupt.clone();
        tokio::spawn(async move {
            tokio::select! {
                _ = interrupt_token.cancelled() => interrupt.interrupt(),
                _ = done.cancelled() => {}
            }
        });

        tokio::task::spawn_blocking(move || {
            let _done = done_guard.drop_guard();
            let guard = match conn.lock() {
                Ok(g) => g,
                Err(_) => {
                    tx.blocking_send(Err(ConnectorError::internal("connection lock poisoned")));
                    return;
                }
            };
            if let Err(e) = run_query(&guard, &sql, &opts, &tx) {
                let e = if opts.cancel.is_cancelled() && e.kind != ErrorKind::Cancelled {
                    ConnectorError::cancelled()
                } else {
                    e
                };
                tx.blocking_send(Err(e));
            }
        });
        Ok(stream)
    }

    async fn list_schemas(&self) -> Result<Vec<SchemaInfo>> {
        self.with_conn(|c| {
            let mut stmt = c.prepare("PRAGMA database_list").map_err(map_err)?;
            let names = stmt
                .query_map([], |r| r.get::<_, String>(1))
                .map_err(map_err)?
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(map_err)?;
            Ok(names
                .into_iter()
                .filter(|n| n != "temp")
                .map(|name| SchemaInfo {
                    is_default: name == "main",
                    name,
                })
                .collect())
        })
        .await
    }

    async fn list_objects(&self, schema: &str) -> Result<Vec<DbObject>> {
        let schema = schema.to_string();
        self.with_conn(move |c| {
            let sql = format!(
                "SELECT name, type FROM {}.sqlite_master \
                 WHERE type IN ('table','view') AND name NOT LIKE 'sqlite_%' ORDER BY type, name",
                quote_ident(ConnectorKind::Sqlite, &schema)
            );
            let mut stmt = c.prepare(&sql).map_err(map_err)?;
            let rows = stmt
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
                .map_err(map_err)?
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(map_err)?;
            Ok(rows
                .into_iter()
                .map(|(name, t)| DbObject {
                    schema: schema.clone(),
                    name,
                    kind: if t == "view" {
                        ObjectKind::View
                    } else {
                        ObjectKind::Table
                    },
                    comment: None,
                    row_estimate: None,
                })
                .collect())
        })
        .await
    }

    async fn describe(&self, schema: &str, name: &str) -> Result<ObjectDetail> {
        let (schema, name) = (schema.to_string(), name.to_string());
        self.with_conn(move |c| {
            let q_schema = quote_ident(ConnectorKind::Sqlite, &schema);
            let (kind, ddl): (String, Option<String>) = c
                .query_row(
                    &format!("SELECT type, sql FROM {q_schema}.sqlite_master WHERE name = ?1"),
                    [&name],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .map_err(|e| match e {
                    rusqlite::Error::QueryReturnedNoRows => {
                        ConnectorError::query(format!("object not found: {schema}.{name}"))
                    }
                    e => map_err(e),
                })?;
            let mut stmt = c
                .prepare(&format!(
                    "PRAGMA {q_schema}.table_info({})",
                    quote_ident(ConnectorKind::Sqlite, &name)
                ))
                .map_err(map_err)?;
            let columns = stmt
                .query_map([], |r| {
                    Ok(ColumnInfo {
                        name: r.get(1)?,
                        data_type: r.get::<_, String>(2)?,
                        nullable: r.get::<_, i64>(3)? == 0,
                        default: r.get(4)?,
                        is_primary_key: r.get::<_, i64>(5)? > 0,
                        comment: None,
                    })
                })
                .map_err(map_err)?
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(map_err)?;
            let foreign_keys = sqlite_fks(c, &q_schema, &schema, &name)?;
            Ok(ObjectDetail {
                object: DbObject {
                    schema,
                    name,
                    kind: if kind == "view" {
                        ObjectKind::View
                    } else {
                        ObjectKind::Table
                    },
                    comment: None,
                    row_estimate: None,
                },
                columns,
                ddl,
                foreign_keys,
            })
        })
        .await
    }
}

fn sqlite_fks(c: &rusqlite::Connection, q_schema: &str, schema: &str, table: &str) -> Result<Vec<ForeignKey>> {
    let mut stmt = c
        .prepare(&format!("PRAGMA {q_schema}.foreign_key_list({})", quote_ident(ConnectorKind::Sqlite, table)))
        .map_err(map_err)?;
    let rows = stmt
        .query_map([], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?, r.get::<_, Option<String>>(4)?))
        })
        .map_err(map_err)?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(map_err)?;
    let mut out: Vec<(i64, ForeignKey)> = Vec::new();
    for (id, ref_table, from, to) in rows {
        match out.iter_mut().find(|(i, _)| *i == id) {
            Some((_, fk)) => {
                fk.columns.push(from);
                fk.ref_columns.push(to.unwrap_or_default());
            }
            None => out.push((
                id,
                ForeignKey { columns: vec![from], ref_schema: schema.to_string(), ref_table, ref_columns: vec![to.unwrap_or_default()] },
            )),
        }
    }
    Ok(out.into_iter().map(|(_, f)| f).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use databrain_auth::{AuthMethod, InlineCredentialSource};
    use databrain_connector_core::arrow::array::AsArray;
    use databrain_connector_core::arrow::datatypes::{DataType, Int64Type};
    use databrain_connector_core::CancellationToken;

    fn cfg(path: &str) -> ConnectionConfig {
        let mut c = ConnectionConfig::new(ConnectorKind::Sqlite, AuthMethod::None);
        c.file_path = Some(path.into());
        c
    }

    async fn session() -> Box<dyn Session> {
        let creds = Arc::new(InlineCredentialSource::new(AuthMethod::None, None));
        SqliteConnector.connect(&cfg(":memory:"), creds).await.unwrap()
    }

    async fn exec(s: &dyn Session, sql: &str) -> databrain_connector_core::Collected {
        s.execute(sql, ExecOptions::default())
            .await
            .unwrap()
            .collect()
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn query_types_and_batches() {
        let s = session().await;
        exec(
            s.as_ref(),
            "create table t(id integer primary key, name text, score real, data blob, d)",
        )
        .await;
        let r = exec(
            s.as_ref(),
            "insert into t(name, score, data, d) values ('a', 1.5, x'00ff', 1), ('b', null, null, 2.5)",
        )
        .await;
        assert_eq!(r.summary.rows_affected, Some(2));

        let r = exec(s.as_ref(), "select * from t order by id").await;
        let schema = r.schema.clone().unwrap();
        assert_eq!(schema.field(0).data_type(), &DataType::Int64);
        assert_eq!(schema.field(1).data_type(), &DataType::Utf8);
        assert_eq!(schema.field(2).data_type(), &DataType::Float64);
        assert_eq!(schema.field(3).data_type(), &DataType::Binary);
        assert_eq!(schema.field(4).data_type(), &DataType::Float64); // inferred
        assert_eq!(r.num_rows(), 2);
    }

    #[tokio::test]
    async fn batching_respects_batch_size() {
        let s = session().await;
        let opts = ExecOptions {
            batch_size: 100,
            cancel: CancellationToken::new(),
        };
        let r = s
            .execute(
                "with recursive c(x) as (select 1 union all select x+1 from c where x < 1050) select x from c",
                opts,
            )
            .await
            .unwrap()
            .collect()
            .await
            .unwrap();
        assert_eq!(r.batches.len(), 11);
        assert_eq!(r.num_rows(), 1050);
        let last = r.batches.last().unwrap().column(0).as_primitive::<Int64Type>();
        assert_eq!(last.value(last.len() - 1), 1050);
    }

    #[tokio::test]
    async fn cancel_interrupts_long_query() {
        let s = session().await;
        let cancel = CancellationToken::new();
        let opts = ExecOptions {
            batch_size: 1000,
            cancel: cancel.clone(),
        };
        let stream = s
            .execute(
                "with recursive c(x) as (select 1 union all select x+1 from c) select count(*) from c",
                opts,
            )
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        cancel.cancel();
        let err = tokio::time::timeout(std::time::Duration::from_secs(5), stream.collect())
            .await
            .expect("cancel should finish quickly")
            .unwrap_err();
        assert!(err.is_cancelled(), "{err:?}");
        // Session is still usable afterwards.
        assert_eq!(exec(s.as_ref(), "select 1").await.num_rows(), 1);
    }

    #[tokio::test]
    async fn syntax_error_has_message() {
        let s = session().await;
        let err = s
            .execute("selec 1", ExecOptions::default())
            .await
            .unwrap()
            .collect()
            .await
            .unwrap_err();
        assert_eq!(err.kind, ErrorKind::Query);
        assert!(err.message.contains("syntax error"), "{}", err.message);
    }

    #[tokio::test]
    async fn introspection() {
        let s = session().await;
        exec(s.as_ref(), "create table users(id integer primary key, email text not null)").await;
        exec(s.as_ref(), "create table orders(id integer primary key, user_id integer references users(id))").await;
        exec(s.as_ref(), "create view v_users as select email from users").await;
        let schemas = s.list_schemas().await.unwrap();
        assert!(schemas.iter().any(|x| x.name == "main" && x.is_default));
        let objs = s.list_objects("main").await.unwrap();
        assert_eq!(objs.len(), 3);
        let o = s.describe("main", "orders").await.unwrap();
        assert_eq!(o.foreign_keys[0].ref_table, "users");
        assert_eq!(o.foreign_keys[0].columns, vec!["user_id"]);
        assert_eq!(s.schema_columns("main").await.unwrap().len(), 3);
        let d = s.describe("main", "users").await.unwrap();
        assert_eq!(d.columns.len(), 2);
        assert!(d.columns[0].is_primary_key);
        assert!(!d.columns[1].nullable);
        assert!(d.ddl.unwrap().contains("CREATE TABLE users"));
    }

    #[tokio::test]
    async fn read_only_blocks_writes() {
        let dir = std::env::temp_dir().join(format!("databrain-ro-{}.db", std::process::id()));
        let path = dir.to_string_lossy().to_string();
        {
            let c = rusqlite::Connection::open(&path).unwrap();
            c.execute_batch("create table t(x int)").unwrap();
        }
        let mut c = cfg(&path);
        c.read_only = true;
        let creds = Arc::new(InlineCredentialSource::new(AuthMethod::None, None));
        let s = SqliteConnector.connect(&c, creds).await.unwrap();
        let err = s
            .execute("insert into t values (1)", ExecOptions::default())
            .await
            .unwrap()
            .collect()
            .await
            .unwrap_err();
        assert!(err.message.to_lowercase().contains("readonly"), "{}", err.message);
        let _ = std::fs::remove_file(&path);
    }
}
