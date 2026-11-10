//! Which columns of a result hold PII, found through the SQL that made it:
//! a masked column stays masked under an alias (`email AS e`), inside an
//! expression (`lower(email)`, `max(email)`) and through `SELECT *`.

use std::collections::{HashMap, HashSet};
use std::ops::ControlFlow;

use databrain_connector_core::ConnectorKind;
use databrain_connector_core::sqlparser::ast::{Expr, ObjectNamePart, TableFactor, Visit, Visitor};
use databrain_connector_core::sqlparser::parser::Parser;
use databrain_lineage::{EdgeKind, Lineage, NoCatalog, NodeKind, analyze};

use crate::policy::{name_parts, same_table};

/// Whether `column` of `table` (a table as the SQL names it, `None` when
/// unknown) is sensitive.
pub type IsPii<'a> = dyn Fn(Option<&str>, &str) -> bool + 'a;

/// One flag per result column (`columns`, in order). `rule_columns`: the
/// column part of every PII rule, for when the SQL can't be traced (then a
/// column is masked if the SQL mentions any of them: fail closed).
pub fn masked_columns(sql: &str, kind: ConnectorKind, columns: &[String], rule_columns: &[String], is_pii: &IsPii) -> Vec<bool> {
    if rule_columns.is_empty() {
        return vec![false; columns.len()];
    }
    let mentions = mentions_any(sql, rule_columns);
    if !kind.traits().language.is_sql() {
        return non_sql_masks(sql, kind, columns, rule_columns, is_pii, mentions);
    }
    let g = analyze(sql, kind, &NoCatalog);
    let tables: Vec<&str> = g.nodes.iter().filter(|n| n.kind == NodeKind::Table).map(|n| n.name.as_str()).collect();
    let result = g.nodes.iter().rev().find(|n| n.kind == NodeKind::Result);
    let names = names_in(sql, kind);
    // `row_to_json(u)`, `to_jsonb(users)`, `u::text`, `SELECT u FROM users u`:
    // lineage reads the row name as a column; it holds every column of the
    // table, so it is masked when any rule applies to that table.
    let ruled = |t: &str| rule_columns.iter().any(|rc| is_pii(Some(t), rc));
    let row_names = |t: &str| {
        let parts = name_parts(t);
        let mut out: Vec<String> = parts.last().cloned().into_iter().collect();
        out.extend(names.aliases.iter().filter(|(_, table)| same_table(table, &parts)).map(|(a, _)| a.clone()));
        out
    };
    let whole_row = |t: &str, col: &str| ruled(t) && row_names(t).iter().any(|n| n == col);
    // A row of a ruled table used as a value somewhere lineage can't place
    // (an ambiguous `c` in a join): untraced columns are masked.
    let row_used = tables.iter().any(|t| ruled(t) && row_names(t).iter().any(|n| names.idents.contains(n)));
    columns
        .iter()
        .map(|c| {
            // By name: the column as returned, from any table the SQL reads.
            if is_pii(None, c) || tables.iter().any(|t| is_pii(Some(t), c)) {
                return true;
            }
            let Some(r) = result.filter(|_| !g.partial) else { return mentions || row_used };
            let sources = sources(&g, &r.id, c);
            if sources.is_empty() {
                // Not traced (an expression the lineage can't place): masked
                // when the SQL names a sensitive column, or a ruled table's
                // whole row, at all.
                return mentions || row_used;
            }
            sources.iter().any(|(t, col)| if col == "*" { is_pii(Some(t), c) } else { is_pii(Some(t), col) || is_pii(None, col) || whole_row(t, col) })
        })
        .collect()
}

/// What the SQL names, from its syntax tree (lower-cased): `(alias, table
/// parts)` of every aliased table (`FROM users u`, `JOIN public.orders AS
/// o`), and every bare identifier used as a value (`row_to_json(u)`,
/// `u::text`; not `u.email`). Empty when the SQL doesn't parse (the
/// lineage then fails closed on its own).
struct Names {
    aliases: Vec<(String, Vec<String>)>,
    idents: HashSet<String>,
}

impl Visitor for Names {
    type Break = ();
    fn pre_visit_table_factor(&mut self, f: &TableFactor) -> ControlFlow<()> {
        if let TableFactor::Table { name, alias: Some(a), .. } = f {
            let parts = name.0.iter().filter_map(|p| if let ObjectNamePart::Identifier(i) = p { Some(i.value.to_ascii_lowercase()) } else { None }).collect();
            self.aliases.push((a.name.value.to_ascii_lowercase(), parts));
        }
        ControlFlow::Continue(())
    }
    fn pre_visit_expr(&mut self, e: &Expr) -> ControlFlow<()> {
        if let Expr::Identifier(i) = e {
            self.idents.insert(i.value.to_ascii_lowercase());
        }
        ControlFlow::Continue(())
    }
}

fn names_in(sql: &str, kind: ConnectorKind) -> Names {
    let mut v = Names { aliases: Vec::new(), idents: HashSet::new() };
    let dialect = databrain_connector_core::sql::dialect_for(kind);
    if let Ok(stmts) = Parser::parse_sql(dialect.as_ref(), sql) {
        for s in &stmts {
            let _ = s.visit(&mut v);
        }
    }
    v
}

/// MongoDB and other non-SQL queries: no lineage, so a column is masked by
/// name, against the collections the query reads or writes (a rule
/// `users.email` masks `email` of `db.users.find({})`). When the
/// collections can't be told, a column named like any rule's column is
/// masked (fail closed). Dotted names (`profile.email`) match by their last
/// part too.
fn non_sql_masks(sql: &str, kind: ConnectorKind, columns: &[String], rule_columns: &[String], is_pii: &IsPii, mentions: bool) -> Vec<bool> {
    let refs = databrain_connector_core::tables::table_refs(sql, kind);
    let tables: Vec<String> = refs.reads.iter().chain(&refs.writes).map(|t| t.join(".")).collect();
    let known = refs.opaque.is_empty() && !tables.is_empty();
    // Whole documents under a new name (`$lookup … as: 'u'`, `{x: '$$ROOT'}`,
    // `$push: '$$ROOT'`) come back as one JSON cell that matches no rule by
    // name: when a collection read has a rule, every column is masked.
    let whole_doc = ["$$ROOT", "$$CURRENT", "$lookup", "$graphLookup"].iter().any(|op| sql.contains(op));
    if whole_doc && (!known || tables.iter().any(|t| rule_columns.iter().any(|r| is_pii(Some(t), r)))) {
        return vec![true; columns.len()];
    }
    columns
        .iter()
        .map(|c| {
            let col = c.to_ascii_lowercase();
            let leaf = col.rsplit('.').next().unwrap_or(&col).to_string();
            if mentions || is_pii(None, &col) || is_pii(None, &leaf) {
                return true;
            }
            if known {
                tables.iter().any(|t| is_pii(Some(t), &col) || is_pii(Some(t), &leaf))
            } else {
                rule_columns.iter().any(|r| r.eq_ignore_ascii_case(&col) || r.eq_ignore_ascii_case(&leaf))
            }
        })
        .collect()
}

/// `(table, column)` pairs whose values reach `column` of node `id`
/// (copied, computed or aggregated; not filters or joins). `*`: the whole
/// table, column by column.
fn sources(g: &Lineage, id: &str, column: &str) -> Vec<(String, String)> {
    let nodes: HashMap<&str, &databrain_lineage::Node> = g.nodes.iter().map(|n| (n.id.as_str(), n)).collect();
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let mut todo = vec![(id.to_string(), column.to_ascii_lowercase())];
    while let Some((node, col)) = todo.pop() {
        if !seen.insert((node.clone(), col.clone())) {
            continue;
        }
        for e in &g.edges {
            if e.to != node || matches!(e.kind, EdgeKind::Filter | EdgeKind::Join) {
                continue;
            }
            // An edge into the whole node (`*`) feeds every column.
            if e.to_column.as_deref().is_some_and(|t| !t.eq_ignore_ascii_case(&col) && !t.ends_with('*')) {
                continue;
            }
            let from_col = e.from_column.clone().map(|c| c.to_ascii_lowercase()).filter(|c| !c.ends_with('*')).unwrap_or_else(|| col.clone());
            match nodes.get(e.from.as_str()) {
                Some(n) if n.kind == NodeKind::Table => {
                    out.push((n.name.clone(), if e.from_column.as_deref().is_none_or(|c| c.ends_with('*')) { "*".into() } else { from_col }))
                }
                Some(_) => todo.push((e.from.clone(), from_col)),
                None => {}
            }
        }
    }
    out
}

/// Whether `sql` names any of `names` as a word (case-insensitive).
fn mentions_any(sql: &str, names: &[String]) -> bool {
    let words: HashSet<String> = sql.split(|c: char| !(c.is_alphanumeric() || c == '_')).filter(|w| !w.is_empty()).map(str::to_ascii_lowercase).collect();
    names.iter().any(|n| words.contains(&n.to_ascii_lowercase())) || sql.contains('*')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::is_pii;
    use databrain_workspace::AiPolicy;

    /// Columns masked under `rules` (as a connection's PII settings), the way the tools ask.
    fn masked(kind: ConnectorKind, rules: &[&str], sql: &str, columns: &[&str]) -> Vec<bool> {
        let pol = AiPolicy { pii_columns: rules.iter().map(|r| r.to_string()).collect(), ..Default::default() };
        let rule_columns: Vec<String> = pol.pii_columns.iter().map(|p| p.rsplit('.').next().unwrap_or("").to_ascii_lowercase()).collect();
        let cols: Vec<String> = columns.iter().map(|c| c.to_string()).collect();
        masked_columns(sql, kind, &cols, &rule_columns, &|t, c| is_pii(&pol, t, c))
    }

    fn pg(rules: &[&str], sql: &str, columns: &[&str]) -> Vec<bool> {
        masked(ConnectorKind::Postgres, rules, sql, columns)
    }

    #[test]
    fn traces_sensitive_columns_through_the_sql() {
        let users = &["users.email"];
        assert_eq!(pg(users, "select email, name from users", &["email", "name"]), [true, false]);
        // Renamed, computed, aggregated: still the email.
        assert_eq!(pg(users, "select email as contact, name from users", &["contact", "name"]), [true, false]);
        assert_eq!(pg(users, "select lower(email) l, count(id) n from users group by 1", &["l", "n"]), [true, false]);
        // count(*) comes from no column: masked like anything not traced, as the SQL names email.
        assert_eq!(pg(users, "select lower(email) l, count(*) n from users group by 1", &["l", "n"]), [true, true]);
        assert_eq!(pg(users, "select max(u.email) m from public.users u", &["m"]), [true]);
        assert_eq!(pg(users, "select * from users", &["id", "email", "name"]), [false, true, false]);
        assert_eq!(pg(users, "select u.* from users u", &["id", "email"]), [false, true]);
        assert_eq!(pg(users, "with x as (select email as m from users) select m as k from x", &["k"]), [true]);
        assert_eq!(pg(users, "select k from (select email k from users) s", &["k"]), [true]);
        assert_eq!(pg(users, "select o.id, u.email as who from orders o join users u on u.id = o.user_id", &["id", "who"]), [false, true]);
        assert_eq!(pg(users, "select name from users union all select email from users", &["name"]), [true], "a union mixes the columns");
        // Used only to filter or join: the values are not in the result.
        assert_eq!(pg(users, "select id from users where email like '%@x.io'", &["id"]), [false]);
        // A rule for one table does not mask the same name elsewhere.
        assert_eq!(pg(users, "select email from leads", &["email"]), [false]);
        // Whole rows: every column of the table in one value.
        for sql in [
            "select row_to_json(u) j from users u",
            "select to_jsonb(users) j from users",
            "select u::text j from users u",
            "select u j from users u",
            "select json_agg(x) j from public.users as x",
            "select row_to_json(u.*) j from users u",
            "select row_to_json(s) j from (select email from users) s",
            "select o.id, row_to_json(c) j from orders o join users c on c.id = o.user_id",
        ] {
            assert_eq!(pg(users, sql, &["j"]), [true], "{sql}");
        }
        assert_eq!(pg(users, "select o.id, row_to_json(c) j from orders o join users c on c.id = o.user_id", &["id", "j"]), [false, true]);
        // A whole row of a table without rules stays visible.
        assert_eq!(pg(users, "select row_to_json(o) j from orders o", &["j"]), [false]);
        assert_eq!(pg(&["ssn"], "select row_to_json(p) j from people p", &["j"]), [true], "a table-less rule may be in any row");
        // A rule with a schema masks SQL that leaves the schema out.
        assert_eq!(pg(&["public.users.email"], "select email, name from users", &["email", "name"]), [true, false]);
        assert_eq!(pg(&["public.users.email"], "select u.email e from public.users u", &["e"]), [true]);
        assert_eq!(pg(&["public.users.email"], "select email from sales.users", &["email"]), [false]);
        // A rule without a table masks the column anywhere.
        assert_eq!(pg(&["ssn"], "select ssn as s, id from people", &["s", "id"]), [true, false]);
        // No rules: nothing masked.
        assert_eq!(pg(&[], "select email from users", &["email"]), [false]);
    }

    #[test]
    fn fails_closed_when_the_sql_cannot_be_traced() {
        let users = &["users.email"];
        // Unparseable SQL that names a sensitive column: every column masked.
        assert_eq!(pg(users, "selec email frm users", &["a", "b"]), [true, true]);
        assert_eq!(pg(users, "selec name frm users", &["a"]), [false], "it names no sensitive column");
        // Not SQL (MongoDB): by name, or masked when the query mentions a sensitive column.
        assert_eq!(masked(ConnectorKind::Mongodb, users, "db.users.find({}, {email: 1})", &["_id", "email"]), [true, true]);
        assert_eq!(masked(ConnectorKind::Mongodb, &["ssn"], "db.people.find({})", &["_id", "ssn"]), [false, true]);
    }

    #[test]
    fn table_rules_mask_mongodb_results() {
        // As in the app: the MongoDB shell parser tells which collections a command uses.
        databrain_connector_core::language::set_mongo_analyzer(databrain_connector_mongodb::analyze);
        let users = &["users.email"];
        // The rule names a collection: its column is masked without being named in the query.
        assert_eq!(masked(ConnectorKind::Mongodb, users, "db.users.find({})", &["_id", "email", "name"]), [false, true, false]);
        assert_eq!(masked(ConnectorKind::Mongodb, users, "db.getSiblingDB('crm').getCollection('users').find({})", &["email"]), [true]);
        assert_eq!(masked(ConnectorKind::Mongodb, users, r#"{"find": "users", "filter": {}}"#, &["email"]), [true]);
        assert_eq!(masked(ConnectorKind::Mongodb, users, "db.users.find({})", &["contact.email"]), [true], "nested field");
        // Another collection's column of the same name is not masked.
        assert_eq!(masked(ConnectorKind::Mongodb, users, "db.leads.find({})", &["email"]), [false]);
        assert_eq!(masked(ConnectorKind::Mongodb, users, "db.users.aggregate([{$match: {}}])", &["email"]), [true]);
        // Collections that can't be told: masked by the rule's column name (fail closed).
        assert_eq!(masked(ConnectorKind::Mongodb, users, "db.runCommand({ping: 1})", &["email", "ok"]), [true, false]);
    }

    #[test]
    fn whole_documents_of_mongodb_collections_with_rules_are_masked() {
        databrain_connector_core::language::set_mongo_analyzer(databrain_connector_mongodb::analyze);
        let users = &["users.email"];
        // A document of `users` under another name: one JSON cell holding `email`.
        for cmd in [
            "db.orders.aggregate([{$lookup: {from: 'users', localField: 'uid', foreignField: '_id', as: 'u'}}])",
            "db.users.aggregate([{$project: {x: '$$ROOT'}}])",
            "db.users.aggregate([{$group: {_id: null, all: {$push: '$$ROOT'}}}])",
            "db.orders.aggregate([{$graphLookup: {from: 'users', startWith: '$uid', connectFromField: 'ref', connectToField: '_id', as: 'u'}}])",
        ] {
            assert_eq!(masked(ConnectorKind::Mongodb, users, cmd, &["_id", "u"]), [true, true], "{cmd}");
        }
        // Collections without rules keep their columns.
        assert_eq!(masked(ConnectorKind::Mongodb, users, "db.orders.aggregate([{$project: {x: '$$ROOT'}}])", &["_id", "x"]), [false, false]);
        assert_eq!(
            masked(
                ConnectorKind::Mongodb,
                users,
                "db.orders.aggregate([{$lookup: {from: 'items', localField: 'iid', foreignField: '_id', as: 'i'}}])",
                &["_id", "i"]
            ),
            [false, false]
        );
    }
}
