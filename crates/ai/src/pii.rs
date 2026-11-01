//! Which columns of a result hold PII, found through the SQL that made it:
//! a masked column stays masked under an alias (`email AS e`), inside an
//! expression (`lower(email)`, `max(email)`) and through `SELECT *`.

use std::collections::{HashMap, HashSet};

use databrain_connector_core::ConnectorKind;
use databrain_lineage::{EdgeKind, Lineage, NodeKind, NoCatalog, analyze};

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
        return columns.iter().map(|c| is_pii(None, c) || mentions).collect();
    }
    let g = analyze(sql, kind, &NoCatalog);
    let tables: Vec<&str> = g.nodes.iter().filter(|n| n.kind == NodeKind::Table).map(|n| n.name.as_str()).collect();
    let result = g.nodes.iter().rev().find(|n| n.kind == NodeKind::Result);
    columns
        .iter()
        .map(|c| {
            // By name: the column as returned, from any table the SQL reads.
            if is_pii(None, c) || tables.iter().any(|t| is_pii(Some(t), c)) {
                return true;
            }
            let Some(r) = result.filter(|_| !g.partial) else { return mentions };
            let sources = sources(&g, &r.id, c);
            if sources.is_empty() {
                // Not traced (an expression the lineage can't place): masked
                // when the SQL names a sensitive column at all.
                return mentions;
            }
            sources.iter().any(|(t, col)| if col == "*" { is_pii(Some(t), c) } else { is_pii(Some(t), col) || is_pii(None, col) })
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
                Some(n) if n.kind == NodeKind::Table => out.push((n.name.clone(), if e.from_column.as_deref().is_none_or(|c| c.ends_with('*')) { "*".into() } else { from_col })),
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
