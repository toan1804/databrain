use sqlparser::ast::{Expr, Ident, JoinOperator, ObjectName, ObjectNamePart, Query, SelectItem, SetExpr, TableAlias, Visit, Visitor};
use std::ops::ControlFlow;

#[derive(Clone)]
pub(crate) struct Source {
    /// Name it is referred to by (alias or table name), lower case.
    pub(crate) alias: String,
    pub(crate) node: usize,
}

#[derive(Clone, Default)]
pub(crate) struct Env {
    /// CTE name (lower case) → node.
    pub(crate) ctes: Vec<(String, usize)>,
    /// Sources of enclosing queries (correlated subqueries, LATERAL).
    pub(crate) outer: Vec<Source>,
}

impl Env {
    pub(crate) fn cte(&self, name: &str) -> Option<usize> {
        self.ctes.iter().rev().find(|(n, _)| n == name).map(|(_, i)| *i)
    }
}

/// Column references, aggregates and subqueries of one expression (not
/// looking inside its subqueries, which are returned to be analyzed alone).
#[derive(Default)]
pub(crate) struct Refs {
    pub(crate) cols: Vec<Vec<Ident>>,
    pub(crate) aggregate: bool,
    pub(crate) window: bool,
    pub(crate) subqueries: Vec<Query>,
    pub(crate) depth: usize,
}

pub(crate) const AGGREGATES: &[&str] = &[
    "count",
    "sum",
    "avg",
    "min",
    "max",
    "array_agg",
    "string_agg",
    "listagg",
    "group_concat",
    "stddev",
    "stddev_pop",
    "stddev_samp",
    "variance",
    "var_pop",
    "var_samp",
    "median",
    "mode",
    "percentile_cont",
    "percentile_disc",
    "approx_count_distinct",
    "count_if",
    "bool_and",
    "bool_or",
    "every",
    "any_value",
    "collect_list",
    "collect_set",
    "json_agg",
    "jsonb_agg",
    "json_object_agg",
    "first",
    "last",
    "arbitrary",
    "approx_distinct",
    "countif",
    "logical_and",
    "logical_or",
    "bit_and",
    "bit_or",
    "corr",
    "covar_pop",
    "covar_samp",
];

/// Bare words that are values, not columns.
pub(crate) const PSEUDO: &[&str] = &[
    "sysdate",
    "systimestamp",
    "current_date",
    "current_timestamp",
    "current_time",
    "current_user",
    "session_user",
    "user",
    "rownum",
    "rowid",
    "level",
    "localtimestamp",
    "localtime",
    "true",
    "false",
    "null",
    "default",
];

impl Visitor for Refs {
    type Break = ();
    fn pre_visit_query(&mut self, q: &Query) -> ControlFlow<()> {
        if self.depth == 0 {
            self.subqueries.push(q.clone());
        }
        self.depth += 1;
        ControlFlow::Continue(())
    }
    fn post_visit_query(&mut self, _q: &Query) -> ControlFlow<()> {
        self.depth -= 1;
        ControlFlow::Continue(())
    }
    fn pre_visit_expr(&mut self, e: &Expr) -> ControlFlow<()> {
        if self.depth > 0 {
            return ControlFlow::Continue(());
        }
        match e {
            Expr::Identifier(i) if i.quote_style.is_some() || !PSEUDO.contains(&i.value.to_lowercase().as_str()) => self.cols.push(vec![i.clone()]),
            Expr::CompoundIdentifier(v) => self.cols.push(v.clone()),
            Expr::Function(f) => {
                if f.over.is_some() {
                    self.window = true;
                } else if let Some(ObjectNamePart::Identifier(n)) = f.name.0.last()
                    && AGGREGATES.contains(&n.value.to_lowercase().as_str())
                {
                    self.aggregate = true;
                }
            }
            _ => {}
        }
        ControlFlow::Continue(())
    }
}

pub(crate) fn refs(e: &Expr) -> Refs {
    let mut r = Refs::default();
    let _ = e.visit(&mut r);
    r
}

pub(crate) fn is_plain(e: &Expr) -> bool {
    match e {
        Expr::Identifier(_) | Expr::CompoundIdentifier(_) => true,
        Expr::Nested(x) => is_plain(x),
        _ => false,
    }
}

pub(crate) fn name_parts(n: &ObjectName) -> Vec<String> {
    n.0.iter()
        .map(|p| match p {
            ObjectNamePart::Identifier(i) => i.value.clone(),
            other => other.to_string(),
        })
        .collect()
}

/// Alias of a table in a statement (lower case), else its last name part.
/// Name a `SELECT` gives an unnamed item (see `Builder::select`).
pub(crate) fn item_name(e: &Expr) -> String {
    match e {
        Expr::Identifier(i) => i.value.clone(),
        Expr::CompoundIdentifier(v) => v.last().map(|i| i.value.clone()).unwrap_or_default(),
        other => short(other.to_string()),
    }
}

/// Output names of a query's first `SELECT`: the anchor of a recursive CTE,
/// whose names the whole CTE takes. `*` items are left out (unknown here).
pub(crate) fn anchor_names(q: &Query) -> Vec<String> {
    let mut body = &*q.body;
    loop {
        match body {
            SetExpr::SetOperation { left, .. } => body = left,
            SetExpr::Query(inner) => body = &inner.body,
            SetExpr::Select(sel) => {
                return sel
                    .projection
                    .iter()
                    .flat_map(|item| match item {
                        SelectItem::UnnamedExpr(e) => vec![item_name(e)],
                        SelectItem::ExprWithAlias { alias, .. } => vec![alias.value.clone()],
                        SelectItem::ExprWithAliases { aliases, .. } => aliases.iter().map(|a| a.value.clone()).collect(),
                        _ => vec![],
                    })
                    .collect();
            }
            _ => return vec![],
        }
    }
}

/// Whether `q` reads a relation named `name` (one part, lower case): a CTE
/// that reads itself is recursive, with or without the RECURSIVE keyword.
pub(crate) fn reads_itself(q: &Query, name: &str) -> bool {
    let mut found = false;
    let _ = sqlparser::ast::visit_relations(q, |r| {
        if r.0.len() == 1 && name_parts(r).first().is_some_and(|p| p.to_lowercase() == name) {
            found = true;
            return ControlFlow::Break(());
        }
        ControlFlow::Continue(())
    });
    found
}

pub(crate) fn alias_or_name(alias: &Option<TableAlias>, name: &ObjectName) -> String {
    alias.as_ref().map(|a| a.name.value.to_lowercase()).unwrap_or_else(|| name_parts(name).last().cloned().unwrap_or_default().to_lowercase())
}

/// `SELECT … INTO t FROM …` (a new table; not MySQL's `INTO @var` / `OUTFILE`):
/// the table and whether it is temporary.
pub(crate) fn select_into(q: &Query) -> Option<(ObjectName, bool)> {
    let SetExpr::Select(s) = &*q.body else { return None };
    let into = s.into.as_ref()?;
    let name = match into.targets.first()? {
        Expr::Identifier(i) if !i.value.starts_with('@') => ObjectName::from(vec![i.clone()]),
        Expr::CompoundIdentifier(v) => ObjectName::from(v.clone()),
        _ => return None,
    };
    Some((name, into.temporary))
}

pub(crate) fn join_keyword(op: &JoinOperator) -> &'static str {
    match op {
        JoinOperator::Left(_) | JoinOperator::LeftOuter(_) => "LEFT JOIN",
        JoinOperator::Right(_) | JoinOperator::RightOuter(_) => "RIGHT JOIN",
        JoinOperator::FullOuter(_) => "FULL JOIN",
        JoinOperator::CrossJoin(_) => "CROSS JOIN",
        JoinOperator::Semi(_) | JoinOperator::LeftSemi(_) | JoinOperator::RightSemi(_) => "SEMI JOIN",
        JoinOperator::Anti(_) | JoinOperator::LeftAnti(_) | JoinOperator::RightAnti(_) => "ANTI JOIN",
        JoinOperator::CrossApply => "CROSS APPLY",
        JoinOperator::OuterApply => "OUTER APPLY",
        JoinOperator::AsOf { .. } => "ASOF JOIN",
        _ => "JOIN",
    }
}

pub(crate) fn line_starts(s: &str) -> Vec<usize> {
    std::iter::once(0).chain(s.match_indices('\n').map(|(i, _)| i + 1)).collect()
}

pub(crate) fn short(s: String) -> String {
    if s.chars().count() <= 160 {
        return s;
    }
    let mut t: String = s.chars().take(157).collect();
    t.push('…');
    t
}
