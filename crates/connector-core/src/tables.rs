//! Which tables a script reads and which it changes, for per-table access
//! rules (the AI may only read or write some tables). Fail-closed: a
//! statement whose tables can't be told is reported as `opaque`, and the
//! caller refuses it when rules are set.
//!
//! SQL is parsed with sqlparser. A table named inside a query (`SELECT`,
//! subqueries, CTE bodies, `INSERT … SELECT` sources) is read; a table named
//! by the write statement itself (`INSERT INTO t`, `UPDATE t`, `DELETE FROM t`,
//! `MERGE INTO t … USING s`, `CREATE/ALTER/DROP/TRUNCATE`, `COPY`) is written.
//! Tables in the non-query parts of a write (`UPDATE … FROM s`, `MERGE … USING s`)
//! count as written too: stricter than needed, never looser.

use std::ops::ControlFlow;

use sqlparser::ast::{ObjectName, ObjectNamePart, ObjectType, Query, Statement, Visit, Visitor};
use sqlparser::parser::Parser;

use crate::sql::{dialect_for, leading_keyword, split_statements};
use crate::traits::QueryLanguage;
use crate::types::ConnectorKind;

/// A table name as written: its parts (`["sales", "orders"]`), unquoted.
pub type TableName = Vec<String>;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TableRefs {
    pub reads: Vec<TableName>,
    pub writes: Vec<TableName>,
    /// Statements whose tables can't be told (unparsed SQL, procedure calls,
    /// dynamic SQL, other query languages): their leading keyword.
    pub opaque: Vec<String>,
    /// A statement changes the default database/schema (`USE`, `SET search_path`…),
    /// so later unqualified names can't be placed.
    pub changes_default: bool,
}

impl TableRefs {
    fn push(list: &mut Vec<TableName>, t: TableName) {
        if !t.is_empty() && !list.iter().any(|x| eq_name(x, &t)) {
            list.push(t);
        }
    }
}

fn eq_name(a: &TableName, b: &TableName) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.eq_ignore_ascii_case(y))
}

fn parts(name: &ObjectName) -> TableName {
    name.0
        .iter()
        .map(|p| match p {
            ObjectNamePart::Identifier(i) => i.value.clone(),
            other => other.to_string(),
        })
        .collect()
}

/// Statements that run code the parser can't see into.
const OPAQUE_KEYWORDS: &[&str] = &["CALL", "EXEC", "EXECUTE", "DO", "BEGIN", "DECLARE", "PREPARE", "IMPORT", "EXPORT", "LOAD", "ATTACH", "DETACH", "PRAGMA", "INSTALL"];

/// Read and written tables of `sql` (a script) in this connection's language.
pub fn table_refs(sql: &str, kind: ConnectorKind) -> TableRefs {
    let language = kind.traits().language;
    if !language.is_sql() {
        return match language {
            QueryLanguage::MongoJson => crate::language::mongo_table_refs(sql),
            _ => TableRefs { opaque: vec![leading_keyword(sql)], ..Default::default() },
        };
    }
    let dialect = dialect_for(kind);
    let mut out = TableRefs::default();
    for span in split_statements(sql, kind) {
        let text = span.sql.trim();
        if text.is_empty() {
            continue;
        }
        let keyword = leading_keyword(text);
        if changes_default(text) {
            out.changes_default = true;
            continue;
        }
        if OPAQUE_KEYWORDS.contains(&keyword.as_str()) {
            out.opaque.push(keyword);
            continue;
        }
        match Parser::parse_sql(dialect.as_ref(), text) {
            Ok(stmts) if !stmts.is_empty() => {
                for s in &stmts {
                    collect(s, &mut out);
                }
            }
            _ => out.opaque.push(keyword),
        }
    }
    out
}

/// `USE db`, `SET search_path …`, `SET SCHEMA …`, `ALTER SESSION SET CURRENT_SCHEMA …`, `USE CATALOG/SCHEMA …`.
fn changes_default(text: &str) -> bool {
    let words: Vec<String> = text.split_whitespace().take(5).map(|w| w.trim_matches(|c: char| !c.is_alphanumeric() && c != '_').to_ascii_lowercase()).collect();
    let w = |i: usize| words.get(i).map(String::as_str).unwrap_or("");
    match w(0) {
        "use" => true,
        "set" => matches!(w(1), "search_path" | "schema" | "catalog" | "database" | "current_schema" | "dataset") || (w(1) == "session" && matches!(w(2), "search_path" | "schema" | "catalog" | "database")),
        "alter" => w(1) == "session" && w(2) == "set" && matches!(w(3), "current_schema" | "container"),
        _ => false,
    }
}

/// Visits one statement: query depth and the depth of the write statement
/// being visited tell reads from writes.
struct Collector<'a> {
    out: &'a mut TableRefs,
    depth: usize,
    /// Query depth of each enclosing non-query statement.
    writes_at: Vec<usize>,
    /// CTE names seen (single-part references to them are not tables).
    ctes: Vec<String>,
    reads: Vec<TableName>,
    writes: Vec<TableName>,
}

impl Visitor for Collector<'_> {
    type Break = ();

    fn pre_visit_query(&mut self, q: &Query) -> ControlFlow<()> {
        self.depth += 1;
        if let Some(with) = &q.with {
            self.ctes.extend(with.cte_tables.iter().map(|c| c.alias.name.value.to_ascii_lowercase()));
        }
        ControlFlow::Continue(())
    }

    fn post_visit_query(&mut self, _q: &Query) -> ControlFlow<()> {
        self.depth -= 1;
        ControlFlow::Continue(())
    }

    fn pre_visit_statement(&mut self, s: &Statement) -> ControlFlow<()> {
        if !matches!(s, Statement::Query(_) | Statement::Explain { .. } | Statement::ExplainTable { .. }) {
            self.writes_at.push(self.depth);
        }
        // DROP's names are not visited as relations.
        if let Statement::Drop { object_type, names, .. } = s {
            if matches!(object_type, ObjectType::Table | ObjectType::View | ObjectType::MaterializedView) {
                for n in names {
                    self.writes.push(parts(n));
                }
            }
        }
        ControlFlow::Continue(())
    }

    fn post_visit_statement(&mut self, s: &Statement) -> ControlFlow<()> {
        if !matches!(s, Statement::Query(_) | Statement::Explain { .. } | Statement::ExplainTable { .. }) {
            self.writes_at.pop();
        }
        ControlFlow::Continue(())
    }

    fn pre_visit_relation(&mut self, r: &ObjectName) -> ControlFlow<()> {
        let name = parts(r);
        let is_write = self.writes_at.last() == Some(&self.depth);
        if is_write {
            self.writes.push(name);
        } else {
            self.reads.push(name);
        }
        ControlFlow::Continue(())
    }
}

fn collect(s: &Statement, out: &mut TableRefs) {
    let mut c = Collector { out, depth: 0, writes_at: vec![], ctes: vec![], reads: vec![], writes: vec![] };
    let _ = s.visit(&mut c);
    let ctes = std::mem::take(&mut c.ctes);
    let not_cte = |t: &TableName| !(t.len() == 1 && ctes.contains(&t[0].to_ascii_lowercase()));
    let (reads, writes) = (std::mem::take(&mut c.reads), std::mem::take(&mut c.writes));
    let out = c.out;
    for t in reads.into_iter().filter(not_cte) {
        TableRefs::push(&mut out.reads, t);
    }
    for t in writes.into_iter().filter(not_cte) {
        TableRefs::push(&mut out.writes, t);
    }
    // Procedures, GRANT, dynamic SQL…: statements that change something
    // without naming a table we could check.
    let keyword = leading_keyword(&s.to_string());
    let no_tables = !matches!(s, Statement::Query(_));
    if no_tables
        && out.reads.is_empty()
        && out.writes.is_empty()
        && matches!(crate::sql::keyword_kind(&keyword), crate::sql::StatementKind::Unknown | crate::sql::StatementKind::Dml)
    {
        out.opaque.push(keyword);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(v: &[TableName]) -> Vec<String> {
        v.iter().map(|t| t.join(".")).collect()
    }

    fn refs(sql: &str) -> (Vec<String>, Vec<String>) {
        let r = table_refs(sql, ConnectorKind::Postgres);
        assert!(r.opaque.is_empty(), "{sql}: {:?}", r.opaque);
        (names(&r.reads), names(&r.writes))
    }

    #[test]
    fn reads_and_writes() {
        assert_eq!(refs("select * from sales.orders o join crm.customers c on c.id = o.cid"), (vec!["sales.orders".into(), "crm.customers".into()], vec![]));
        // CTEs are not tables; their bodies are read.
        assert_eq!(refs("with x as (select * from a.t) select * from x join b.u using (id)"), (vec!["a.t".into(), "b.u".into()], vec![]));
        assert_eq!(refs("select (select max(v) from s.v) from s.t where id in (select id from s.w)").0, ["s.v", "s.t", "s.w"]);
        assert_eq!(refs("insert into dst.t select * from src.t"), (vec!["src.t".into()], vec!["dst.t".into()]));
        assert_eq!(refs("update s.t set a = 1 where id in (select id from s.u)"), (vec!["s.u".into()], vec!["s.t".into()]));
        assert_eq!(refs("delete from s.t where a = 1").1, ["s.t"]);
        assert_eq!(refs("create table s.n as select * from s.o"), (vec!["s.o".into()], vec!["s.n".into()]));
        assert_eq!(refs("drop table s.a, s.b").1, ["s.a", "s.b"]);
        assert_eq!(refs("truncate table s.a").1, ["s.a"]);
        assert_eq!(refs("alter table s.a add column x int").1, ["s.a"]);
        assert_eq!(refs("create view s.v as select * from s.t"), (vec!["s.t".into()], vec!["s.v".into()]));
        // WITH … INSERT: the target is written although it sits in a query.
        let (r, w) = refs("with x as (select * from s.src) insert into s.dst select * from x");
        assert_eq!((r, w), (vec!["s.src".to_string()], vec!["s.dst".to_string()]));
        // MERGE's source counts as written (stricter, never looser).
        assert!(refs("merge into s.t using s.u on t.id = u.id when matched then update set a = u.a").1.contains(&"s.t".to_string()));
        // Quoted names lose their quotes.
        assert_eq!(refs(r#"select * from "Sales"."Big Orders""#).0, ["Sales.Big Orders"]);
        // Several statements.
        assert_eq!(refs("select 1 from a; select 2 from b").0, ["a", "b"]);
        assert_eq!(refs("select 1"), (vec![], vec![]));
    }

    #[test]
    fn opaque_and_default_changes() {
        let r = table_refs("call do_things()", ConnectorKind::Postgres);
        assert_eq!(r.opaque, ["CALL"]);
        assert!(!table_refs("selec * fro t", ConnectorKind::Postgres).opaque.is_empty());
        for s in ["use other_db", "SET search_path TO secret", "set schema 'x'", "alter session set current_schema = hr", "USE CATALOG prod"] {
            assert!(table_refs(s, ConnectorKind::Snowflake).changes_default, "{s}");
        }
        assert!(!table_refs("set timezone = 'UTC'", ConnectorKind::Postgres).changes_default);
        assert!(!table_refs("cypher", ConnectorKind::Neo4j).opaque.is_empty());
    }

    #[test]
    fn other_dialects() {
        let r = table_refs("select top 5 * from dbo.orders", ConnectorKind::Mssql);
        assert_eq!(names(&r.reads), ["dbo.orders"]);
        let r = table_refs("select * from `proj.ds.t`", ConnectorKind::Bigquery);
        assert_eq!(r.reads.len(), 1);
        let r = table_refs("select * from cat.sch.t", ConnectorKind::Databricks);
        assert_eq!(names(&r.reads), ["cat.sch.t"]);
    }
}
