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
    Capabilities, ColumnInfo, ConnectionConfig, Connector, ConnectorError, ConnectorInfo, ConnectorKind, DbObject, ErrorKind, ExecOptions, ExecSummary,
    FieldSpec, ObjectDetail, ObjectKind, QueryStream, Result, SchemaInfo, Session, StreamEvent, Value, quote_ident,
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
            capabilities: Capabilities { ssh: true },
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

    /// Nodes with label `name` (or relationships of type `name`).
    async fn count(&self, rel: bool, name: &str) -> Option<i64> {
        let q = quote_ident(ConnectorKind::Neo4j, name);
        let count_q = if rel { format!("MATCH ()-[r:{q}]->() RETURN count(r) AS c") } else { format!("MATCH (n:{q}) RETURN count(n) AS c") };
        self.rows(query(&count_q)).await.ok().and_then(|r| match r.first().and_then(|m| get(m, "c")) {
            Some(BoltType::Integer(i)) => Some(i.value),
            _ => None,
        })
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
                // Bytes, not `body[i..]`: `i` may be inside a character (`họ`).
                let starts = |k: &str| b.get(i..i + k.len()).is_some_and(|w| w.eq_ignore_ascii_case(k.as_bytes()));
                let boundary = i == 0 || !b[i - 1].is_ascii_alphanumeric();
                if boundary && ["ORDER BY", "SKIP ", "LIMIT ", "UNION"].iter().any(|k| starts(k)) {
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
/// A tag no other query carries (time and a counter).
fn query_tag() -> String {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    format!("databrain-{t:x}-{:x}", N.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
}

/// The query as sent: the user's text and, on a line of its own, a comment
/// with its tag. Neo4j lists a running transaction with its query text
/// (`SHOW TRANSACTIONS`), so Stop finds it by the tag; the comment changes
/// nothing else (the text was classified before).
fn tagged(text: &str, tag: &str) -> String {
    format!("{text}\n/* {tag} */")
}

/// End the transactions running the query tagged `tag` (`SHOW TRANSACTIONS`
/// and `TERMINATE TRANSACTION`, Neo4j 4.4+; `dbms.listTransactions` and
/// `dbms.killTransaction` before). A user may always end their own. Best
/// effort, a few seconds at most; a query that finished isn't found.
async fn terminate(graph: &Graph, tag: &str) {
    let ids = |q: &'static str| async move {
        let mut out = Vec::new();
        let mut s = graph.execute(query(q).param("tag", tag)).await.ok()?;
        while let Ok(Some(r)) = s.next().await {
            if let Ok(id) = r.get::<String>("transactionId") {
                out.push(id);
            }
        }
        Some(out)
    };
    let work = async {
        if let Some(found) = ids("SHOW TRANSACTIONS YIELD transactionId, currentQuery WHERE currentQuery CONTAINS $tag RETURN transactionId").await {
            for id in found {
                // Executed and read: `Graph::run` doesn't make the server
                // carry it out (the transaction stayed Running).
                if let Ok(mut st) = graph.execute(query("TERMINATE TRANSACTION $id").param("id", id)).await {
                    while let Ok(Some(_)) = st.next().await {}
                }
            }
        } else if let Some(found) =
            ids("CALL dbms.listTransactions() YIELD transactionId, currentQuery WHERE currentQuery CONTAINS $tag RETURN transactionId").await
        {
            for id in found {
                if let Ok(mut st) = graph.execute(query("CALL dbms.killTransaction($id)").param("id", id)).await {
                    while let Ok(Some(_)) = st.next().await {}
                }
            }
        }
    };
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), work).await;
}

/// Resolves when nobody reads the stream any more.
async fn receiver_gone(tx: &databrain_connector_core::StreamSender) {
    while !tx.is_closed() {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
}

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
        // Months, days and seconds apart, as Neo4j writes it (`P1Y2M3DT4H`):
        // the driver's conversion counts a month as 30.4 days and wraps
        // negative durations.
        BoltType::Duration(d) => Cell::new(Value::Text(iso_duration(&d)), "duration"),
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
        // Inside a list or map a value has no column type: dates, times and
        // durations as Neo4j writes them, not day or microsecond counts.
        BoltType::Bytes(b) => J::Array(b.value.iter().map(|x| J::from(*x)).collect()),
        other => J::String(temporal_text(other)),
    }
}

/// A temporal value as Neo4j writes it: `2020-01-02`,
/// `2020-01-02T03:04:05.5+01:00[Europe/Paris]`, `03:04:05Z`, `P1M2DT3S`.
fn temporal_text(v: &BoltType) -> String {
    use chrono::{DateTime, FixedOffset, NaiveDate, NaiveDateTime, NaiveTime, SecondsFormat};
    const T: &str = "%H:%M:%S%.f";
    let ok = match v {
        BoltType::Date(d) => NaiveDate::try_from(d).ok().map(|d| d.to_string()),
        BoltType::LocalDateTime(d) => NaiveDateTime::try_from(d).ok().map(|t| t.format(&format!("%Y-%m-%dT{T}")).to_string()),
        BoltType::DateTime(d) => DateTime::<FixedOffset>::try_from(d).ok().map(|t| t.to_rfc3339_opts(SecondsFormat::AutoSi, true)),
        BoltType::DateTimeZoneId(d) => {
            DateTime::<FixedOffset>::try_from(d).ok().map(|t| format!("{}[{}]", t.to_rfc3339_opts(SecondsFormat::AutoSi, true), d.tz_id()))
        }
        BoltType::LocalTime(t) => Some(NaiveTime::from(t).format(T).to_string()),
        BoltType::Time(t) => {
            let (nt, off) = <(NaiveTime, FixedOffset)>::from(t);
            Some(format!("{}{}", nt.format(T), if off.local_minus_utc() == 0 { "Z".to_string() } else { off.to_string() }))
        }
        BoltType::Duration(d) => Some(iso_duration(d)),
        _ => None,
    };
    ok.unwrap_or_else(|| format!("{v:?}"))
}

/// Months, days, seconds and nanoseconds of a duration (the driver keeps
/// them private; its `Debug` lists them in this order).
fn duration_parts(d: &neo4rs::BoltDuration) -> Option<[i64; 4]> {
    let text = format!("{d:?}");
    let mut out = [0i64; 4];
    let mut it = text.split("value: ").skip(1);
    for slot in &mut out {
        let n = it.next()?;
        let end = n.find(|c: char| !(c == '-' || c.is_ascii_digit())).unwrap_or(n.len());
        *slot = n[..end].parse().ok()?;
    }
    Some(out)
}

/// A duration as Neo4j writes it: `P1Y2M3DT4H5M6.000000007S`, `PT-1H30M`
/// (a sign per part), `PT0S`.
fn iso_duration(d: &neo4rs::BoltDuration) -> String {
    let Some([months, days, seconds, nanos]) = duration_parts(d) else { return format!("{d:?}") };
    let mut s = String::from("P");
    for (n, unit) in [(months / 12, 'Y'), (months % 12, 'M'), (days, 'D')] {
        if n != 0 {
            s.push_str(&format!("{n}{unit}"));
        }
    }
    let total = seconds as i128 * 1_000_000_000 + nanos as i128;
    if total != 0 {
        s.push('T');
        let (sign, abs) = (if total < 0 { "-" } else { "" }, total.unsigned_abs());
        let (h, rest) = (abs / 3_600_000_000_000, abs % 3_600_000_000_000);
        let (m, rest) = (rest / 60_000_000_000, rest % 60_000_000_000);
        let (sec, frac) = (rest / 1_000_000_000, rest % 1_000_000_000);
        if h != 0 {
            s.push_str(&format!("{sign}{h}H"));
        }
        if m != 0 {
            s.push_str(&format!("{sign}{m}M"));
        }
        if sec != 0 || frac != 0 {
            let frac = if frac == 0 { String::new() } else { format!(".{frac:09}").trim_end_matches('0').to_string() };
            s.push_str(&format!("{sign}{sec}{frac}S"));
        }
    }
    if s == "P" { "PT0S".into() } else { s }
}

#[async_trait]
impl Session for Neo4jSession {
    fn kind(&self) -> ConnectorKind {
        ConnectorKind::Neo4j
    }

    /// Each query runs in its own auto-commit transaction.
    fn transaction_state(&self) -> databrain_connector_core::TxState {
        databrain_connector_core::TxState::None
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

    async fn execute(&self, stmt: databrain_connector_core::Statement<'_>, opts: ExecOptions) -> Result<QueryStream> {
        let sql = stmt.sql();
        let (tx, stream) = QueryStream::channel(4);
        let graph = self.graph.clone();
        let text = sql.to_string();
        tokio::spawn(async move {
            // Its tag on the server: Stop (or nobody reading any more) ends
            // the transaction there, not only the stream here.
            let tag = query_tag();
            let work = async {
                let order = return_columns(&text);
                let mut s = graph.execute(query(&tagged(&text, &tag))).await.map_err(err)?;
                let mut rows = RowBatcher::new(opts.batch_size);
                let max = opts.max_rows.unwrap_or(usize::MAX);
                while let Some(r) = s.next().await.map_err(err)? {
                    if rows.rows() as usize >= max {
                        // A read stops on the server; a write runs to its end
                        // (its rows past the limit are dropped), never cut off.
                        if databrain_connector_core::sql::classify(&text, ConnectorKind::Neo4j).kind == databrain_connector_core::sql::StatementKind::Read {
                            drop(s);
                            terminate(&graph, &tag).await;
                        } else {
                            while s.next().await.map_err(err)?.is_some() {}
                            // `max` is the row limit plus one (the engine asks for one more to tell it was cut).
                            tx.send(StreamEvent::Notice(format!("The statement ran to its end; only its first {} rows are kept.", max.saturating_sub(1))))
                                .await;
                        }
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
                _ = opts.cancel.cancelled() => {
                    terminate(&graph, &tag).await;
                    Err(ConnectorError::cancelled())
                }
                _ = receiver_gone(&tx) => {
                    terminate(&graph, &tag).await;
                    return;
                }
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
    /// Nodes with the label / relationships of the type: from the count store, no scan.
    async fn table_layout(&self, schema: &str, name: &str) -> Result<databrain_connector_core::TableLayout> {
        Ok(databrain_connector_core::TableLayout {
            row_estimate: self.count(schema.eq_ignore_ascii_case(NEO4J_RELATIONSHIPS), name).await,
            ..Default::default()
        })
    }

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
        let row_estimate = self.count(rel, name).await;
        let mut ddl: Vec<String> = Vec::new();
        for show in [
            "SHOW CONSTRAINTS YIELD labelsOrTypes, createStatement WHERE $l IN labelsOrTypes RETURN createStatement",
            "SHOW INDEXES YIELD labelsOrTypes, owningConstraint, createStatement WHERE $l IN labelsOrTypes AND owningConstraint IS NULL RETURN createStatement",
        ] {
            if let Ok(s) = self.strings(query(show).param("l", name), "createStatement").await {
                ddl.extend(s.into_iter().map(|x| format!("{x};")));
            }
        }
        let object = DbObject {
            schema: schema.to_string(),
            name: name.to_string(),
            kind: if rel { ObjectKind::View } else { ObjectKind::Table },
            comment: None,
            row_estimate,
        };
        Ok(ObjectDetail {
            object,
            columns,
            ddl: Some(if ddl.is_empty() {
                format!("// No constraints or indexes on {}{q}", if rel { "relationship type " } else { ":" })
            } else {
                ddl.join("\n")
            }),
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
        // Names outside ASCII are read, not a crash.
        assert_eq!(return_columns("MATCH (n) RETURN n.họ_tên, n.tuổi AS tuổi ORDER BY tuổi LIMIT 5"), ["n.họ_tên", "tuổi"]);
        assert_eq!(return_columns("MATCH (n:Người) RETURN n.`tên đầy đủ` AS `tên`, '日本, 語' AS s"), ["tên", "s"]);
        for q in ["RETURN é", "RETURN 'ê", "MATCH (n) RETURN n.🙂 limit 1", "RETURN\u{a0}1", "é RETURN"] {
            let _ = return_columns(q);
        }
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

    fn duration(months: i64, days: i64, seconds: i64, nanos: i64) -> neo4rs::BoltDuration {
        neo4rs::BoltDuration::new(months.into(), days.into(), seconds.into(), nanos.into())
    }

    /// Durations keep their months and days, and their sign.
    #[test]
    fn durations_as_neo4j_writes_them() {
        assert_eq!(iso_duration(&duration(14, 3, 3723, 5)), "P1Y2M3DT1H2M3.000000005S");
        assert_eq!(iso_duration(&duration(1, 0, 0, 0)), "P1M", "a month is not 30.4 days");
        assert_eq!(iso_duration(&duration(0, 0, -5400, 0)), "PT-1H-30M", "not wrapped around");
        assert_eq!(iso_duration(&duration(0, 0, 0, 500_000_000)), "PT0.5S");
        assert_eq!(iso_duration(&duration(0, 0, 0, 0)), "PT0S");
        assert_eq!(cell(BoltType::Duration(duration(0, 2, 60, 0))), Cell::new(Value::Text("P2DT1M".into()), "duration"));
    }

    /// Dates, times and durations inside a list or a map are written as
    /// Neo4j writes them, not as day or microsecond counts.
    #[test]
    fn temporal_values_inside_lists_and_maps() {
        use chrono::{FixedOffset, NaiveDate, NaiveTime, TimeZone};
        let day = NaiveDate::from_ymd_opt(2020, 1, 2).unwrap();
        let at = day.and_hms_milli_opt(3, 4, 5, 500).unwrap();
        let paris = FixedOffset::east_opt(3600).unwrap();
        let list = BoltType::List(neo4rs::BoltList::from(vec![
            BoltType::Date(day.into()),
            BoltType::LocalDateTime(at.into()),
            BoltType::DateTime(paris.from_local_datetime(&at).unwrap().into()),
            BoltType::LocalTime(NaiveTime::from_hms_opt(3, 4, 5).unwrap().into()),
            BoltType::Time((NaiveTime::from_hms_opt(3, 4, 5).unwrap(), paris).into()),
            BoltType::Duration(duration(1, 2, 3, 0)),
        ]));
        assert_eq!(
            json(&list),
            serde_json::json!(["2020-01-02", "2020-01-02T03:04:05.500", "2020-01-02T03:04:05.500+01:00", "03:04:05", "03:04:05+01:00", "P1M2DT3S"])
        );
        let mut m = BoltMap::new();
        m.put("since".into(), BoltType::Date(day.into()));
        m.put("zoned".into(), BoltType::DateTimeZoneId((at, "Europe/Paris").into()));
        let j = json(&BoltType::Map(m));
        assert_eq!(j["since"], "2020-01-02");
        assert!(j["zoned"].as_str().unwrap().ends_with("[Europe/Paris]"), "{j}");
    }

    /// The query sent carries its tag in a comment on a line of its own (a
    /// `//` comment ending the text can't swallow it); tags differ.
    #[test]
    fn queries_carry_their_tag() {
        let (a, b) = (query_tag(), query_tag());
        assert_ne!(a, b);
        assert_eq!(tagged("MATCH (n) RETURN n // all", &a), format!("MATCH (n) RETURN n // all\n/* {a} */"));
    }

    /// Live: Stop and the row limit end a read on the server (it ran on
    /// before); a write at the row limit runs to its end; a finished query
    /// is left alone.
    #[ignore = "live: needs DATABRAIN_NEO4J_URI (cargo test -- --ignored)"]
    #[tokio::test]
    async fn live_stop_ends_the_transaction_on_the_server() {
        let Ok(uri) = std::env::var("DATABRAIN_NEO4J_URI") else {
            panic!("live test: DATABRAIN_NEO4J_URI not set (run with --ignored)");
        };
        use databrain_auth::{AuthMethod, InlineCredentialSource};
        let user = std::env::var("DATABRAIN_NEO4J_USER").unwrap_or_else(|_| "neo4j".into());
        let mut cfg = ConnectionConfig::new(ConnectorKind::Neo4j, AuthMethod::Password { user: user.clone() });
        cfg.options.insert("uri".into(), uri);
        let creds = InlineCredentialSource::new(AuthMethod::Password { user }, std::env::var("DATABRAIN_NEO4J_PASSWORD").ok().map(secrecy::SecretString::from));
        let s = Neo4jConnector.connect(&cfg, Arc::new(creds)).await.unwrap();
        let s = &s;
        // Transactions running a query with this text in it (not this one: its text holds only `$t`).
        let running = |text: &'static str| async move {
            let r = s
                .execute(
                    databrain_connector_core::Statement::internal(&format!(
                        "SHOW TRANSACTIONS YIELD currentQuery WHERE currentQuery CONTAINS '{text}' AND NOT currentQuery CONTAINS 'SHOW TRANSACTIONS' RETURN count(*) AS n"
                    )),
                    ExecOptions::default(),
                )
                .await
                .unwrap()
                .collect()
                .await
                .unwrap();
            databrain_connector_core::arrow::util::display::ArrayFormatter::try_new(r.batches[0].column(0).as_ref(), &Default::default())
                .unwrap()
                .value(0)
                .to_string()
        };
        let gone = |text: &'static str| async move {
            for _ in 0..20 {
                if running(text).await == "0" {
                    return true;
                }
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            }
            false
        };
        // A long read, stopped: ended on the server.
        let long = "UNWIND range(1, 4000000000) AS slowread RETURN count(slowread) AS n";
        let cancel = databrain_connector_core::CancellationToken::new();
        let stream =
            s.execute(databrain_connector_core::Statement::internal(long), ExecOptions { cancel: cancel.clone(), ..Default::default() }).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        assert_eq!(running("slowread").await, "1", "the read runs");
        cancel.cancel();
        let e = tokio::time::timeout(std::time::Duration::from_secs(10), stream.collect()).await.expect("stopped").unwrap_err();
        assert_eq!(e.kind, ErrorKind::Cancelled);
        assert!(gone("slowread").await, "the read still runs on the server");
        // The termination itself, on a query whose connection stays open
        // (the server never sees it dropped): only its tag ends it.
        let graph = Arc::new(
            Graph::connect(
                ConfigBuilder::new()
                    .uri(std::env::var("DATABRAIN_NEO4J_URI").unwrap())
                    .user(std::env::var("DATABRAIN_NEO4J_USER").unwrap_or_else(|_| "neo4j".into()))
                    .password(std::env::var("DATABRAIN_NEO4J_PASSWORD").unwrap_or_default())
                    .build()
                    .unwrap(),
            )
            .await
            .unwrap(),
        );
        let tag = query_tag();
        let held = {
            let (graph, text) = (graph.clone(), tagged("UNWIND range(1, 4000000000) AS keptopen RETURN count(keptopen) AS n", &tag));
            tokio::spawn(async move {
                let mut st = graph.execute(query(&text)).await?;
                st.next().await.map(|_| ())
            })
        };
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        assert_eq!(running("keptopen").await, "1");
        terminate(&graph, &tag).await;
        // Terminated on the server: gone, or marked so (a tight loop like
        // this one notices it at its next check).
        let status = || async {
            let mut st = graph
                .execute(query(
                    "SHOW TRANSACTIONS YIELD currentQuery, status WHERE currentQuery CONTAINS 'keptopen' AND NOT currentQuery CONTAINS 'SHOW' RETURN status",
                ))
                .await
                .unwrap();
            let mut out = Vec::new();
            while let Ok(Some(r)) = st.next().await {
                out.push(r.get::<String>("status").unwrap_or_default());
            }
            out
        };
        let mut ok = false;
        for _ in 0..20 {
            let st = status().await;
            if st.iter().all(|s| s.starts_with("Terminated")) {
                ok = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
        assert!(ok, "still running on the server: {:?}", status().await);
        held.abort();
        // The row limit on a read: its first rows, and it ends on the server.
        let many = "UNWIND range(1, 4000000000) AS limitedread RETURN limitedread";
        let opts = ExecOptions { max_rows: Some(3), ..Default::default() };
        let r = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            s.execute(databrain_connector_core::Statement::internal(many), opts).await.unwrap().collect().await
        })
        .await
        .expect("not read to its end")
        .unwrap();
        assert_eq!(r.num_rows(), 3);
        assert!(gone("limitedread").await, "the limited read still runs on the server");
        // The row limit on a write: it runs to its end.
        s.execute(databrain_connector_core::Statement::internal("MATCH (n:DbStop) DELETE n"), ExecOptions::default()).await.unwrap().collect().await.unwrap();
        let opts = ExecOptions { max_rows: Some(2), ..Default::default() };
        let w = s
            .execute(databrain_connector_core::Statement::internal("UNWIND range(1, 50) AS i CREATE (n:DbStop {i: i}) RETURN n.i AS i"), opts)
            .await
            .unwrap()
            .collect()
            .await
            .unwrap();
        assert_eq!(w.num_rows(), 2);
        assert!(w.notices.iter().any(|n| n.contains("ran to its end")), "{:?}", w.notices);
        let n = s
            .execute(databrain_connector_core::Statement::internal("MATCH (n:DbStop) RETURN count(n) AS c"), ExecOptions::default())
            .await
            .unwrap()
            .collect()
            .await
            .unwrap();
        let c = databrain_connector_core::arrow::util::display::ArrayFormatter::try_new(n.batches[0].column(0).as_ref(), &Default::default())
            .unwrap()
            .value(0)
            .to_string();
        assert_eq!(c, "50", "every node was created");
        s.execute(databrain_connector_core::Statement::internal("MATCH (n:DbStop) DELETE n"), ExecOptions::default()).await.unwrap().collect().await.unwrap();
    }

    /// Live: DATABRAIN_NEO4J_URI=neo4j://localhost:7687 DATABRAIN_NEO4J_PASSWORD=… cargo test -p databrain-connector-neo4j -- --nocapture
    #[ignore = "live: needs DATABRAIN_NEO4J_URI (cargo test -- --ignored)"]
    #[tokio::test]
    async fn live_roundtrip() {
        let Ok(uri) = std::env::var("DATABRAIN_NEO4J_URI") else {
            panic!("live test: DATABRAIN_NEO4J_URI not set (run with --ignored)");
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
            async move { s.execute(databrain_connector_core::Statement::internal(&t), ExecOptions::default()).await.unwrap().collect().await }
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
