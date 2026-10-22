//! Statement splitting and read/write classification for query languages
//! other than SQL ([`QueryLanguage`]). SQL itself is handled in [`crate::sql`]
//! with sqlparser; these are keyword rules, conservative where unsure
//! (`Unknown` is treated like a write: it asks before running).
//!
//! A new language: add its [`QueryLanguage`] variant and a `classify` arm.

use crate::sql::{Classification, StatementKind, StatementSpan, keyword_kind, leading_keyword, words};
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
    let has = |list: &[&str]| words(text).any(|w| list.iter().any(|k| w.eq_ignore_ascii_case(k)));
    let kind = match language {
        QueryLanguage::Sql => unreachable!("SQL is classified in sql::classify"),
        // CQL keywords mean what they mean in SQL; BATCH groups writes.
        QueryLanguage::Cql => match keyword.as_str() {
            // `BEGIN [UNLOGGED|COUNTER] BATCH … APPLY BATCH` writes.
            "BATCH" | "APPLY" => StatementKind::Dml,
            "BEGIN" if has(&["BATCH"]) => StatementKind::Dml,
            "SELECT" if has(&["INSERT", "UPDATE", "DELETE"]) => StatementKind::Unknown,
            k => keyword_kind(k),
        },
        QueryLanguage::Cypher => {
            if has(&["CONSTRAINT", "INDEX"]) && has(&["CREATE", "DROP"]) {
                StatementKind::Ddl
            } else if has(&["CREATE", "MERGE", "SET", "DELETE", "DETACH", "REMOVE", "DROP", "LOAD", "FOREACH"]) {
                StatementKind::Dml
            } else if has(&["CALL"]) {
                // Procedures may write.
                StatementKind::Unknown
            } else if matches!(keyword.as_str(), "MATCH" | "OPTIONAL" | "WITH" | "RETURN" | "UNWIND" | "SHOW" | "EXPLAIN" | "PROFILE") {
                StatementKind::Read
            } else {
                StatementKind::Unknown
            }
        }
        QueryLanguage::MongoJson => mongo_kind(text),
    };
    Classification { kind, missing_where: false, keyword }
}

/// `{"find": "orders", …}`, `db.orders.find({…})` or `db.orders.insertOne(…)`.
fn mongo_kind(text: &str) -> StatementKind {
    let t = text.to_ascii_lowercase();
    let op = mongo_op(&t);
    let writes_out = t.contains("\"$out\"") || t.contains("\"$merge\"") || t.contains("$out:") || t.contains("$merge:");
    match op.as_deref() {
        Some("find" | "findone" | "count" | "countdocuments" | "estimateddocumentcount" | "distinct" | "listcollections" | "listindexes" | "explain") => {
            StatementKind::Read
        }
        Some("aggregate") if !writes_out => StatementKind::Read,
        Some("aggregate") => StatementKind::Dml,
        Some(o) if o.starts_with("insert") || o.starts_with("update") || o.starts_with("delete") || o.starts_with("replace") || o.starts_with("findoneand") || o == "bulkwrite" => {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::split_with;
    use crate::traits::{STANDARD, SourceTraits};

    const MONGO: SourceTraits = SourceTraits { language: QueryLanguage::MongoJson, ..STANDARD };
    const CQL: SourceTraits = SourceTraits { language: QueryLanguage::Cql, ..STANDARD };
    const CYPHER: SourceTraits = SourceTraits { language: QueryLanguage::Cypher, ..STANDARD };

    #[test]
    fn splits_per_language() {
        let m = split_with("\n  db.orders.find({\"a\": \"x;y\"})\n\n", &MONGO);
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].sql, "db.orders.find({\"a\": \"x;y\"})");
        assert!(split_with("   ", &MONGO).is_empty());
        let c: Vec<String> = split_with("SELECT * FROM ks.t; INSERT INTO ks.t (a) VALUES ('x;y');", &CQL).into_iter().map(|s| s.sql).collect();
        assert_eq!(c, vec!["SELECT * FROM ks.t", "INSERT INTO ks.t (a) VALUES ('x;y')"]);
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
        assert_eq!(k("db.orders.deleteMany({})", L::MongoJson), S::Dml);
        assert_eq!(k("db.orders.drop()", L::MongoJson), S::Ddl);
        assert_eq!(k("what is this", L::MongoJson), S::Unknown);
        assert_eq!(k("use shop\ndb.getSiblingDB(\"x\").getCollection(\"o\").find({}).limit(5)", L::MongoJson), S::Read);
        assert_eq!(k("db.getCollection('o').insertOne({})", L::MongoJson), S::Dml);
        assert_eq!(k("db.runCommand({ping: 1})", L::MongoJson), S::Unknown);
        assert_eq!(k("SELECT * FROM ks.users WHERE id = 1", L::Cql), S::Read);
        assert_eq!(k("BEGIN BATCH INSERT INTO t (a) VALUES (1); APPLY BATCH", L::Cql), S::Dml);
        assert_eq!(k("CREATE TABLE ks.t (id int PRIMARY KEY)", L::Cql), S::Ddl);
        assert_eq!(k("MATCH (p:Person) RETURN p.name LIMIT 5", L::Cypher), S::Read);
        assert_eq!(k("MATCH (p:Person {name: 'A'}) SET p.age = 3", L::Cypher), S::Dml);
        assert_eq!(k("CREATE INDEX FOR (p:Person) ON (p.name)", L::Cypher), S::Ddl);
        assert_eq!(k("CALL db.labels()", L::Cypher), S::Unknown);
    }
}
