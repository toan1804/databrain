//! What a MongoDB command does, from the shell parser that runs it: read or
//! write, and which collections it reads and writes, pipeline stages
//! included (`$out` / `$merge` write; `$lookup`, `$graphLookup` and
//! `$unionWith` read). Classification and table access rules use this
//! instead of reading the text, so no spelling of a stage escapes them.

use databrain_connector_core::language::MongoInfo;
use databrain_connector_core::sql::StatementKind;
use mongodb::bson::{Bson, Document};

use crate::shell;

/// [`MongoInfo`] of `text`; `None` when the shell parser refuses it (then
/// it doesn't run either).
pub fn analyze(text: &str) -> Option<MongoInfo> {
    let plan = shell::parse(text, None).ok()?;
    let mut a = Acc { db: plan.db.clone(), reads: vec![], writes: vec![] };
    let kind = command_kind(&plan.command, &mut a, 0);
    Some(MongoInfo { kind, reads: a.reads, writes: a.writes })
}

struct Acc {
    db: Option<String>,
    reads: Vec<Vec<String>>,
    writes: Vec<Vec<String>>,
}

impl Acc {
    fn name(&self, db: Option<&str>, coll: &str) -> Vec<String> {
        match db.map(str::to_string).or_else(|| self.db.clone()) {
            Some(d) => vec![d, coll.to_string()],
            None => vec![coll.to_string()],
        }
    }
    fn read(&mut self, db: Option<&str>, coll: &str) {
        let n = self.name(db, coll);
        if !coll.is_empty() && !self.reads.contains(&n) {
            self.reads.push(n);
        }
    }
    fn write(&mut self, db: Option<&str>, coll: &str) {
        let n = self.name(db, coll);
        if !coll.is_empty() && !self.writes.contains(&n) {
            self.writes.push(n);
        }
    }
}

/// The command's name: its first key (case as written).
fn command_name(cmd: &Document) -> Option<&str> {
    cmd.keys().next().map(String::as_str)
}

fn command_kind(cmd: &Document, a: &mut Acc, depth: usize) -> StatementKind {
    use StatementKind as K;
    let Some(name) = command_name(cmd) else { return K::Unknown };
    let coll = cmd.get_str(name).ok().map(str::to_string);
    let main = |a: &mut Acc, write: bool| {
        if let Some(c) = &coll {
            if write { a.write(None, c) } else { a.read(None, c) }
        }
    };
    match name.to_ascii_lowercase().as_str() {
        "find" | "count" | "distinct" | "listindexes" | "collstats" => {
            main(a, false);
            K::Read
        }
        "listcollections" | "listdatabases" | "dbstats" => K::Read,
        "aggregate" => {
            main(a, false);
            let writes = cmd.get_array("pipeline").map(|p| pipeline(p, a, depth)).unwrap_or(false);
            if writes { K::Dml } else { K::Read }
        }
        "insert" | "update" | "delete" | "findandmodify" => {
            main(a, true);
            K::Dml
        }
        "create" | "drop" | "createindexes" | "dropindexes" | "collmod" => {
            main(a, true);
            K::Ddl
        }
        "renamecollection" | "dropdatabase" => K::Ddl,
        "mapreduce" => {
            main(a, false);
            match cmd.get("out") {
                Some(Bson::Document(d)) if d.contains_key("inline") => K::Read,
                Some(Bson::String(c)) => {
                    a.write(None, c);
                    K::Dml
                }
                _ => K::Dml,
            }
        }
        // EXPLAIN of a command counts as the command (as in SQL).
        "explain" if depth < 4 => match cmd.get_document("explain") {
            Ok(inner) => K::Read.max(command_kind(inner, a, depth + 1)),
            Err(_) => K::Unknown,
        },
        _ => K::Unknown,
    }
}

/// Walk a pipeline; true when a stage writes (`$out`, `$merge`).
fn pipeline(stages: &[Bson], a: &mut Acc, depth: usize) -> bool {
    let mut writes = false;
    for st in stages {
        let Bson::Document(st) = st else { continue };
        for (op, v) in st {
            match (op.as_str(), v) {
                ("$out", Bson::String(c)) => {
                    a.write(None, c);
                    writes = true;
                }
                ("$out", Bson::Document(d)) => {
                    a.write(d.get_str("db").ok(), d.get_str("coll").unwrap_or_default());
                    writes = true;
                }
                ("$merge", Bson::String(c)) => {
                    a.write(None, c);
                    writes = true;
                }
                ("$merge", Bson::Document(d)) => {
                    match d.get("into") {
                        Some(Bson::String(c)) => a.write(None, c),
                        Some(Bson::Document(i)) => a.write(i.get_str("db").ok(), i.get_str("coll").unwrap_or_default()),
                        _ => {}
                    }
                    writes = true;
                }
                // Any other spelling of a writing stage still writes.
                ("$out" | "$merge", _) => writes = true,
                ("$lookup" | "$graphLookup", Bson::Document(d)) => {
                    if let Ok(from) = d.get_str("from") {
                        a.read(None, from);
                    } else if let Ok(from) = d.get_document("from") {
                        a.read(from.get_str("db").ok(), from.get_str("coll").unwrap_or_default());
                    }
                    if let Ok(p) = d.get_array("pipeline") {
                        writes |= pipeline(p, a, depth + 1);
                    }
                }
                ("$unionWith", Bson::String(c)) => a.read(None, c),
                ("$unionWith", Bson::Document(d)) => {
                    a.read(None, d.get_str("coll").unwrap_or_default());
                    if let Ok(p) = d.get_array("pipeline") {
                        writes |= pipeline(p, a, depth + 1);
                    }
                }
                ("$facet", Bson::Document(f)) => {
                    for (_, p) in f {
                        if let Bson::Array(p) = p {
                            writes |= pipeline(p, a, depth + 1);
                        }
                    }
                }
                _ => {}
            }
        }
    }
    writes
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kind(t: &str) -> StatementKind {
        analyze(t).map(|i| i.kind).unwrap_or(StatementKind::Unknown)
    }
    fn names(v: &[Vec<String>]) -> Vec<String> {
        v.iter().map(|n| n.join(".")).collect()
    }

    /// Every spelling of a writing stage the shell accepts is a write.
    #[test]
    fn writing_stages_in_any_spelling() {
        for t in [
            "db.orders.aggregate([{ $out : 'copy' }])",
            "db.orders.aggregate([{$out:'copy'}])",
            "db.orders.aggregate([{ \"$out\" : \"copy\" }])",
            "db.orders.aggregate([{'$out': {db: 'other', coll: 'copy'}}])",
            "db.orders.aggregate([{ $merge : 'copy' }])",
            "db.orders.aggregate([{$merge: {into: {db: 'x', coll: 'copy'}, whenMatched: 'replace'}}])",
            "db.orders.aggregate([{$lookup: {from: 'c', as: 'c', pipeline: [{$match: {}}]}}, {$facet: {a: [{$match: {}}]}}, {\n  $out\n  :\n  'copy'\n}])",
            r#"{"aggregate": "orders", "pipeline": [{"$out": "copy"}], "cursor": {}}"#,
            r#"db.runCommand({aggregate: 'orders', pipeline: [{ $merge : 'copy' }], cursor: {}})"#,
        ] {
            assert_eq!(kind(t), StatementKind::Dml, "{t}");
        }
        let i = analyze("use shop\ndb.orders.aggregate([{$lookup: {from: 'customers', localField: 'c', foreignField: '_id', as: 'c'}}, { $out : 'copy' }])")
            .unwrap();
        assert_eq!(names(&i.reads), ["shop.orders", "shop.customers"]);
        assert_eq!(names(&i.writes), ["shop.copy"]);
        let i = analyze("db.orders.aggregate([{'$out': {db: 'other', coll: 'copy'}}])").unwrap();
        assert_eq!(names(&i.writes), ["other.copy"]);
    }

    #[test]
    fn reads_writes_and_others() {
        assert_eq!(kind("db.orders.find({status: 'A'}).limit(5)"), StatementKind::Read);
        assert_eq!(kind("db.orders.aggregate([{$match: {outcome: '$outside'}}, {$project: {out: 1}}])"), StatementKind::Read, "field names aren't stages");
        assert_eq!(kind("db.orders.deleteMany({})"), StatementKind::Dml);
        assert_eq!(kind("db.orders.drop()"), StatementKind::Ddl);
        assert_eq!(kind("db.runCommand({ping: 1})"), StatementKind::Unknown);
        assert_eq!(kind("db.runCommand({explain: {aggregate: 'orders', pipeline: [{$out: 'x'}], cursor: {}}})"), StatementKind::Dml);
        assert!(analyze("this is not a command").is_none());
        let i = analyze("db.getSiblingDB('hr').getCollection('pay').deleteMany({})").unwrap();
        assert_eq!(names(&i.writes), ["hr.pay"]);
    }
}
