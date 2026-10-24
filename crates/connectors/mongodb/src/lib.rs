//! MongoDB connector (official `mongodb` driver).
//!
//! Explorer: databases are the schemas, collections and views the tables.
//! Queries: mongosh-style calls or JSON database commands (see [`shell`]);
//! documents stream as rows, top-level fields as columns, nested documents
//! and arrays as JSON text (`databrain_connector_core::documents`).
//! Describe samples documents to list fields and their types.

pub mod shell;

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use databrain_auth::{AuthMethodKind, Credential, CredentialSource};
use databrain_connector_core::documents::{Cell, Row, RowBatcher};
use databrain_connector_core::{
    Capabilities, ColumnInfo, ConnectionConfig, Connector, ConnectorError, ConnectorInfo, ConnectorKind, DbObject, ExecOptions,
    ExecSummary, FieldSpec, IndexInfo, ObjectDetail, ObjectKind, QueryStream, Result, SchemaInfo, Session, StreamEvent, StreamSender,
    TableLayout, Value,
};
use futures::StreamExt;
use mongodb::bson::{Bson, Document, doc};
use mongodb::options::ClientOptions;
use secrecy::ExposeSecret;

use shell::Output;

#[derive(Debug, Default)]
pub struct MongoConnector;

const SAMPLE: i64 = 200;

#[async_trait]
impl Connector for MongoConnector {
    fn validate(&self, cfg: &ConnectionConfig) -> std::result::Result<(), String> {
        if cfg.opt("uri").is_none() && cfg.host.as_deref().is_none_or(|h| h.trim().is_empty()) {
            return Err("Enter a host or a connection string (mongodb://… or mongodb+srv://…)".into());
        }
        if cfg.ssh.is_some() && cfg.opt("uri").is_some() {
            return Err("An SSH tunnel needs the host and port; leave the connection string empty".into());
        }
        Ok(())
    }

    fn info(&self) -> ConnectorInfo {
        ConnectorInfo {
            kind: ConnectorKind::Mongodb,
            traits: ConnectorKind::Mongodb.traits(),
            display_name: "MongoDB",
            default_port: Some(27017),
            uses_file: false,
            auth_methods: vec![AuthMethodKind::Password, AuthMethodKind::None],
            capabilities: Capabilities { transactions: false, cancel: true, schemas: true, read_only_sessions: false, ssh: true },
            fields: vec![
                FieldSpec::new("uri", "Connection string")
                    .placeholder("mongodb+srv://cluster0.example.net/ (optional)")
                    .help("Overrides host and port; the user and password below are added to it. Atlas: copy it from Connect → Drivers."),
                FieldSpec::new("host", "Host").placeholder("localhost"),
                FieldSpec::new("port", "Port").placeholder("27017"),
                FieldSpec::new("database", "Database").placeholder("(all databases)").help("Default database for db.… commands; empty lists every database you can read"),
                FieldSpec::new("auth_source", "Authentication database").placeholder("admin"),
                FieldSpec::new("tls", "TLS").placeholder("false").help("true for TLS (mongodb+srv:// uses TLS already)"),
            ],
            note: Some("Queries are mongosh-style (db.orders.find({status: \"A\"}).limit(10)) or a JSON command ({\"find\": \"orders\"})."),
        }
    }

    async fn connect(&self, cfg: &ConnectionConfig, creds: Arc<dyn CredentialSource>) -> Result<Box<dyn Session>> {
        let uri = match cfg.opt("uri") {
            Some(u) => u.to_string(),
            None => {
                let host = cfg.host_or_default().trim();
                let host = if host.contains(':') && !host.starts_with('[') { format!("[{host}]") } else { host.to_string() };
                let mut u = format!("mongodb://{host}:{}/", cfg.port.unwrap_or(27017));
                if cfg.opt("tls").is_some_and(|v| v.eq_ignore_ascii_case("true") || v == "1") {
                    u.push_str("?tls=true");
                }
                u
            }
        };
        let mut o = ClientOptions::parse(&uri).await.map_err(|e| {
            // user:password@ in the string: reserved characters must be percent-encoded there.
            let has_userinfo = uri.split_once("://").is_some_and(|(_, r)| r.contains('@'));
            let hint = if has_userinfo {
                " If the password contains @ : / ? # [ ] or %, percent-encode it in the connection string (@ is %40), or leave the user and password out of the string and enter them in the fields below."
            } else {
                ""
            };
            ConnectorError::config(format!("Invalid connection string: {e}.{hint}"))
        })?;
        o.app_name = Some("DataBrain".into());
        o.connect_timeout.get_or_insert(Duration::from_secs(15));
        o.server_selection_timeout.get_or_insert(Duration::from_secs(15));
        if let Credential::Password { user, password } = creds.get().await? {
            if !user.trim().is_empty() {
                let mut c = o.credential.take().unwrap_or_default();
                c.username = Some(user);
                c.password = Some(password.expose_secret().to_string());
                if let Some(src) = cfg.opt("auth_source") {
                    c.source = Some(src.to_string());
                }
                o.credential = Some(c);
            }
        } else if let (Some(src), Some(c)) = (cfg.opt("auth_source"), o.credential.as_mut()) {
            c.source = Some(src.to_string());
        }
        let database = cfg.database.clone().filter(|d| !d.trim().is_empty()).or_else(|| o.default_database.clone());
        let client = mongodb::Client::with_options(o).map_err(err)?;
        let s = MongoSession { client, database };
        // Fail now (wrong host, password…) rather than on the first query.
        s.ping().await?;
        Ok(Box::new(s))
    }
}

fn err(e: mongodb::error::Error) -> ConnectorError {
    use mongodb::error::ErrorKind as K;
    let msg = e.to_string();
    match *e.kind {
        K::Authentication { .. } => ConnectorError::new(databrain_connector_core::ErrorKind::Auth, format!("Sign-in failed: {msg}")),
        K::ServerSelection { .. } | K::Io(_) | K::DnsResolve { .. } | K::ConnectionPoolCleared { .. } => ConnectorError::connection(msg),
        _ => ConnectorError::query(msg),
    }
}

pub struct MongoSession {
    client: mongodb::Client,
    /// The connection's database (`db.` refers to it).
    database: Option<String>,
}

impl MongoSession {
    fn db(&self, name: Option<&str>) -> Result<mongodb::Database> {
        let n = name.map(str::to_string).or_else(|| self.database.clone()).ok_or_else(|| {
            ConnectorError::query("No database chosen: start with `use <database>` or set the connection's database")
        })?;
        Ok(self.client.database(&n))
    }

    async fn sample(&self, db: &str, coll: &str) -> Result<Vec<Document>> {
        let c = self.client.database(db).collection::<Document>(coll);
        let mut cur = c.aggregate([doc! {"$sample": {"size": SAMPLE}}]).await.map_err(err)?;
        let mut out = Vec::new();
        while let Some(d) = cur.next().await {
            out.push(d.map_err(err)?);
        }
        Ok(out)
    }
}

/// Top-level fields of a document as cells.
fn row_of(d: Document) -> Row {
    d.into_iter().map(|(k, v)| (k, cell(v))).collect()
}

fn cell(v: Bson) -> Cell {
    match v {
        Bson::Null | Bson::Undefined => Cell::null(),
        Bson::Boolean(b) => Cell::new(Value::Bool(b), "bool"),
        Bson::Int32(i) => Cell::new(Value::Int(i as i64), "int"),
        Bson::Int64(i) => Cell::new(Value::Int(i), "long"),
        Bson::Double(f) => Cell::new(Value::Float(f), "double"),
        Bson::String(s) => Cell::new(Value::Text(s), "string"),
        Bson::ObjectId(o) => Cell::new(Value::Text(o.to_hex()), "objectId"),
        Bson::DateTime(t) => Cell::new(Value::TimestampTz(t.timestamp_millis().saturating_mul(1000)), "date"),
        Bson::Decimal128(d) => Cell::new(Value::Text(d.to_string()), "decimal"),
        Bson::Binary(b) => Cell::new(Value::Bytes(b.bytes), "binData"),
        Bson::Symbol(s) => Cell::new(Value::Text(s), "symbol"),
        Bson::Timestamp(t) => Cell::new(Value::Text(format!("Timestamp({}, {})", t.time, t.increment)), "timestamp"),
        Bson::RegularExpression(r) => Cell::new(Value::Text(format!("/{}/{}", r.pattern, r.options)), "regex"),
        Bson::JavaScriptCode(c) => Cell::new(Value::Text(c), "javascript"),
        Bson::MinKey => Cell::new(Value::Text("MinKey".into()), "minKey"),
        Bson::MaxKey => Cell::new(Value::Text("MaxKey".into()), "maxKey"),
        v @ Bson::Document(_) => Cell::json(&v.into_relaxed_extjson(), "object"),
        v @ Bson::Array(_) => Cell::json(&v.into_relaxed_extjson(), "array"),
        v => Cell::json(&v.into_relaxed_extjson(), "other"),
    }
}

/// MongoDB type name of a value (describe).
fn type_name(v: &Bson) -> &'static str {
    match v {
        Bson::Document(_) => "object",
        Bson::Array(_) => "array",
        other => cell(other.clone()).db_type,
    }
}

async fn emit(tx: &StreamSender, evs: Vec<StreamEvent>) -> bool {
    for e in evs {
        if !tx.send(e).await {
            return false;
        }
    }
    true
}

async fn run(s: &MongoSession, text: &str, opts: &ExecOptions, tx: &StreamSender) -> Result<()> {
    let plan = shell::parse(text, opts.max_rows).map_err(ConnectorError::query)?;
    let db = s.db(plan.db.as_deref())?;
    let mut rows = RowBatcher::new(opts.batch_size);
    let max = opts.max_rows.unwrap_or(usize::MAX);
    match plan.output {
        Output::Cursor => {
            let mut cmd = plan.command;
            if !cmd.contains_key("cursor") && cmd.contains_key("aggregate") {
                cmd.insert("cursor", doc! {});
            }
            let mut cur = db.run_cursor_command(cmd).await.map_err(err)?;
            while let Some(d) = cur.next().await {
                if rows.rows() as usize >= max {
                    break;
                }
                if !emit(tx, rows.push(row_of(d.map_err(err)?))?).await {
                    return Ok(());
                }
            }
        }
        Output::Document => {
            let mut d = db.run_command(plan.command).await.map_err(err)?;
            // Results of commands that return a cursor's first batch inline.
            if let Ok(c) = d.get_document("cursor") {
                if let Ok(batch) = c.get_array("firstBatch") {
                    for b in batch.clone() {
                        if let Bson::Document(x) = b {
                            emit(tx, rows.push(row_of(x))?).await;
                        }
                    }
                    return finish(rows, tx).await;
                }
            }
            d.remove("$clusterTime");
            d.remove("operationTime");
            emit(tx, rows.push(row_of(d))?).await;
        }
        Output::Count => {
            let d = db.run_command(plan.command).await.map_err(err)?;
            let n = d.get("n").and_then(|v| v.as_i64().or_else(|| v.as_i32().map(i64::from)).or_else(|| v.as_f64().map(|f| f as i64))).unwrap_or(0);
            emit(tx, rows.push(vec![("count".into(), Cell::new(Value::Int(n), "long"))])?).await;
        }
        Output::Distinct(field) => {
            let d = db.run_command(plan.command).await.map_err(err)?;
            for v in d.get_array("values").cloned().unwrap_or_default() {
                if rows.rows() as usize >= max {
                    break;
                }
                emit(tx, rows.push(vec![(field.clone(), cell(v))])?).await;
            }
        }
        Output::Write => {
            let d = db.run_command(plan.command).await.map_err(err)?;
            if let Ok(errs) = d.get_array("writeErrors") {
                let first = errs.first().and_then(|e| e.as_document()).and_then(|e| e.get_str("errmsg").ok()).unwrap_or("write failed");
                return Err(ConnectorError::query(format!("{first} ({} write errors)", errs.len())));
            }
            let num = |k: &str| d.get(k).and_then(|v| v.as_i32().map(i64::from).or_else(|| v.as_i64()));
            let n = num("nModified").or(num("n")).unwrap_or(0).max(0) as u64;
            if let Ok(up) = d.get_array("upserted") {
                tx.send(StreamEvent::Notice(format!("{} document(s) inserted by upsert", up.len()))).await;
            }
            tx.send(StreamEvent::Done(ExecSummary { rows_affected: Some(n) })).await;
            return Ok(());
        }
    }
    finish(rows, tx).await
}

async fn finish(rows: RowBatcher, tx: &StreamSender) -> Result<()> {
    let evs = rows.finish()?;
    emit(tx, evs).await;
    tx.send(StreamEvent::Done(ExecSummary { rows_affected: None })).await;
    Ok(())
}

#[async_trait]
impl Session for MongoSession {
    fn kind(&self) -> ConnectorKind {
        ConnectorKind::Mongodb
    }

    async fn server_version(&self) -> Result<String> {
        let d = self.client.database("admin").run_command(doc! {"buildInfo": 1}).await.map_err(err)?;
        Ok(format!("MongoDB {}", d.get_str("version").unwrap_or("?")))
    }

    async fn ping(&self) -> Result<()> {
        self.client.database("admin").run_command(doc! {"ping": 1}).await.map(|_| ()).map_err(err)
    }

    async fn execute(&self, sql: &str, opts: ExecOptions) -> Result<QueryStream> {
        let (tx, stream) = QueryStream::channel(4);
        let s = MongoSession { client: self.client.clone(), database: self.database.clone() };
        let text = sql.to_string();
        tokio::spawn(async move {
            let r = tokio::select! {
                r = run(&s, &text, &opts, &tx) => r,
                _ = opts.cancel.cancelled() => Err(ConnectorError::cancelled()),
            };
            if let Err(e) = r {
                tx.send_err(e).await;
            }
        });
        Ok(stream)
    }

    async fn list_schemas(&self) -> Result<Vec<SchemaInfo>> {
        let names = match self.client.list_database_names().authorized_databases(true).await {
            Ok(n) => n,
            // No listDatabases right: the connection's database only.
            Err(e) => match &self.database {
                Some(d) => vec![d.clone()],
                None => return Err(err(e)),
            },
        };
        let mut out: Vec<SchemaInfo> = names
            .into_iter()
            .filter(|n| !matches!(n.as_str(), "config" | "local"))
            .map(|n| SchemaInfo { is_default: self.database.as_deref() == Some(n.as_str()), name: n, catalog: None })
            .collect();
        out.sort_by(|a, b| b.is_default.cmp(&a.is_default).then((a.name == "admin").cmp(&(b.name == "admin"))).then(a.name.cmp(&b.name)));
        Ok(out)
    }

    async fn list_objects(&self, schema: &str) -> Result<Vec<DbObject>> {
        let mut cur = self.client.database(schema).list_collections().await.map_err(err)?;
        let mut out = Vec::new();
        while let Some(c) = cur.next().await {
            let c = c.map_err(err)?;
            if c.name.starts_with("system.") {
                continue;
            }
            let kind = if matches!(c.collection_type, mongodb::results::CollectionType::View) { ObjectKind::View } else { ObjectKind::Table };
            out.push(DbObject { schema: schema.to_string(), name: c.name, kind, comment: None, row_estimate: None });
        }
        out.sort_by(|a, b| a.kind.is_relation().cmp(&b.kind.is_relation()).then(a.name.cmp(&b.name)));
        out.sort_by_key(|o| o.kind == ObjectKind::View);
        Ok(out)
    }

    /// Fields found in a sample of documents, with their types.
    async fn describe(&self, schema: &str, name: &str) -> Result<ObjectDetail> {
        let objects = self.list_objects(schema).await?;
        let mut object = objects.into_iter().find(|o| o.name == name).ok_or_else(|| ConnectorError::query(format!("collection not found: {schema}.{name}")))?;
        let coll = self.client.database(schema).collection::<Document>(name);
        object.row_estimate = coll.estimated_document_count().await.ok().map(|n| n as i64);
        let docs = self.sample(schema, name).await?;
        let mut fields: Vec<(String, Vec<&'static str>, usize)> = Vec::new();
        for d in &docs {
            for (k, v) in d {
                let t = type_name(v);
                match fields.iter_mut().find(|f| f.0 == *k) {
                    Some(f) => {
                        if !f.1.contains(&t) {
                            f.1.push(t);
                        }
                        f.2 += 1;
                    }
                    None => fields.push((k.clone(), vec![t], 1)),
                }
            }
        }
        let columns = fields
            .into_iter()
            .map(|(name, types, seen)| ColumnInfo {
                is_primary_key: name == "_id",
                nullable: seen < docs.len() || types.contains(&"null"),
                data_type: types.join(" | "),
                default: None,
                comment: (seen < docs.len()).then(|| format!("in {seen} of {} sampled documents", docs.len())),
                name,
            })
            .collect();
        // DDL: the collection's options and indexes as shell commands.
        let mut ddl = String::new();
        let info = self.client.database(schema).list_collections().filter(doc! {"name": name}).await.map_err(err)?.next().await;
        let opts = info.and_then(|c| c.ok()).map(|c| mongodb::bson::to_document(&c.options).unwrap_or_default()).unwrap_or_default();
        let opts: Document = opts.into_iter().filter(|(_, v)| !matches!(v, Bson::Null)).collect();
        let q = serde_json::to_string(name).unwrap_or_default();
        ddl.push_str(&format!("db.createCollection({q}{});\n", if opts.is_empty() { String::new() } else { format!(", {}", Bson::Document(opts).into_relaxed_extjson()) }));
        if let Ok(mut ix) = coll.list_indexes().await {
            while let Some(Ok(i)) = ix.next().await {
                if i.keys.get("_id").is_some() && i.keys.len() == 1 {
                    continue;
                }
                let o: Document = mongodb::bson::to_document(&i.options).unwrap_or_default().into_iter().filter(|(_, v)| !matches!(v, Bson::Null)).collect();
                ddl.push_str(&format!(
                    "db.getCollection({q}).createIndex({}{});\n",
                    Bson::Document(i.keys).into_relaxed_extjson(),
                    if o.is_empty() { String::new() } else { format!(", {}", Bson::Document(o).into_relaxed_extjson()) }
                ));
            }
        }
        Ok(ObjectDetail { object, columns, ddl: Some(ddl), foreign_keys: vec![] })
    }

    async fn table_layout(&self, schema: &str, name: &str) -> Result<TableLayout> {
        let coll = self.client.database(schema).collection::<Document>(name);
        let mut l = TableLayout { row_estimate: coll.estimated_document_count().await.ok().map(|n| n as i64), ..Default::default() };
        let mut ix = coll.list_indexes().await.map_err(err)?;
        while let Some(i) = ix.next().await {
            let i = i.map_err(err)?;
            let o = i.options.unwrap_or_default();
            let method = i.keys.values().find_map(|v| v.as_str().map(str::to_string));
            l.indexes.push(IndexInfo {
                primary: i.keys.keys().eq(["_id"].iter()),
                name: o.name.unwrap_or_default(),
                columns: i.keys.keys().cloned().collect(),
                unique: o.unique.unwrap_or(false),
                method,
            });
        }
        Ok(l)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cells_keep_types() {
        let d = doc! {"_id": mongodb::bson::oid::ObjectId::parse_str("65a1b2c3d4e5f60718293a4b").unwrap(), "n": 3, "at": mongodb::bson::DateTime::from_millis(1000), "tags": ["a"], "addr": {"city": "HN"}};
        let r = row_of(d);
        assert_eq!(r[0].1, Cell::new(Value::Text("65a1b2c3d4e5f60718293a4b".into()), "objectId"));
        assert_eq!(r[1].1.db_type, "int");
        assert_eq!(r[2].1.value, Value::TimestampTz(1_000_000));
        assert_eq!(r[3].1, Cell::new(Value::Text("[\"a\"]".into()), "array"));
        assert_eq!(r[4].1.value, Value::Text("{\"city\":\"HN\"}".into()));
    }

    /// Live test: DATABRAIN_MONGO_URI=mongodb://localhost:27017 cargo test -p databrain-connector-mongodb -- --nocapture
    #[tokio::test]
    async fn live_roundtrip() {
        let Ok(uri) = std::env::var("DATABRAIN_MONGO_URI") else {
            return eprintln!("skipping: DATABRAIN_MONGO_URI not set");
        };
        use databrain_auth::{AuthMethod, InlineCredentialSource};
        let mut cfg = ConnectionConfig::new(ConnectorKind::Mongodb, AuthMethod::None);
        cfg.options.insert("uri".into(), uri);
        cfg.database = Some("databrain_test".into());
        let s = MongoConnector.connect(&cfg, Arc::new(InlineCredentialSource::new(AuthMethod::None, None))).await.unwrap();
        let q = |t: &str| {
            let t = t.to_string();
            let s = &s;
            async move { s.execute(&t, ExecOptions::default()).await.unwrap().collect().await }
        };
        q("db.people.drop()").await.ok();
        assert_eq!(q("db.people.insertMany([{name: 'An', age: 30, tags: ['a']}, {name: 'Binh', age: 41.5}])").await.unwrap().summary.rows_affected, Some(2));
        let r = q("db.people.find({}, {_id: 0}).sort({name: 1})").await.unwrap();
        assert_eq!(r.num_rows(), 2);
        let schema = r.batches[0].schema();
        assert_eq!(schema.field(0).name(), "name");
        assert_eq!(schema.field(1).data_type(), &databrain_connector_core::arrow::datatypes::DataType::Float64);
        assert_eq!(q("db.people.countDocuments({age: {$gt: 35}})").await.unwrap().num_rows(), 1);
        assert_eq!(q("db.people.updateMany({}, {$set: {ok: true}})").await.unwrap().summary.rows_affected, Some(2));
        assert!(s.list_schemas().await.unwrap().iter().any(|d| d.name == "databrain_test"));
        assert!(s.list_objects("databrain_test").await.unwrap().iter().any(|o| o.name == "people"));
        let d = s.describe("databrain_test", "people").await.unwrap();
        assert!(d.columns.iter().any(|c| c.name == "age" && c.data_type.contains("double")));
        assert!(d.ddl.unwrap().starts_with("db.createCollection(\"people\")"));
        q("db.people.drop()").await.unwrap();
    }
}
