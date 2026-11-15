//! Statement splitting and read/write classification for query languages
//! other than SQL ([`QueryLanguage`]). SQL itself is handled in [`crate::sql`]
//! with sqlparser; these are keyword rules, conservative where unsure
//! (`Unknown` is treated like a write: it asks before running).
//!
//! A new language: add its [`QueryLanguage`] variant and a `classify` arm.

use crate::sql::{Classification, StatementKind, StatementSpan, leading_keyword, words};
use crate::traits::QueryLanguage;

/// The whole (trimmed) text as one command (MongoDB JSON).
pub fn split_whole(text: &str) -> Vec<StatementSpan> {
    let start = text.len() - text.trim_start().len();
    let end = text.trim_end().len();
    if start >= end {
        return vec![];
    }
    vec![StatementSpan { start, end, sql: text[start..end].to_string() }]
}

/// Read, write or schema change, by the language's own keywords.
pub fn classify(text: &str, language: QueryLanguage) -> Classification {
    let keyword = leading_keyword(text);
    let kind = match language {
        QueryLanguage::Sql => unreachable!("SQL is classified in sql::classify"),
        QueryLanguage::Cypher => {
            let written = cypher_kind(text);
            // Cypher has `\uXXXX` escapes. Should the server decode them
            // before it reads keywords (as Java character streams may),
            // `CREATE` is CREATE: read both ways, the stricter wins.
            // An escape that can't be decoded: Unknown.
            if text.contains("\\u") || text.contains("\\U") {
                match decode_unicode_escapes(text) {
                    Some(decoded) => written.max(cypher_kind(&decoded)),
                    None => StatementKind::Unknown,
                }
            } else {
                written
            }
        }
        QueryLanguage::MongoJson => mongo_kind(text),
    };
    // `deleteMany({})`, `updateMany({}, …)`: confirmed as a write without
    // WHERE is, named by its method (`DELETEMANY`, not the leading `db`).
    let missing_where = language == QueryLanguage::MongoJson && mongo_info(text).flatten().is_some_and(|i| i.every_document);
    let keyword = if missing_where { mongo_op(&text.to_ascii_lowercase()).map(|o| o.to_ascii_uppercase()).unwrap_or(keyword) } else { keyword };
    Classification { kind, missing_where, keyword }
}

/// A Cypher statement by its keywords (anywhere in the text: a word in a
/// string counts too, the safe side).
fn cypher_kind(text: &str) -> StatementKind {
    let keyword = leading_keyword(text);
    let has = |list: &[&str]| words(text).any(|w| list.iter().any(|k| w.eq_ignore_ascii_case(k)));
    if has(&["CONSTRAINT", "INDEX"]) && has(&["CREATE", "DROP"]) {
        StatementKind::Ddl
    } else if has(&["CREATE", "INSERT", "MERGE", "SET", "DELETE", "DETACH", "REMOVE", "DROP", "LOAD", "FOREACH"]) {
        // INSERT: Neo4j 5's GQL spelling of CREATE.
        StatementKind::Dml
    } else if has(&["TERMINATE"]) {
        // `SHOW TRANSACTIONS YIELD … TERMINATE TRANSACTIONS …` ends others' work.
        StatementKind::Other
    } else if has(&["CALL"]) {
        // Procedures may write.
        StatementKind::Unknown
    } else if matches!(keyword.as_str(), "MATCH" | "OPTIONAL" | "WITH" | "RETURN" | "UNWIND" | "SHOW" | "EXPLAIN" | "PROFILE") {
        StatementKind::Read
    } else {
        StatementKind::Unknown
    }
}

/// `text` with every `\uXXXX` and `\UXXXXXXXX` replaced by its character
/// (a UTF-16 surrogate pair as one); `None` when one is malformed.
fn decode_unicode_escapes(text: &str) -> Option<String> {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find('\\') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        let digits = match after.as_bytes().first() {
            Some(b'u') => 4,
            Some(b'U') => 8,
            // `\\`: a backslash, never the start of an escape (`'C:\\users'`).
            Some(b'\\') => {
                out.push_str("\\\\");
                rest = &after[1..];
                continue;
            }
            _ => {
                out.push('\\');
                rest = after;
                continue;
            }
        };
        let hex = after.get(1..1 + digits).filter(|h| h.bytes().all(|b| b.is_ascii_hexdigit()))?;
        let mut code = u32::from_str_radix(hex, 16).ok()?;
        rest = &after[1 + digits..];
        if (0xD800..0xDC00).contains(&code) {
            // A high surrogate: its low half follows as `\uXXXX`.
            let low =
                rest.strip_prefix("\\u").and_then(|r| r.get(..4)).and_then(|h| u32::from_str_radix(h, 16).ok()).filter(|l| (0xDC00..0xE000).contains(l))?;
            code = 0x10000 + ((code - 0xD800) << 10) + (low - 0xDC00);
            rest = &rest[6..];
        }
        out.push(char::from_u32(code)?);
    }
    out.push_str(rest);
    Some(out)
}

/// What a MongoDB command does, from the shell parser the MongoDB connector
/// runs it with (its database command, pipeline stages included).
#[derive(Debug, Clone, PartialEq)]
pub struct MongoInfo {
    pub kind: StatementKind,
    /// Collections read and written, `[db, coll]` or `[coll]`.
    pub reads: Vec<Vec<String>>,
    pub writes: Vec<Vec<String>>,
    /// An update or delete of many documents with an empty filter: it
    /// changes every document of the collection (SQL's write without WHERE).
    pub every_document: bool,
    /// `$lookup` equalities between the aggregated collection's fields and
    /// another collection's (only where the field still is the collection's:
    /// see the connector's analyzer).
    pub joins: Vec<MongoJoin>,
}

/// `local.local_field = from.foreign_field`, from a `$lookup` stage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MongoJoin {
    /// `[db, coll]` or `[coll]`.
    pub local: Vec<String>,
    pub local_field: String,
    pub from: Vec<String>,
    pub foreign_field: String,
}

/// Parses a command the way the MongoDB connector runs it; `None` when it
/// isn't one the connector accepts.
pub type MongoAnalyzer = fn(&str) -> Option<MongoInfo>;

static MONGO_ANALYZER: std::sync::OnceLock<MongoAnalyzer> = std::sync::OnceLock::new();

/// Set by the app with the MongoDB connector's parser (once per process).
pub fn set_mongo_analyzer(f: MongoAnalyzer) {
    let _ = MONGO_ANALYZER.set(f);
}

/// The connector's view of `text`: `Some(None)` = it doesn't parse (it
/// won't run either); `None` = no analyzer in this process.
fn mongo_info(text: &str) -> Option<Option<MongoInfo>> {
    MONGO_ANALYZER.get().map(|f| f(text))
}

/// `$lookup` joins of a MongoDB command: `None` when no analyzer is set
/// in this process or the command doesn't parse.
pub fn mongo_joins(text: &str) -> Option<Vec<MongoJoin>> {
    mongo_info(text).flatten().map(|i| i.joins)
}

/// `{"find": "orders", …}`, `db.orders.find({…})` or `db.orders.insertOne(…)`.
fn mongo_kind(text: &str) -> StatementKind {
    match mongo_info(text) {
        Some(Some(info)) => return info.kind,
        Some(None) => return StatementKind::Unknown,
        None => {}
    }
    // No parser here (a crate without the MongoDB connector): by the text,
    // and any `$out` / `$merge` token makes an aggregate a write, however it
    // is quoted or spaced.
    let t = text.to_ascii_lowercase();
    let op = mongo_op(&t);
    let writes_out = t.contains("$out") || t.contains("$merge");
    match op.as_deref() {
        Some("find" | "findone" | "count" | "countdocuments" | "estimateddocumentcount" | "distinct" | "listcollections" | "listindexes" | "explain") => {
            StatementKind::Read
        }
        Some("aggregate") if !writes_out => StatementKind::Read,
        Some("aggregate") => StatementKind::Dml,
        Some(o)
            if o.starts_with("insert")
                || o.starts_with("update")
                || o.starts_with("delete")
                || o.starts_with("replace")
                || o.starts_with("findoneand")
                || o == "bulkwrite" =>
        {
            StatementKind::Dml
        }
        Some(o) if o.starts_with("create") || o.starts_with("drop") || o == "renamecollection" || o == "collmod" => StatementKind::Ddl,
        _ => StatementKind::Unknown,
    }
}

/// The command name: the first key of a JSON command, or the method of a
/// `db…collection.method(` call (after `use x` lines; `getSiblingDB` and
/// `getCollection` are accessors, not the method).
fn mongo_op(t: &str) -> Option<String> {
    let t = t.trim_start();
    let t = t.lines().skip_while(|l| l.trim().is_empty() || l.trim_start().starts_with("use ")).collect::<Vec<_>>().join("\n");
    let t = t.trim();
    if let Some(rest) = t.strip_prefix('{') {
        let rest = rest.trim_start().strip_prefix('"')?;
        return Some(rest[..rest.find('"')?].to_string());
    }
    let mut rest = t.strip_prefix("db")?;
    loop {
        let r = rest.trim_start().strip_prefix('.')?;
        let end = r.find(|c: char| !(c.is_alphanumeric() || c == '_' || c == '$'))?;
        let (name, after) = (&r[..end], r[end..].trim_start());
        if !after.starts_with('(') {
            // A collection name.
            rest = after;
            continue;
        }
        if name == "getsiblingdb" || name == "getcollection" {
            rest = &after[after.find(')')? + 1..];
            continue;
        }
        return Some(name.to_string());
    }
}

/// Collections a MongoDB command reads and writes (`db.coll` when `use db` or
/// `getSiblingDB("db")` names the database, else `coll`). `$lookup`,
/// `$graphLookup` and `$unionWith` read their collections; `$out` and
/// `$merge` write theirs. A command whose collection can't be found is opaque.
pub fn mongo_table_refs(text: &str) -> crate::tables::TableRefs {
    use crate::tables::TableRefs;
    let mut out = TableRefs::default();
    match mongo_info(text) {
        Some(Some(info)) => {
            out.reads = info.reads;
            out.writes = info.writes;
            out.changes_default = false;
            if out.reads.is_empty() && out.writes.is_empty() {
                out.opaque.push(leading_keyword(text));
            }
            return out;
        }
        Some(None) => {
            out.opaque.push(leading_keyword(text));
            return out;
        }
        None => {}
    }
    let mut db: Option<String> = None;
    let mut body = Vec::new();
    for line in text.lines() {
        let l = line.trim();
        if let Some(name) = l.strip_prefix("use ") {
            db = Some(name.trim().trim_end_matches(';').trim_matches(|c| c == '"' || c == '\'').to_string());
            out.changes_default = false;
        } else {
            body.push(line);
        }
    }
    let body = body.join("\n");
    let t = body.trim();
    let lower = t.to_ascii_lowercase();
    let quoted = |s: &str| -> Option<String> {
        let s = s.trim_start();
        let q = s.chars().next().filter(|c| *c == '"' || *c == '\'')?;
        let rest = &s[1..];
        Some(rest[..rest.find(q)?].to_string())
    };
    // `db.getSiblingDB("x")` picks the database.
    if let Some(i) = lower.find("getsiblingdb(")
        && let Some(n) = quoted(&t[i + "getsiblingdb(".len()..])
    {
        db = Some(n);
    }
    let name = |coll: String| match &db {
        Some(d) => vec![d.clone(), coll],
        None => vec![coll],
    };
    // The main collection.
    let main = if t.starts_with('{') {
        // `{"find": "orders", …}`: the first key's string value.
        t.find(':').and_then(|i| quoted(&t[i + 1..]))
    } else if let Some(i) = lower.find("getcollection(") {
        quoted(&t[i + "getcollection(".len()..])
    } else {
        // db[.getSiblingDB(..)].coll.method(
        let mut rest = lower.strip_prefix("db").map(|_| &t[2..]);
        let mut found = None;
        while let Some(r) = rest {
            let Some(r) = r.trim_start().strip_prefix('.') else { break };
            let end = r.find(|c: char| !(c.is_alphanumeric() || c == '_' || c == '$')).unwrap_or(r.len());
            let (n, after) = (&r[..end], r[end..].trim_start());
            if after.starts_with('(') {
                if n.eq_ignore_ascii_case("getSiblingDB") {
                    rest = after.find(')').map(|j| &after[j + 1..]);
                    continue;
                }
                break;
            }
            found = Some(n.to_string());
            rest = Some(after);
        }
        found
    };
    let Some(main) = main.filter(|m| !m.is_empty()) else {
        out.opaque.push(leading_keyword(text));
        return out;
    };
    let writes = matches!(mongo_kind(text), StatementKind::Dml | StatementKind::Ddl | StatementKind::Unknown);
    let pipeline_writes = lower.contains("$out") || lower.contains("$merge");
    if writes && !(pipeline_writes && lower.contains("aggregate")) {
        out.writes.push(name(main.clone()));
    } else {
        out.reads.push(name(main.clone()));
    }
    // Other collections named in the pipeline: after `from:`, `coll:`,
    // `$unionWith:` (reads) and `$out:` / `into:` (writes).
    let mut scan = |keys: &[&str], write: bool| {
        for key in keys {
            let mut from = 0;
            while let Some(i) = lower[from..].find(key) {
                let at = from + i + key.len();
                from = at;
                let after = t[at..].trim_start().trim_start_matches(['"', '\'']).trim_start();
                let Some(after) = after.strip_prefix(':') else { continue };
                if let Some(c) = quoted(after) {
                    let n = name(c);
                    let list = if write { &mut out.writes } else { &mut out.reads };
                    if !list.contains(&n) {
                        list.push(n);
                    }
                }
            }
        }
    };
    scan(&["from", "coll", "$unionwith"], false);
    scan(&["$out", "$merge", "into"], true);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::split_with;
    use crate::traits::{STANDARD, SourceTraits};

    const MONGO: SourceTraits = SourceTraits { language: QueryLanguage::MongoJson, ..STANDARD };
    const CYPHER: SourceTraits = SourceTraits { language: QueryLanguage::Cypher, ..STANDARD };

    #[test]
    fn splits_per_language() {
        let m = split_with("\n  db.orders.find({\"a\": \"x;y\"})\n\n", &MONGO);
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].sql, "db.orders.find({\"a\": \"x;y\"})");
        assert!(split_with("   ", &MONGO).is_empty());
        assert_eq!(split_with("MATCH (n) RETURN n; MATCH (m) RETURN m", &CYPHER).len(), 2);
    }

    #[test]
    fn classifies_per_language() {
        let k = |t: &str, l| classify(t, l).kind;
        use QueryLanguage as L;
        use StatementKind as S;
        assert_eq!(k("db.orders.find({status: 'A'})", L::MongoJson), S::Read);
        assert_eq!(k(r#"{"aggregate": "orders", "pipeline": [{"$match": {}}]}"#, L::MongoJson), S::Read);
        assert_eq!(k(r#"{"aggregate": "orders", "pipeline": [{"$out": "copy"}]}"#, L::MongoJson), S::Dml);
        // Without the connector's parser: any $out / $merge token, however spaced or quoted.
        assert_eq!(k("db.orders.aggregate([{ $out : 'copy' }])", L::MongoJson), S::Dml);
        assert_eq!(k("db.orders.aggregate([{ '$merge' : 'copy' }])", L::MongoJson), S::Dml);
        assert_eq!(k("db.orders.deleteMany({})", L::MongoJson), S::Dml);
        assert_eq!(k("db.orders.drop()", L::MongoJson), S::Ddl);
        assert_eq!(k("what is this", L::MongoJson), S::Unknown);
        assert_eq!(k("use shop\ndb.getSiblingDB(\"x\").getCollection(\"o\").find({}).limit(5)", L::MongoJson), S::Read);
        assert_eq!(k("db.getCollection('o').insertOne({})", L::MongoJson), S::Dml);
        assert_eq!(k("db.runCommand({ping: 1})", L::MongoJson), S::Unknown);
        assert_eq!(k("MATCH (p:Person) RETURN p.name LIMIT 5", L::Cypher), S::Read);
        assert_eq!(k("MATCH (p:Person {name: 'A'}) SET p.age = 3", L::Cypher), S::Dml);
        assert_eq!(k("CREATE INDEX FOR (p:Person) ON (p.name)", L::Cypher), S::Ddl);
        assert_eq!(k("CALL db.labels()", L::Cypher), S::Unknown);
        assert_eq!(k("MATCH (a:P {id: 1}) INSERT (a)-[:R]->(:X {n: 2})", L::Cypher), S::Dml);
        assert_eq!(k("INSERT (:P {name: 'A'})", L::Cypher), S::Dml);
        assert_eq!(k("SHOW TRANSACTIONS YIELD transactionId AS id WHERE id <> 'x' TERMINATE TRANSACTIONS id", L::Cypher), S::Other);
        assert_eq!(k("TERMINATE TRANSACTIONS 'neo4j-transaction-1'", L::Cypher), S::Other);
        // Still reads: the words in a string or a property name aren't statements.
        assert_eq!(k("SHOW TRANSACTIONS", L::Cypher), S::Read);
        assert_eq!(k("MATCH (n) RETURN n.inserted_at", L::Cypher), S::Read);
    }

    /// A `\uXXXX` escape can't hide a write keyword: the text is read as
    /// written and decoded, and the stricter reading wins.
    #[test]
    fn cypher_unicode_escapes_dont_hide_writes() {
        let k = |t: &str| classify(t, QueryLanguage::Cypher).kind;
        use StatementKind as S;
        // Refused as a read.
        assert_eq!(k(r"MATCH (n) \u0043REATE (m:X)"), S::Dml);
        assert_eq!(k(r"MATCH (n) \u0064\u0065\u006c\u0065\u0074\u0065 n"), S::Dml);
        assert_eq!(k(r"MATCH (n) \U00000044ELETE n"), S::Dml);
        assert_eq!(k(r"MATCH (n) RETURN '\u0027 \u0043REATE (x) //'"), S::Dml, "an escaped quote that may end the string");
        assert_eq!(k(r"\u0043ALL db.x()"), S::Unknown);
        assert_eq!(k(r"MATCH (n) RETURN '\u00ZZ'"), S::Unknown, "malformed: can't tell");
        assert_eq!(k(r"RETURN '\uD83D'"), S::Unknown, "a lone surrogate");
        // Still reads.
        assert_eq!(k(r"MATCH (n) WHERE n.name = 'caf\u00e9' RETURN n"), S::Read);
        assert_eq!(k(r"RETURN '\uD83D\uDE00' AS smile"), S::Read);
        assert_eq!(k(r"MATCH (n) WHERE n.path = 'C:\\users\\an' RETURN n"), S::Read);
        assert_eq!(decode_unicode_escapes(r"a\u0043\\u0043\U0001F600").as_deref(), Some(r"aC\\u0043😀"));
    }

    #[test]
    fn mongo_collections() {
        let n = |v: &[Vec<String>]| v.iter().map(|t| t.join(".")).collect::<Vec<_>>();
        let r = mongo_table_refs("db.orders.find({status: 'A'})");
        assert_eq!((n(&r.reads), n(&r.writes)), (vec!["orders".to_string()], vec![]));
        let r =
            mongo_table_refs("use shop\ndb.orders.aggregate([{$lookup: {from: 'customers', localField: 'c', foreignField: '_id', as: 'c'}}, {$out: 'copy'}])");
        assert_eq!(n(&r.reads), ["shop.orders", "shop.customers"]);
        assert_eq!(n(&r.writes), ["shop.copy"]);
        let r = mongo_table_refs(r#"{"insert": "logs", "documents": [{}]}"#);
        assert_eq!(n(&r.writes), ["logs"]);
        let r = mongo_table_refs("db.getSiblingDB(\"hr\").getCollection(\"pay\").deleteMany({})");
        assert_eq!(n(&r.writes), ["hr.pay"]);
        assert!(!mongo_table_refs("db.runCommand({ping: 1})").opaque.is_empty() || !mongo_table_refs("db.runCommand({ping: 1})").reads.is_empty());
        assert!(!mongo_table_refs("what").opaque.is_empty());
    }
}
