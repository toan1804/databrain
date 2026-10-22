//! Neo4j connector (Bolt via `neo4rs`).
//!
//! Explorer: two "schemas", Nodes and Relationships; their objects are the
//! labels and relationship types, described from `db.schema.*Properties()`.
//! Queries are Cypher. Records become rows: scalar values typed, temporal
//! values as dates/timestamps, a returned node or relationship expanded
//! into its properties (`n.name`, `n._labels`…), lists/maps/paths as JSON.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use databrain_auth::{AuthMethodKind, Credential, CredentialSource};
use databrain_connector_core::documents::{Cell, Row, RowBatcher};
use databrain_connector_core::traits::{NEO4J_NODES, NEO4J_RELATIONSHIPS};
use databrain_connector_core::value::days_from_civil;
use databrain_connector_core::{
    Capabilities, ColumnInfo, ConnectionConfig, Connector, ConnectorError, ConnectorInfo, ConnectorKind, DbObject, ErrorKind,
    ExecOptions, ExecSummary, FieldSpec, ObjectDetail, ObjectKind, QueryStream, Result, SchemaInfo, Session, StreamEvent, Value,
    quote_ident,
};
use neo4rs::{BoltMap, BoltType, ConfigBuilder, Graph, query};
use secrecy::ExposeSecret;

#[derive(Debug, Default)]
pub struct Neo4jConnector;

#[async_trait]
impl Connector for Neo4jConnector {
    fn validate(&self, cfg: &ConnectionConfig) -> std::result::Result<(), String> {
        if cfg.opt("uri").is_none() && cfg.host.as_deref().is_none_or(|h| h.trim().is_empty()) {
            return Err("Enter a host or a connection URI (neo4j://…, neo4j+s://…, bolt://…)".into());
        }
        if cfg.ssh.is_some() && cfg.opt("uri").is_some() {
            return Err("An SSH tunnel needs the host and port; leave the connection URI empty".into());
        }
        Ok(())
    }

    fn info(&self) -> ConnectorInfo {
        ConnectorInfo {
            kind: ConnectorKind::Neo4j,
            traits: ConnectorKind::Neo4j.traits(),
            display_name: "Neo4j",
            default_port: Some(7687),
            uses_file: false,
            auth_methods: vec![AuthMethodKind::Password],
            capabilities: Capabilities { transactions: false, cancel: true, schemas: true, read_only_sessions: false, ssh: true },
            fields: vec![
                FieldSpec::new("uri", "Connection URI")
                    .placeholder("neo4j+s://xxxx.databases.neo4j.io (optional)")
                    .help("Overrides host and port. neo4j+s:// and bolt+s:// use TLS (Aura)."),
                FieldSpec::new("host", "Host").placeholder("localhost"),
                FieldSpec::new("port", "Port").placeholder("7687"),
                FieldSpec::new("database", "Database").placeholder("neo4j"),
            ],
            note: Some("Queries are Cypher: MATCH (p:Person) RETURN p.name LIMIT 10. Labels and relationship types are listed as tables."),
        }
    }

    async fn connect(&self, cfg: &ConnectionConfig, creds: Arc<dyn CredentialSource>) -> Result<Box<dyn Session>> {
        let uri = match cfg.opt("uri") {
            Some(u) => u.to_string(),
            None => format!("neo4j://{}:{}", cfg.host_or_default().trim(), cfg.port.unwrap_or(7687)),
        };
        let (user, password) = match creds.get().await? {
            Credential::Password { user, password } => (user, password.expose_secret().to_string()),
            _ => ("neo4j".to_string(), String::new()),
        };
        let database = cfg.database.clone().filter(|d| !d.trim().is_empty()).unwrap_or_else(|| "neo4j".into());
        let config = ConfigBuilder::new()
            .uri(uri)
            .user(if user.trim().is_empty() { "neo4j".to_string() } else { user })
            .password(password)
            .db(database.as_str())
            .fetch_size(500)
            .max_connections(4)
            .build()
            .map_err(|e| ConnectorError::config(e.to_string()))?;
        let graph = tokio::time::timeout(std::time::Duration::from_secs(20), Graph::connect(config))
            .await
            .map_err(|_| ConnectorError::connection("connection timed out"))?
            .map_err(err)?;
        let s = Neo4jSession { graph: Arc::new(graph) };
        s.ping().await?;
        Ok(Box::new(s))
    }
}

fn err(e: neo4rs::Error) -> ConnectorError {
    use neo4rs::Error as E;
    let msg = e.to_string();
    match e {
        E::AuthenticationError(_) => ConnectorError::new(ErrorKind::Auth, format!("Sign-in failed: {msg}")),
        E::IOError { .. } | E::ConnectionError | E::UrlParseError(_) | E::UnsupportedScheme(_) => ConnectorError::connection(msg),
        _ => ConnectorError::query(msg),
    }
}

pub struct Neo4jSession {
    graph: Arc<Graph>,
}

impl Neo4jSession {
    /// Rows of a small catalog query.
    async fn rows(&self, q: neo4rs::Query) -> Result<Vec<BoltMap>> {
        let mut s = self.graph.execute(q).await.map_err(err)?;
        let mut out = Vec::new();
        while let Some(r) = s.next().await.map_err(err)? {
            out.push(r.to_strict::<BoltMap>().map_err(|e| ConnectorError::query(e.to_string()))?);
        }
        Ok(out)
    }

    async fn strings(&self, q: neo4rs::Query, key: &str) -> Result<Vec<String>> {
        Ok(self.rows(q).await?.iter().filter_map(|m| text(m.value.get(key)?)).collect())
    }
}

fn text(v: &BoltType) -> Option<String> {
    match v {
        BoltType::String(s) => Some(s.value.clone()),
        _ => None,
    }
}

fn get<'a>(m: &'a BoltMap, k: &str) -> Option<&'a BoltType> {
    m.value.iter().find(|(key, _)| key.value == k).map(|(_, v)| v)
}

/// Column names of the last `RETURN` clause in order (Neo4j names a column
/// after its alias or its expression text): used to order a record's keys,
/// which the driver hands out unordered.
pub fn return_columns(cypher: &str) -> Vec<String> {
    let upper = cypher.to_ascii_uppercase();
    let Some(pos) = find_word_last(&upper, "RETURN") else { return vec![] };
    let mut body = &cypher[pos + 6..];
    // Up to ORDER BY / SKIP / LIMIT / UNION at depth 0.
    let b = body.as_bytes();
    let (mut depth, mut i, mut end) = (0i32, 0usize, body.len());
    while i < b.len() {
        match b[i] {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth -= 1,
            q @ (b'"' | b'\'' | b'`') => {
                i += 1;
                while i < b.len() && b[i] != q {
                    i += 1;
                }
            }
            _ if depth == 0 => {
                let rest = body[i..].to_ascii_uppercase();
                let boundary = i == 0 || !b[i - 1].is_ascii_alphanumeric();
                if boundary && ["ORDER BY", "SKIP ", "LIMIT ", "UNION"].iter().any(|k| rest.starts_with(k)) {
                    end = i;
                    break;
                }
            }
            _ => {}
        }
        i += 1;
    }
    body = body[..end].trim().trim_end_matches(';');
    let body = body.strip_prefix("DISTINCT ").or_else(|| body.strip_prefix("distinct ")).unwrap_or(body);
    split_top(body)
        .into_iter()
        .map(|item| {
            let item = item.trim();
            let up = item.to_ascii_uppercase();
            match find_word_last(&up, "AS") {
                Some(p) => item[p + 2..].trim().trim_matches('`').to_string(),
                None => item.to_string(),
            }
        })
        .filter(|s| !s.is_empty() && s != "*")
        .collect()
}

fn find_word_last(upper: &str, word: &str) -> Option<usize> {
    let b = upper.as_bytes();
    let mut best = None;
    let mut from = 0;
    while let Some(p) = upper[from..].find(word) {
        let at = from + p;
        let before = at == 0 || !(b[at - 1].is_ascii_alphanumeric() || b[at - 1] == b'_');
        let after = at + word.len() >= b.len() || !(b[at + word.len()].is_ascii_alphanumeric() || b[at + word.len()] == b'_');
        if before && after {
            best = Some(at);
        }
        from = at + word.len();
    }
    best
}

fn split_top(s: &str) -> Vec<String> {
    let (mut out, mut cur, mut depth) = (Vec::new(), String::new(), 0i32);
    let mut quote: Option<char> = None;
    for c in s.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '"' | '\'' | '`') => quote = Some(c),
            (None, '(' | '[' | '{') => depth += 1,
            (None, ')' | ']' | '}') => depth -= 1,
            (None, ',') if depth == 0 => {
                out.push(std::mem::take(&mut cur));
                continue;
            }
            _ => {}
        }
        cur.push(c);
    }
    if !cur.trim().is_empty() {
        out.push(cur);
    }
    out
}

/// A record as a row: keys in RETURN order, nodes/relationships expanded.
fn row_of(record: BoltMap, order: &[String]) -> Row {
    let mut entries: Vec<(String, BoltType)> = record.value.into_iter().map(|(k, v)| (k.value, v)).collect();
    entries.sort_by(|a, b| {
        let pa = order.iter().position(|o| *o == a.0).unwrap_or(usize::MAX);
        let pb = order.iter().position(|o| *o == b.0).unwrap_or(usize::MAX);
        pa.cmp(&pb).then_with(|| a.0.cmp(&b.0))
    });
    let single = entries.len() == 1;
    let mut out = Vec::new();
    for (k, v) in entries {
        let prefix = |p: &str| if single { p.to_string() } else { format!("{k}.{p}") };
        match v {
            BoltType::Node(n) => {
                out.push((prefix("_id"), Cell::new(Value::Int(n.id.value), "node id")));
                let labels: Vec<serde_json::Value> = n.labels.value.iter().map(json).collect();
                out.push((prefix("_labels"), Cell::json(&serde_json::Value::Array(labels), "labels")));
                out.extend(sorted_props(n.properties).into_iter().map(|(p, v)| (prefix(&p), cell(v))));
            }
            BoltType::Relation(r) => {
                out.push((prefix("_type"), Cell::new(Value::Text(r.typ.value.clone()), "relationship type")));
                out.push((prefix("_start"), Cell::new(Value::Int(r.start_node_id.value), "node id")));
                out.push((prefix("_end"), Cell::new(Value::Int(r.end_node_id.value), "node id")));
                out.extend(sorted_props(r.properties).into_iter().map(|(p, v)| (prefix(&p), cell(v))));
            }
            BoltType::UnboundedRelation(r) => {
                out.push((prefix("_type"), Cell::new(Value::Text(r.typ.value.clone()), "relationship type")));
                out.extend(sorted_props(r.properties).into_iter().map(|(p, v)| (prefix(&p), cell(v))));
            }
            v => out.push((k, cell(v))),
        }
    }
    out
}

fn sorted_props(m: BoltMap) -> Vec<(String, BoltType)> {
    let mut v: Vec<(String, BoltType)> = m.value.into_iter().map(|(k, v)| (k.value, v)).collect();
    v.sort_by(|a, b| a.0.cmp(&b.0));
    v
}

fn cell(v: BoltType) -> Cell {
    use chrono::{Datelike, NaiveDate, NaiveDateTime, NaiveTime, Timelike};
    let time_micros = |t: NaiveTime| t.num_seconds_from_midnight() as i64 * 1_000_000 + t.nanosecond() as i64 / 1000;
    let ndt_micros = |t: NaiveDateTime| t.and_utc().timestamp_micros();
    match v {
        BoltType::Null(_) => Cell::null(),
        BoltType::Boolean(b) => Cell::new(Value::Bool(b.value), "boolean"),
        BoltType::Integer(i) => Cell::new(Value::Int(i.value), "integer"),
        BoltType::Float(f) => Cell::new(Value::Float(f.value), "float"),
        BoltType::String(s) => Cell::new(Value::Text(s.value), "string"),
        BoltType::Bytes(b) => Cell::new(Value::Bytes(b.value.to_vec()), "bytes"),
        BoltType::Date(d) => match NaiveDate::try_from(&d) {
            Ok(d) => Cell::new(Value::Date(days_from_civil(d.year() as i64, d.month(), d.day()) as i32), "date"),
            Err(_) => Cell::new(Value::Text(format!("{d:?}")), "date"),
        },
        BoltType::DateTime(d) => match chrono::DateTime::<chrono::FixedOffset>::try_from(&d) {
            Ok(t) => Cell::new(Value::TimestampTz(t.timestamp_micros()), "zoned datetime"),
            Err(_) => Cell::new(Value::Text(format!("{d:?}")), "zoned datetime"),
        },
        BoltType::DateTimeZoneId(d) => match chrono::DateTime::<chrono::FixedOffset>::try_from(&d) {
            Ok(t) => Cell::new(Value::TimestampTz(t.timestamp_micros()), "zoned datetime"),
            Err(_) => Cell::new(Value::Text(format!("{d:?}")), "zoned datetime"),
        },
        BoltType::LocalDateTime(d) => match NaiveDateTime::try_from(&d) {
            Ok(t) => Cell::new(Value::Timestamp(ndt_micros(t)), "local datetime"),
            Err(_) => Cell::new(Value::Text(format!("{d:?}")), "local datetime"),
        },
        BoltType::LocalTime(t) => Cell::new(Value::Time(time_micros(NaiveTime::from(&t))), "local time"),
        BoltType::Time(t) => {
            let (nt, off) = <(NaiveTime, chrono::FixedOffset)>::from(&t);
            Cell::new(Value::Text(format!("{nt}{off}")), "zoned time")
        }
        BoltType::Duration(d) => {
            let d: std::time::Duration = d.into();
            Cell::new(Value::Text(format!("PT{}S", d.as_secs_f64())), "duration")
        }
        v @ BoltType::Node(_) => Cell::json(&json(&v), "node"),
        v @ (BoltType::Relation(_) | BoltType::UnboundedRelation(_)) => Cell::json(&json(&v), "relationship"),
        v @ BoltType::Path(_) => Cell::json(&json(&v), "path"),
        v @ BoltType::List(_) => Cell::json(&json(&v), "list"),
        v @ BoltType::Map(_) => Cell::json(&json(&v), "map"),
        v @ (BoltType::Point2D(_) | BoltType::Point3D(_)) => Cell::json(&json(&v), "point"),
    }
}

/// Any value as JSON (nested values, labels).
fn json(v: &BoltType) -> serde_json::Value {
    use serde_json::{Value as J, json};
    let map = |m: &BoltMap| -> J {
        let mut v: Vec<(&String, J)> = m.value.iter().map(|(k, v)| (&k.value, self::json(v))).collect();
        v.sort_by(|a, b| a.0.cmp(b.0));
        J::Object(v.into_iter().map(|(k, v)| (k.clone(), v)).collect())
    };
    match v {
        BoltType::Null(_) => J::Null,
        BoltType::Boolean(b) => J::Bool(b.value),
        BoltType::Integer(i) => J::from(i.value),
        BoltType::Float(f) => J::from(f.value),
        BoltType::String(s) => J::String(s.value.clone()),
        BoltType::List(l) => J::Array(l.value.iter().map(self::json).collect()),
        BoltType::Map(m) => map(m),
        BoltType::Node(n) => {
            let mut o = map(&n.properties);
            o["_id"] = J::from(n.id.value);
            o["_labels"] = J::Array(n.labels.value.iter().map(self::json).collect());
            o
        }
        BoltType::Relation(r) => {
            let mut o = map(&r.properties);
            o["_type"] = J::String(r.typ.value.clone());
            o["_start"] = J::from(r.start_node_id.value);
            o["_end"] = J::from(r.end_node_id.value);
            o
        }
        BoltType::UnboundedRelation(r) => {
            let mut o = map(&r.properties);
            o["_type"] = J::String(r.typ.value.clone());
            o
        }
        BoltType::Path(p) => json!({"nodes": self::json(&BoltType::List(p.nodes.clone())), "relationships": self::json(&BoltType::List(p.rels.clone()))}),
        BoltType::Point2D(p) => json!({"srid": p.sr_id.value, "x": p.x.value, "y": p.y.value}),
        BoltType::Point3D(p) => json!({"srid": p.sr_id.value, "x": p.x.value, "y": p.y.value, "z": p.z.value}),
        other => match cell(other.clone()).value {
            Value::Text(s) => J::String(s),
            Value::Int(i) | Value::Timestamp(i) | Value::TimestampTz(i) | Value::Time(i) => J::from(i),
            Value::Date(d) => J::from(d),
            _ => J::Null,
        },
    }
}

#[async_trait]
impl Session for Neo4jSession {
    fn kind(&self) -> ConnectorKind {
        ConnectorKind::Neo4j
    }

    async fn server_version(&self) -> Result<String> {
        let rows = self.rows(query("CALL dbms.components() YIELD name, versions, edition RETURN name, versions[0] AS version, edition")).await?;
        Ok(rows
            .first()
            .map(|m| {
                let g = |k| get(m, k).and_then(text).unwrap_or_default();
                format!("{} {} ({})", g("name"), g("version"), g("edition"))
            })
            .unwrap_or_else(|| "Neo4j".into()))
    }

    async fn ping(&self) -> Result<()> {
        self.rows(query("RETURN 1 AS ok")).await.map(|_| ())
    }

    async fn execute(&self, sql: &str, opts: ExecOptions) -> Result<QueryStream> {
        let (tx, stream) = QueryStream::channel(4);
        let graph = self.graph.clone();
        let text = sql.to_string();
        tokio::spawn(async move {
            let work = async {
                let order = return_columns(&text);
                let mut s = graph.execute(query(&text)).await.map_err(err)?;
                let mut rows = RowBatcher::new(opts.batch_size);
                let max = opts.max_rows.unwrap_or(usize::MAX);
                while let Some(r) = s.next().await.map_err(err)? {
                    if rows.rows() as usize >= max {
                        break;
                    }
                    let m = r.to_strict::<BoltMap>().map_err(|e| ConnectorError::query(e.to_string()))?;
                    for ev in rows.push(row_of(m, &order))? {
                        if !tx.send(ev).await {
                            return Ok(());
                        }
                    }
                }
                for ev in rows.finish()? {
                    tx.send(ev).await;
                }
                tx.send(StreamEvent::Done(ExecSummary { rows_affected: None })).await;
                Ok::<(), ConnectorError>(())
            };
            let r = tokio::select! {
                r = work => r,
                _ = opts.cancel.cancelled() => Err(ConnectorError::cancelled()),
            };
            if let Err(e) = r {
                tx.send_err(e).await;
            }
        });
        Ok(stream)
    }

    async fn list_schemas(&self) -> Result<Vec<SchemaInfo>> {
        Ok(vec![
            SchemaInfo { name: NEO4J_NODES.into(), is_default: true, catalog: None },
            SchemaInfo { name: NEO4J_RELATIONSHIPS.into(), is_default: false, catalog: None },
        ])
    }

    async fn list_objects(&self, schema: &str) -> Result<Vec<DbObject>> {
        let (q, key, kind) = if schema.eq_ignore_ascii_case(NEO4J_RELATIONSHIPS) {
            ("CALL db.relationshipTypes() YIELD relationshipType RETURN relationshipType AS name ORDER BY name", "name", ObjectKind::View)
        } else {
            ("CALL db.labels() YIELD label RETURN label AS name ORDER BY name", "name", ObjectKind::Table)
        };
        Ok(self
            .strings(query(q), key)
            .await?
            .into_iter()
            .map(|name| DbObject { schema: schema.to_string(), name, kind, comment: None, row_estimate: None })
            .collect())
    }

    /// Properties from the schema procedures, the count, and constraints/indexes as DDL.
    async fn describe(&self, schema: &str, name: &str) -> Result<ObjectDetail> {
        let rel = schema.eq_ignore_ascii_case(NEO4J_RELATIONSHIPS);
        let q = quote_ident(ConnectorKind::Neo4j, name);
        let props = if rel {
            self.rows(
                query("CALL db.schema.relTypeProperties() YIELD relType, propertyName, propertyTypes, mandatory WHERE relType = $t AND propertyName IS NOT NULL RETURN propertyName, propertyTypes, mandatory")
                    .param("t", format!(":{q}")),
            )
            .await
        } else {
            self.rows(
                query("CALL db.schema.nodeTypeProperties() YIELD nodeLabels, propertyName, propertyTypes, mandatory WHERE $l IN nodeLabels AND propertyName IS NOT NULL RETURN propertyName, propertyTypes, mandatory")
                    .param("l", name),
            )
            .await
        }
        .unwrap_or_default();
        let mut by_name: HashMap<String, (Vec<String>, bool)> = HashMap::new();
        for p in &props {
            let Some(n) = get(p, "propertyName").and_then(text) else { continue };
            let e = by_name.entry(n).or_insert((vec![], true));
            if let Some(BoltType::List(l)) = get(p, "propertyTypes") {
                for t in l.value.iter().filter_map(text) {
                    if !e.0.contains(&t) {
                        e.0.push(t);
                    }
                }
            }
            e.1 &= matches!(get(p, "mandatory"), Some(BoltType::Boolean(b)) if b.value);
        }
        let mut names: Vec<String> = by_name.keys().cloned().collect();
        names.sort();
        let columns = names
            .into_iter()
            .map(|n| {
                let (types, mandatory) = by_name.remove(&n).unwrap_or_default();
                ColumnInfo { name: n, data_type: types.join(" | "), nullable: !mandatory, is_primary_key: false, default: None, comment: None }
            })
            .collect();
        let count_q = if rel { format!("MATCH ()-[r:{q}]->() RETURN count(r) AS c") } else { format!("MATCH (n:{q}) RETURN count(n) AS c") };
        let row_estimate = self.rows(query(&count_q)).await.ok().and_then(|r| match r.first().and_then(|m| get(m, "c")) {
            Some(BoltType::Integer(i)) => Some(i.value),
            _ => None,
        });
        let mut ddl: Vec<String> = Vec::new();
        for show in ["SHOW CONSTRAINTS YIELD labelsOrTypes, createStatement WHERE $l IN labelsOrTypes RETURN createStatement", "SHOW INDEXES YIELD labelsOrTypes, owningConstraint, createStatement WHERE $l IN labelsOrTypes AND owningConstraint IS NULL RETURN createStatement"] {
            if let Ok(s) = self.strings(query(show).param("l", name), "createStatement").await {
                ddl.extend(s.into_iter().map(|x| format!("{x};")));
            }
        }
        let object = DbObject { schema: schema.to_string(), name: name.to_string(), kind: if rel { ObjectKind::View } else { ObjectKind::Table }, comment: None, row_estimate };
        Ok(ObjectDetail {
            object,
            columns,
            ddl: Some(if ddl.is_empty() { format!("// No constraints or indexes on {}{q}", if rel { "relationship type " } else { ":" }) } else { ddl.join("\n") }),
            foreign_keys: vec![],
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn return_columns_in_order() {
        assert_eq!(return_columns("MATCH (p:Person) RETURN p.name, p.age AS age, count(*) ORDER BY age LIMIT 5"), ["p.name", "age", "count(*)"]);
        assert_eq!(return_columns("MATCH (a)-[r]->(b) RETURN DISTINCT a, r, b"), ["a", "r", "b"]);
        assert_eq!(return_columns("RETURN {a: 1, b: [1, 2]} AS m, 'x, y' AS `odd name`"), ["m", "odd name"]);
        assert_eq!(return_columns("MATCH (n) WITH n RETURN n.returned_at"), ["n.returned_at"]);
        assert!(return_columns("CREATE (n:X)").is_empty());
    }

    #[test]
    fn records_become_rows() {
        let mut props = BoltMap::new();
        props.put("name".into(), BoltType::String("An".into()));
        props.put("age".into(), BoltType::Integer(30.into()));
        let node = neo4rs::BoltNode { id: 7.into(), labels: neo4rs::BoltList::from(vec![BoltType::String("Person".into())]), properties: props };
        let mut rec = BoltMap::new();
        rec.put("n".into(), BoltType::Node(node.clone()));
        let r = row_of(rec, &["n".into()]);
        let names: Vec<&str> = r.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(names, ["_id", "_labels", "age", "name"], "a single node: its properties are the columns");
        let mut rec = BoltMap::new();
        rec.put("total".into(), BoltType::Float(neo4rs::BoltFloat::new(1.5)));
        rec.put("n".into(), BoltType::Node(node));
        let r = row_of(rec, &["n".into(), "total".into()]);
        assert_eq!(r[0].0, "n._id");
        assert_eq!(r.last().unwrap(), &("total".to_string(), Cell::new(Value::Float(1.5), "float")));
    }

    /// Live: DATABRAIN_NEO4J_URI=neo4j://localhost:7687 DATABRAIN_NEO4J_PASSWORD=… cargo test -p databrain-connector-neo4j -- --nocapture
    #[tokio::test]
    async fn live_roundtrip() {
        let Ok(uri) = std::env::var("DATABRAIN_NEO4J_URI") else {
            return eprintln!("skipping: DATABRAIN_NEO4J_URI not set");
        };
        use databrain_auth::{AuthMethod, InlineCredentialSource};
        let user = std::env::var("DATABRAIN_NEO4J_USER").unwrap_or_else(|_| "neo4j".into());
        let mut cfg = ConnectionConfig::new(ConnectorKind::Neo4j, AuthMethod::Password { user: user.clone() });
        cfg.options.insert("uri".into(), uri);
        let creds = InlineCredentialSource::new(AuthMethod::Password { user }, std::env::var("DATABRAIN_NEO4J_PASSWORD").ok().map(secrecy::SecretString::from));
        let s = Neo4jConnector.connect(&cfg, Arc::new(creds)).await.unwrap();
        let q = |t: &str| {
            let t = t.to_string();
            let s = &s;
            async move { s.execute(&t, ExecOptions::default()).await.unwrap().collect().await }
        };
        q("MATCH (n:DbTest) DETACH DELETE n").await.unwrap();
        q("CREATE (:DbTest {name: 'An', age: 30})-[:KNOWS {since: date('2020-01-02')}]->(:DbTest {name: 'Binh', age: 41})").await.unwrap();
        let r = q("MATCH (a:DbTest)-[k:KNOWS]->(b) RETURN a.name AS from, b.name AS to, k.since AS since").await.unwrap();
        assert_eq!(r.num_rows(), 1);
        let schema = r.batches[0].schema();
        assert_eq!(schema.fields().iter().map(|f| f.name().as_str()).collect::<Vec<_>>(), ["from", "to", "since"]);
        assert_eq!(schema.field(2).data_type(), &databrain_connector_core::arrow::datatypes::DataType::Date32);
        let r = q("MATCH (n:DbTest) RETURN n ORDER BY n.name").await.unwrap();
        assert!(r.batches[0].schema().fields().iter().any(|f| f.name() == "name"));
        assert!(s.list_objects("Nodes").await.unwrap().iter().any(|o| o.name == "DbTest"));
        let d = s.describe("Nodes", "DbTest").await.unwrap();
        assert!(d.columns.iter().any(|c| c.name == "age"), "{:?}", d.columns);
        assert_eq!(d.object.row_estimate, Some(2));
        q("MATCH (n:DbTest) DETACH DELETE n").await.unwrap();
    }
}
