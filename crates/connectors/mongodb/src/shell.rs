//! mongosh-style commands → MongoDB database commands.
//!
//! Accepted (one command per run, optional `use <db>` lines before it):
//! - `db.orders.find({status: "A"}, {name: 1}).sort({at: -1}).limit(10)`
//! - `db.getCollection("my orders").aggregate([...])`, `db.getSiblingDB("x").orders.find()`
//! - `findOne`, `countDocuments`, `estimatedDocumentCount`, `distinct`, `insertOne`,
//!   `insertMany`, `updateOne`, `updateMany`, `replaceOne`, `deleteOne`, `deleteMany`,
//!   `createIndex`, `dropIndex`, `drop`, `getIndexes`
//! - `db.runCommand({...})`, `db.adminCommand({...})`, `db.getCollectionNames()`,
//!   `db.createCollection("x")`, `db.stats()`
//! - a JSON database command: `{"find": "orders", "filter": {...}}`
//!
//! Arguments are relaxed JSON (JSON5: unquoted keys, single quotes, trailing
//! commas) with `ObjectId("…")`, `ISODate("…")`, `new Date("…")`,
//! `NumberLong(…)`, `NumberInt(…)`, `NumberDecimal("…")` and `UUID("…")`.

use mongodb::bson::{Bson, Document, doc};

/// What running the command produces.
#[derive(Debug, Clone, PartialEq)]
pub enum Output {
    /// A cursor command (`find`, `aggregate`, `listCollections`…): one row per document.
    Cursor,
    /// One document shown as one row (`runCommand`, `stats`…).
    Document,
    /// A write: rows affected from `n` / `nModified`.
    Write,
    /// `count` → one row `{count: n}`.
    Count,
    /// `distinct` → one row per value, in a column named after the field.
    Distinct(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    /// Database to run on (`None` = the connection's).
    pub db: Option<String>,
    pub command: Document,
    pub output: Output,
}

pub fn parse(text: &str, max_rows: Option<usize>) -> Result<Plan, String> {
    let mut db: Option<String> = None;
    let mut rest = text.trim();
    // `use shop` lines.
    loop {
        let line_end = rest.find('\n').unwrap_or(rest.len());
        let line = rest[..line_end].trim().trim_end_matches(';').trim();
        if let Some(name) = line.strip_prefix("use ").map(str::trim).filter(|n| !n.is_empty() && !n.contains(char::is_whitespace)) {
            db = Some(name.trim_matches(|c| c == '"' || c == '\'').to_string());
            rest = rest[line_end..].trim_start();
            continue;
        }
        break;
    }
    let rest = rest.trim().trim_end_matches(';').trim();
    if rest.is_empty() {
        return Err("Write a command, e.g. db.orders.find({}) or {\"find\": \"orders\"}".into());
    }
    if rest.starts_with('{') {
        let command = to_document(&value(rest)?)?;
        let name = command.keys().next().cloned().unwrap_or_default();
        let output = match name.as_str() {
            "find" | "aggregate" | "listCollections" | "listIndexes" => Output::Cursor,
            "insert" | "update" | "delete" | "findAndModify" => Output::Write,
            _ => Output::Document,
        };
        return Ok(Plan { db, command, output });
    }
    if !(rest.starts_with("db.") || rest.starts_with("db[")) {
        return Err("Start with db. (db.orders.find({})) or write a JSON command ({\"find\": \"orders\"})".into());
    }
    let calls = chain(rest)?;
    let mut it = calls.into_iter();
    match it.next() {
        Some(Seg::Name(n)) if n == "db" => {}
        _ => return Err("Start with db. (db.orders.find({})) or write a JSON command ({\"find\": \"orders\"})".into()),
    }
    let mut coll: Option<String> = None;
    let mut call: Option<(String, Vec<serde_json::Value>)> = None;
    for seg in it.by_ref() {
        match seg {
            Seg::Call(m, args) if coll.is_none() && m == "getSiblingDB" => db = Some(str_arg(&args, 0, &m)?),
            Seg::Call(m, args) if coll.is_none() && m == "getCollection" => coll = Some(str_arg(&args, 0, &m)?),
            Seg::Name(n) if coll.is_none() => coll = Some(n),
            Seg::Call(m, args) => {
                call = Some((m, args));
                break;
            }
            Seg::Name(n) => return Err(format!("unexpected .{n}")),
        }
    }
    let (method, args) = call.ok_or("Call a method, e.g. .find({})")?;
    let modifiers: Vec<(String, Vec<serde_json::Value>)> = it
        .map(|s| match s {
            Seg::Call(m, a) => Ok((m, a)),
            Seg::Name(n) => Err(format!("unexpected .{n}")),
        })
        .collect::<Result<_, _>>()?;
    match coll {
        None => db_method(db, &method, &args),
        Some(c) => coll_method(db, &c, &method, &args, &modifiers, max_rows),
    }
}

fn db_method(db: Option<String>, method: &str, args: &[serde_json::Value]) -> Result<Plan, String> {
    let d = |command: Document, output| Ok(Plan { db: db.clone(), command, output });
    match method {
        "runCommand" => {
            let command = doc_arg(args, 0, method)?;
            let output = match command.keys().next().map(String::as_str) {
                Some("find" | "aggregate" | "listCollections" | "listIndexes") => Output::Cursor,
                _ => Output::Document,
            };
            d(command, output)
        }
        "adminCommand" => Ok(Plan { db: Some("admin".into()), command: doc_arg(args, 0, method)?, output: Output::Document }),
        "getCollectionNames" | "getCollectionInfos" => d(doc! {"listCollections": 1, "nameOnly": method == "getCollectionNames"}, Output::Cursor),
        "createCollection" => {
            let mut c = doc! {"create": str_arg(args, 0, method)?};
            if args.len() > 1 {
                c.extend(doc_arg(args, 1, method)?);
            }
            d(c, Output::Document)
        }
        "stats" => d(doc! {"dbStats": 1}, Output::Document),
        "dropDatabase" => d(doc! {"dropDatabase": 1}, Output::Document),
        m => Err(format!("db.{m}() is not supported here; use db.runCommand({{…}})")),
    }
}

fn coll_method(
    db: Option<String>,
    coll: &str,
    method: &str,
    args: &[serde_json::Value],
    modifiers: &[(String, Vec<serde_json::Value>)],
    max_rows: Option<usize>,
) -> Result<Plan, String> {
    let opt_doc = |i: usize| -> Result<Option<Document>, String> { if args.len() > i { Ok(Some(doc_arg(args, i, method)?)) } else { Ok(None) } };
    let plan = |command: Document, output| Ok(Plan { db: db.clone(), command, output });
    match method {
        "find" | "findOne" => {
            let mut c = doc! {"find": coll, "filter": opt_doc(0)?.unwrap_or_default()};
            if let Some(p) = opt_doc(1)? {
                c.insert("projection", p);
            }
            for (m, a) in modifiers {
                match m.as_str() {
                    "sort" => c.insert("sort", doc_arg(a, 0, m)?),
                    "projection" | "project" => c.insert("projection", doc_arg(a, 0, m)?),
                    "limit" => c.insert("limit", int_arg(a, 0, m)?),
                    "skip" => c.insert("skip", int_arg(a, 0, m)?),
                    "hint" => c.insert("hint", a.first().map(json_to_bson).transpose()?.unwrap_or(Bson::Null)),
                    "maxTimeMS" => c.insert("maxTimeMS", int_arg(a, 0, m)?),
                    "batchSize" => c.insert("batchSize", int_arg(a, 0, m)?),
                    "toArray" | "pretty" => None,
                    "count" => return plan(doc! {"count": coll, "query": c.get_document("filter").cloned().unwrap_or_default()}, Output::Count),
                    _ => return Err(format!(".{m}() is not supported after find")),
                };
            }
            if method == "findOne" {
                c.insert("limit", 1);
                c.insert("singleBatch", true);
            } else if let Some(n) = max_rows {
                // The engine keeps at most `max_rows`: don't ask for more.
                let n = n as i64;
                let lim = c.get("limit").and_then(Bson::as_i64).filter(|l| *l > 0);
                if lim.is_none_or(|l| l > n) {
                    c.insert("limit", n);
                }
            }
            plan(c, Output::Cursor)
        }
        "aggregate" => {
            let pipeline = args.first().ok_or("aggregate needs a pipeline: aggregate([{$match: {}}])")?;
            let stages = match json_to_bson(pipeline)? {
                Bson::Array(a) => a,
                Bson::Document(d) => vec![Bson::Document(d)],
                _ => return Err("aggregate needs a pipeline array".into()),
            };
            let mut c = doc! {"aggregate": coll, "pipeline": stages, "cursor": {}};
            if let Some(o) = opt_doc(1)? {
                c.extend(o);
            }
            plan(c, Output::Cursor)
        }
        "countDocuments" | "count" => plan(doc! {"count": coll, "query": opt_doc(0)?.unwrap_or_default()}, Output::Count),
        "estimatedDocumentCount" => plan(doc! {"count": coll}, Output::Count),
        "distinct" => {
            let key = str_arg(args, 0, method)?;
            plan(doc! {"distinct": coll, "key": key.clone(), "query": opt_doc(1)?.unwrap_or_default()}, Output::Distinct(key))
        }
        "insertOne" => plan(doc! {"insert": coll, "documents": [doc_arg(args, 0, method)?]}, Output::Write),
        "insertMany" => {
            let docs = match args.first().map(json_to_bson).transpose()? {
                Some(Bson::Array(a)) => a,
                _ => return Err("insertMany needs an array of documents".into()),
            };
            plan(doc! {"insert": coll, "documents": docs}, Output::Write)
        }
        "updateOne" | "updateMany" | "replaceOne" => {
            let q = doc_arg(args, 0, method)?;
            let u = args.get(1).ok_or(format!("{method} needs a filter and an update"))?;
            let u = json_to_bson(u)?;
            if !matches!(u, Bson::Document(_) | Bson::Array(_)) {
                return Err(format!("{method}: the update must be a document or a pipeline"));
            }
            let mut one = doc! {"q": q, "u": u, "multi": method == "updateMany"};
            if let Some(o) = opt_doc(2)? {
                if let Some(up) = o.get("upsert") {
                    one.insert("upsert", up.clone());
                }
            }
            plan(doc! {"update": coll, "updates": [one]}, Output::Write)
        }
        "deleteOne" | "deleteMany" => {
            let q = opt_doc(0)?.unwrap_or_default();
            plan(doc! {"delete": coll, "deletes": [{"q": q, "limit": if method == "deleteOne" { 1 } else { 0 }}]}, Output::Write)
        }
        "createIndex" => {
            let key = doc_arg(args, 0, method)?;
            let mut index = doc! {"key": key.clone(), "name": index_name(&key)};
            if let Some(o) = opt_doc(1)? {
                index.extend(o);
            }
            plan(doc! {"createIndexes": coll, "indexes": [index]}, Output::Document)
        }
        "dropIndex" => {
            let index = args.first().map(json_to_bson).transpose()?.ok_or("dropIndex needs a name or key")?;
            plan(doc! {"dropIndexes": coll, "index": index}, Output::Document)
        }
        "getIndexes" => plan(doc! {"listIndexes": coll}, Output::Cursor),
        "drop" => plan(doc! {"drop": coll}, Output::Document),
        "stats" => plan(doc! {"collStats": coll}, Output::Document),
        m => Err(format!(".{m}() is not supported; use find, aggregate, countDocuments, distinct, insert/update/delete…, or db.runCommand({{…}})")),
    }
}

/// `{a: 1, b: -1}` → `a_1_b_-1` (MongoDB's default index name).
fn index_name(key: &Document) -> String {
    key.iter()
        .map(|(k, v)| format!("{k}_{}", match v {
            Bson::Int32(i) => i.to_string(),
            Bson::Int64(i) => i.to_string(),
            Bson::Double(f) => f.to_string(),
            Bson::String(s) => s.clone(),
            _ => "1".into(),
        }))
        .collect::<Vec<_>>()
        .join("_")
}

#[derive(Debug)]
enum Seg {
    Name(String),
    Call(String, Vec<serde_json::Value>),
}

/// `db.a.b(x).c(y)` → segments. `db["my coll"]` is a name too.
fn chain(text: &str) -> Result<Vec<Seg>, String> {
    let b = text.as_bytes();
    let mut i = 0;
    let mut out = Vec::new();
    let ident = |i: &mut usize| {
        let s = *i;
        while *i < b.len() && (b[*i].is_ascii_alphanumeric() || b[*i] == b'_' || b[*i] == b'$' || b[*i] == b'-') {
            *i += 1;
        }
        text[s..*i].to_string()
    };
    let skip_ws = |i: &mut usize| {
        while *i < b.len() && b[*i].is_ascii_whitespace() {
            *i += 1;
        }
    };
    loop {
        skip_ws(&mut i);
        if i >= b.len() {
            break;
        }
        if !out.is_empty() {
            match b[i] {
                b'.' => i += 1,
                b'[' => {
                    let end = matching(b, i).ok_or("unclosed [")?;
                    let name: String = json5::from_str(&text[i + 1..end]).map_err(|e| format!("bad collection name: {e}"))?;
                    out.push(Seg::Name(name));
                    i = end + 1;
                    continue;
                }
                c => return Err(format!("unexpected `{}` at {}", c as char, i + 1)),
            }
            skip_ws(&mut i);
        }
        let name = ident(&mut i);
        if name.is_empty() {
            return Err(format!("expected a name at {}", i + 1));
        }
        skip_ws(&mut i);
        if i < b.len() && b[i] == b'(' {
            let end = matching(b, i).ok_or_else(|| format!("unclosed ( after {name}"))?;
            out.push(Seg::Call(name, args(&text[i + 1..end])?));
            i = end + 1;
        } else {
            out.push(Seg::Name(name));
        }
    }
    Ok(out)
}

/// Index of the bracket closing the one at `open`, skipping strings.
fn matching(b: &[u8], open: usize) -> Option<usize> {
    let mut depth = 0i32;
    let mut i = open;
    while i < b.len() {
        match b[i] {
            q @ (b'"' | b'\'' | b'`') => {
                i += 1;
                while i < b.len() && b[i] != q {
                    if b[i] == b'\\' {
                        i += 1;
                    }
                    i += 1;
                }
            }
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

fn args(text: &str) -> Result<Vec<serde_json::Value>, String> {
    if text.trim().is_empty() {
        return Ok(vec![]);
    }
    match value(&format!("[{text}]"))? {
        serde_json::Value::Array(a) => Ok(a),
        _ => unreachable!(),
    }
}

/// Relaxed JSON with shell helpers → JSON (extended JSON for helpers).
fn value(text: &str) -> Result<serde_json::Value, String> {
    let v: serde_json::Value = json5::from_str(&helpers(text)).map_err(|e| format!("Could not read the arguments: {}", e.to_string().lines().next().unwrap_or_default()))?;
    Ok(integers(v))
}

/// JSON5 reads every number as a float: whole numbers written without a
/// point become integers again (`limit(5)`, `{age: 30}`, `{at: -1}`).
fn integers(v: serde_json::Value) -> serde_json::Value {
    use serde_json::Value as J;
    match v {
        J::Number(n) => match n.as_f64() {
            Some(f) if n.as_i64().is_none() && f.fract() == 0.0 && f.abs() < 9.0e15 => J::from(f as i64),
            _ => J::Number(n),
        },
        J::Array(a) => J::Array(a.into_iter().map(integers).collect()),
        J::Object(o) => J::Object(o.into_iter().map(|(k, v)| (k, integers(v))).collect()),
        v => v,
    }
}

/// Replace `ObjectId("x")` etc. (outside strings) by extended JSON.
fn helpers(text: &str) -> String {
    const H: &[&str] = &["new Date", "ObjectId", "ISODate", "NumberLong", "NumberInt", "NumberDecimal", "UUID", "Date"];
    let b = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c == b'"' || c == b'\'' {
            let start = i;
            i += 1;
            while i < b.len() && b[i] != c {
                if b[i] == b'\\' {
                    i += 1;
                }
                i += 1;
            }
            i = (i + 1).min(b.len());
            out.push_str(&text[start..i]);
            continue;
        }
        let prev_word = i > 0 && (b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'_' || b[i - 1] == b'$');
        if !prev_word {
            if let Some(h) = H.iter().find(|h| text[i..].starts_with(*h) && text[i + h.len()..].trim_start().starts_with('(')) {
                let open = i + h.len() + (text[i + h.len()..].len() - text[i + h.len()..].trim_start().len());
                if let Some(close) = matching(b, open) {
                    let inner = text[open + 1..close].trim();
                    let unq = inner.trim_matches(|c| c == '"' || c == '\'');
                    let rep = match *h {
                        "ObjectId" => format!("{{\"$oid\": \"{unq}\"}}"),
                        "ISODate" | "new Date" | "Date" if inner.is_empty() => format!("{{\"$date\": {{\"$numberLong\": \"{}\"}}}}", now_ms()),
                        "ISODate" | "new Date" | "Date" => format!("{{\"$date\": \"{}\"}}", iso(unq)),
                        "NumberLong" => format!("{{\"$numberLong\": \"{unq}\"}}"),
                        "NumberInt" => unq.to_string(),
                        "NumberDecimal" => format!("{{\"$numberDecimal\": \"{unq}\"}}"),
                        _ => format!("{{\"$uuid\": \"{unq}\"}}"),
                    };
                    out.push_str(&rep);
                    i = close + 1;
                    continue;
                }
            }
        }
        out.push(c as char);
        i += 1;
    }
    out
}

fn now_ms() -> String {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis().to_string()).unwrap_or_default()
}

/// `2024-01-02` → `2024-01-02T00:00:00Z`; times without a zone are UTC.
fn iso(s: &str) -> String {
    let s = s.trim();
    if !s.contains('T') && !s.contains(' ') {
        return format!("{s}T00:00:00Z");
    }
    let s = s.replacen(' ', "T", 1);
    let has_zone = s.ends_with('Z') || s[s.find('T').unwrap_or(0)..].contains(['+', '-']);
    if has_zone { s } else { format!("{s}Z") }
}

fn json_to_bson(v: &serde_json::Value) -> Result<Bson, String> {
    Bson::try_from(v.clone()).map_err(|e| format!("invalid value: {e}"))
}

fn to_document(v: &serde_json::Value) -> Result<Document, String> {
    match json_to_bson(v)? {
        Bson::Document(d) => Ok(d),
        _ => Err("expected a document {…}".into()),
    }
}

fn doc_arg(args: &[serde_json::Value], i: usize, m: &str) -> Result<Document, String> {
    to_document(args.get(i).ok_or_else(|| format!("{m}() needs a document argument"))?)
}

fn str_arg(args: &[serde_json::Value], i: usize, m: &str) -> Result<String, String> {
    args.get(i).and_then(|v| v.as_str()).map(str::to_string).ok_or_else(|| format!("{m}() needs a name in quotes"))
}

fn int_arg(args: &[serde_json::Value], i: usize, m: &str) -> Result<i64, String> {
    args.get(i).and_then(|v| v.as_i64().or_else(|| v.as_f64().map(|f| f as i64))).ok_or_else(|| format!("{m}() needs a number"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_calls_become_commands() {
        let p = parse("db.orders.find({status: 'A', _id: ObjectId('65a1b2c3d4e5f60718293a4b')}, {name: 1}).sort({at: -1}).limit(5)", Some(100)).unwrap();
        assert_eq!(p.output, Output::Cursor);
        assert_eq!(p.command.get_str("find").unwrap(), "orders");
        let f = p.command.get_document("filter").unwrap();
        assert_eq!(f.get_str("status").unwrap(), "A");
        assert!(matches!(f.get("_id"), Some(Bson::ObjectId(_))));
        assert_eq!(p.command.get_document("sort").unwrap().get_i32("at").unwrap(), -1);
        assert_eq!(p.command.get_i64("limit").unwrap(), 5);
        // The row cap limits an unbounded find.
        let p = parse("db.orders.find()", Some(1001)).unwrap();
        assert_eq!(p.command.get_i64("limit").unwrap(), 1001);
        // getSiblingDB, getCollection, use, ISODate.
        let p = parse("use shop\ndb.getCollection(\"my orders\").countDocuments({at: {$gte: ISODate('2024-01-02')}})", None).unwrap();
        assert_eq!(p.db.as_deref(), Some("shop"));
        assert_eq!(p.output, Output::Count);
        assert_eq!(p.command.get_str("count").unwrap(), "my orders");
        assert!(matches!(p.command.get_document("query").unwrap().get_document("at").unwrap().get("$gte"), Some(Bson::DateTime(_))));
        let p = parse("db.getSiblingDB('crm').customers.aggregate([{$match: {}}, {$count: 'n'},])", None).unwrap();
        assert_eq!((p.db.as_deref(), p.command.get_str("aggregate").unwrap()), (Some("crm"), "customers"));
        assert_eq!(p.command.get_array("pipeline").unwrap().len(), 2);
        assert_eq!(parse("db[\"a.b\"].distinct('k')", None).unwrap().output, Output::Distinct("k".into()));
    }

    #[test]
    fn writes_and_json_commands() {
        let p = parse("db.orders.updateMany({s: 'A'}, {$set: {s: 'B'}}, {upsert: true});", None).unwrap();
        assert_eq!(p.output, Output::Write);
        let u = p.command.get_array("updates").unwrap()[0].as_document().unwrap().clone();
        assert!(u.get_bool("multi").unwrap() && u.get_bool("upsert").unwrap());
        let p = parse("db.orders.deleteOne({_id: 1})", None).unwrap();
        assert_eq!(p.command.get_array("deletes").unwrap()[0].as_document().unwrap().get_i32("limit").unwrap(), 1);
        assert_eq!(parse("db.t.createIndex({a: 1, b: -1})", None).unwrap().command.get_array("indexes").unwrap()[0].as_document().unwrap().get_str("name").unwrap(), "a_1_b_-1");
        let p = parse(r#"{"find": "orders", "filter": {"a": {"$gt": 1}}}"#, None).unwrap();
        assert_eq!(p.output, Output::Cursor);
        assert_eq!(parse("db.runCommand({ping: 1})", None).unwrap().output, Output::Document);
        assert_eq!(parse("db.adminCommand({listDatabases: 1})", None).unwrap().db.as_deref(), Some("admin"));
    }

    #[test]
    fn helpful_errors() {
        assert!(parse("", None).unwrap_err().contains("Write a command"));
        assert!(parse("select * from t", None).unwrap_err().contains("db."));
        assert!(parse("db.t.find({a: })", None).unwrap_err().contains("arguments"));
        assert!(parse("db.t.mapReduce()", None).unwrap_err().contains("not supported"));
        assert!(parse("db.t.find(", None).unwrap_err().contains("unclosed"));
        // Helpers inside strings are kept as text.
        let p = parse("db.t.find({note: 'ObjectId(\"x\")'})", None).unwrap();
        assert_eq!(p.command.get_document("filter").unwrap().get_str("note").unwrap(), "ObjectId(\"x\")");
    }
}
