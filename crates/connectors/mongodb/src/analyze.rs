//! What a MongoDB command does, from the shell parser that runs it: read or
//! write, and which collections it reads and writes, pipeline stages
//! included (`$out` / `$merge` write; `$lookup`, `$graphLookup` and
//! `$unionWith` read). Classification and table access rules use this
//! instead of reading the text, so no spelling of a stage escapes them.

use databrain_connector_core::language::{MongoInfo, MongoJoin};
use databrain_connector_core::sql::StatementKind;
use mongodb::bson::{Bson, Document};

use crate::shell;

/// [`MongoInfo`] of `text`; `None` when the shell parser refuses it (then
/// it doesn't run either).
pub fn analyze(text: &str) -> Option<MongoInfo> {
    let plan = shell::parse(text, None).ok()?;
    let mut a = Acc { db: plan.db.clone(), reads: vec![], writes: vec![] };
    let kind = command_kind(&plan.command, &mut a, 0);
    let joins = lookups(&plan.command, plan.db.as_deref());
    Some(MongoInfo { kind, reads: a.reads, writes: a.writes, every_document: every_document(&plan.command), joins })
}

/// A delete of every match (`limit: 0`) or an update of every match
/// (`multi: true`) whose filter is empty: every document of the collection.
fn every_document(cmd: &Document) -> bool {
    let empty = |d: &Document| d.get_document("q").is_ok_and(Document::is_empty);
    let statements = |key: &str| cmd.get_array(key).into_iter().flatten().filter_map(Bson::as_document);
    match command_name(cmd).map(str::to_ascii_lowercase).as_deref() {
        Some("delete") => statements("deletes").any(|d| empty(d) && d.get("limit").is_some_and(|l| l.as_i64().or(l.as_i32().map(i64::from)) == Some(0))),
        Some("update") => statements("updates").any(|d| empty(d) && d.get_bool("multi").unwrap_or(false)),
        _ => false,
    }
}

/// Equality `$lookup`s (`localField` / `foreignField`) at the top of an
/// aggregate's pipeline whose `localField` is still a field of the
/// aggregated collection: only stages that keep documents as they are
/// (`$match`, `$sort`, `$limit`, `$skip`, `$sample`, `$lookup`, `$unwind`)
/// may come before it, and the field must not be inside an earlier
/// lookup's `as`. Anything else (`$project`, `$group`, `$addFields`,
/// pipeline lookups, other databases) is not learned.
fn lookups(cmd: &Document, db: Option<&str>) -> Vec<MongoJoin> {
    let mut out = Vec::new();
    if !command_name(cmd).is_some_and(|n| n.eq_ignore_ascii_case("aggregate")) {
        return out;
    }
    let (Ok(main), Ok(stages)) = (cmd.get_str("aggregate"), cmd.get_array("pipeline")) else { return out };
    let name = |coll: &str| match db {
        Some(d) => vec![d.to_string(), coll.to_string()],
        None => vec![coll.to_string()],
    };
    let mut added: Vec<String> = Vec::new();
    for st in stages {
        let Bson::Document(st) = st else { return out };
        let Some((op, v)) = st.iter().next().filter(|_| st.len() == 1) else { return out };
        match (op.as_str(), v) {
            ("$match" | "$sort" | "$limit" | "$skip" | "$sample" | "$unwind", _) => {}
            ("$lookup", Bson::Document(d)) => {
                if let (Ok(from), Ok(local), Ok(foreign)) = (d.get_str("from"), d.get_str("localField"), d.get_str("foreignField"))
                    && !local.is_empty()
                    && !foreign.is_empty()
                    && !added.iter().any(|a| local == a || local.starts_with(&format!("{a}.")))
                {
                    out.push(MongoJoin { local: name(main), local_field: local.to_string(), from: name(from), foreign_field: foreign.to_string() });
                }
                if let Ok(as_) = d.get_str("as") {
                    added.push(as_.to_string());
                }
            }
            _ => return out,
        }
    }
    out
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
            // A view (`createCollection("v", {viewOn: "orders", pipeline})`)
            // shows its source's documents: it reads them, and what its
            // pipeline looks up.
            if let Ok(source) = cmd.get_str("viewOn") {
                a.read(None, source);
            }
            if let Ok(p) = cmd.get_array("pipeline") {
                pipeline(p, a, depth);
            }
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
                // `{replace | merge | reduce: "coll", db: "other"}`.
                Some(Bson::Document(d)) => match ["replace", "merge", "reduce"].iter().find_map(|k| d.get_str(k).ok()) {
                    Some(c) => {
                        a.write(d.get_str("db").ok(), c);
                        K::Dml
                    }
                    None => K::Unknown,
                },
                // Where it writes can't be told.
                _ => K::Unknown,
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

/// Pipelines nested this deep ($lookup in $facet in $unionWith…) are not
/// walked further: counted as writing, the safe side.
const MAX_PIPELINE_DEPTH: usize = 8;

/// Walk a pipeline; true when a stage writes (`$out`, `$merge`).
fn pipeline(stages: &[Bson], a: &mut Acc, depth: usize) -> bool {
    if depth >= MAX_PIPELINE_DEPTH {
        return true;
    }
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

    fn joins(t: &str) -> Vec<String> {
        analyze(t).unwrap().joins.into_iter().map(|j| format!("{}.{} = {}.{}", j.local.join("."), j.local_field, j.from.join("."), j.foreign_field)).collect()
    }

    #[test]
    fn lookup_equalities_are_joins() {
        assert_eq!(
            joins(
                "use shop\ndb.orders.aggregate([{$match: {s: 1}}, {$lookup: {from: 'customers', localField: 'cust', foreignField: '_id', as: 'c'}}, {$unwind: '$c'}, {$lookup: {from: 'items', localField: '_id', foreignField: 'order_id', as: 'i'}}])"
            ),
            ["shop.orders.cust = shop.customers._id", "shop.orders._id = shop.items.order_id"]
        );
        // A field of an earlier lookup's result is not the collection's.
        assert_eq!(
            joins(
                "db.orders.aggregate([{$lookup: {from: 'customers', localField: 'cust', foreignField: '_id', as: 'c'}}, {$lookup: {from: 'regions', localField: 'c.region', foreignField: '_id', as: 'r'}}])"
            ),
            ["orders.cust = customers._id"]
        );
    }

    #[test]
    fn lookups_after_reshaping_or_without_fields_are_not_joins() {
        assert!(
            joins("db.orders.aggregate([{$project: {cust: '$x'}}, {$lookup: {from: 'customers', localField: 'cust', foreignField: '_id', as: 'c'}}])")
                .is_empty()
        );
        assert!(
            joins("db.orders.aggregate([{$group: {_id: '$cust'}}, {$lookup: {from: 'customers', localField: '_id', foreignField: '_id', as: 'c'}}])")
                .is_empty()
        );
        assert!(joins("db.orders.aggregate([{$lookup: {from: 'customers', as: 'c', pipeline: [{$match: {}}]}}])").is_empty());
        assert!(joins("db.orders.find({})").is_empty());
    }

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
        // Nested past the walk's limit: counted as writing.
        let mut deep = "{$match: {}}".to_string();
        for _ in 0..MAX_PIPELINE_DEPTH {
            deep = format!("{{$facet: {{x: [{deep}]}}}}");
        }
        assert_eq!(kind(&format!("db.orders.aggregate([{deep}])")), StatementKind::Dml, "too deep to check");
        assert_eq!(kind("db.orders.aggregate([{$facet: {x: [{$facet: {y: [{$match: {}}]}}]}}])"), StatementKind::Read);
        let i = analyze("db.getSiblingDB('hr').getCollection('pay').deleteMany({})").unwrap();
        assert_eq!(names(&i.writes), ["hr.pay"]);
    }

    /// A delete or update of every document asks, as SQL without WHERE does.
    #[test]
    fn writes_to_every_document() {
        let every = |t: &str| analyze(t).unwrap().every_document;
        for t in [
            "db.t.deleteMany({})",
            "db.t.updateMany({}, {$set: {a: 1}})",
            r#"{"delete": "t", "deletes": [{"q": {}, "limit": 0}]}"#,
            r#"db.runCommand({update: 't', updates: [{q: {a: 1}, u: {}}, {q: {}, u: {$set: {b: 1}}, multi: true}]})"#,
        ] {
            assert!(every(t), "{t}");
        }
        for t in ["db.t.deleteMany({a: 1})", "db.t.deleteOne({})", "db.t.updateOne({}, {$set: {a: 1}})", "db.t.replaceOne({}, {a: 1})", "db.t.find({})"] {
            assert!(!every(t), "{t}");
        }
        // As the app classifies it: confirmed, named by its method.
        databrain_connector_core::language::set_mongo_analyzer(analyze);
        let c = databrain_connector_core::language::classify("db.orders.deleteMany({})", databrain_connector_core::traits::QueryLanguage::MongoJson);
        assert!(c.missing_where && c.kind == StatementKind::Dml);
        assert_eq!(c.keyword, "DELETEMANY");
        let c = databrain_connector_core::language::classify("db.orders.deleteMany({a: 1})", databrain_connector_core::traits::QueryLanguage::MongoJson);
        assert!(!c.missing_where && c.kind == StatementKind::Dml);
        // No filter at all: not run (as in mongosh), and never a read.
        assert_eq!(
            databrain_connector_core::language::classify("db.orders.deleteMany()", databrain_connector_core::traits::QueryLanguage::MongoJson).kind,
            StatementKind::Unknown
        );
    }

    /// mapReduce into a collection and a view report the collections they touch.
    #[test]
    fn map_reduce_outputs_and_views() {
        let i = analyze("use shop\ndb.runCommand({mapReduce: 'orders', map: 'function(){}', reduce: 'function(){}', out: {merge: 'totals', db: 'reports'}})")
            .unwrap();
        assert_eq!((i.kind, names(&i.reads), names(&i.writes)), (StatementKind::Dml, vec!["shop.orders".to_string()], vec!["reports.totals".to_string()]));
        let i = analyze("db.runCommand({mapReduce: 'orders', map: 'f', reduce: 'g', out: {replace: 'totals'}})").unwrap();
        assert_eq!(names(&i.writes), ["totals"]);
        assert_eq!(kind("db.runCommand({mapReduce: 'orders', map: 'f', reduce: 'g', out: {inline: 1}})"), StatementKind::Read);
        assert_eq!(kind("db.runCommand({mapReduce: 'orders', map: 'f', reduce: 'g', out: {elsewhere: 1}})"), StatementKind::Unknown);
        assert_eq!(kind("db.runCommand({mapReduce: 'orders', map: 'f', reduce: 'g'})"), StatementKind::Unknown);
        let i = analyze("use shop\ndb.createCollection('big_orders', {viewOn: 'orders', pipeline: [{$match: {total: {$gt: 100}}}, {$lookup: {from: 'customers', localField: 'c', foreignField: '_id', as: 'c'}}]})").unwrap();
        assert_eq!(i.kind, StatementKind::Ddl);
        assert_eq!(names(&i.reads), ["shop.orders", "shop.customers"]);
        assert_eq!(names(&i.writes), ["shop.big_orders"]);
        let i = analyze("db.runCommand({collMod: 'v', viewOn: 'payroll', pipeline: []})").unwrap();
        assert_eq!(names(&i.reads), ["payroll"]);
    }
}
