use databrain_connector_core::ConnectorKind;
use sqlparser::ast::{Expr, Ident, JoinOperator, ObjectName, ObjectNamePart, OneOrManyWithParens, Query, SelectItem, SetExpr, Visit, Visitor};
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
    pub(crate) subqueries: Vec<Query>,
    depth: usize,
    /// Bare words that are values in this dialect (see [`pseudo`]).
    pseudo: &'static [&'static str],
    /// Parameters of the lambdas being visited (`x -> x + 1`), lower case.
    lambda: Vec<Vec<String>>,
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

/// Bare words that are values, not columns, in every dialect.
const VALUES: &[&str] = &[
    "current_date",
    "current_timestamp",
    "current_time",
    "current_user",
    "session_user",
    "current_role",
    "localtimestamp",
    "localtime",
    "true",
    "false",
    "null",
    "default",
];

/// Bare words that are values (pseudo-columns, niladic functions) where the
/// dialect has them; elsewhere they are ordinary column names (`level`,
/// `user` and `rowid` are common ones: SQLite's `rowid` is the row's key).
pub(crate) fn pseudo(kind: ConnectorKind) -> &'static [&'static str] {
    match kind {
        ConnectorKind::Oracle => {
            &["sysdate", "systimestamp", "user", "uid", "rownum", "rowid", "level", "connect_by_isleaf", "connect_by_iscycle", "ora_rowscn"]
        }
        ConnectorKind::Postgres => &["user", "current_catalog", "current_schema"],
        ConnectorKind::Mssql => &["user", "system_user"],
        _ => &[],
    }
}

impl Refs {
    fn is_value(&self, i: &Ident) -> bool {
        let v = i.value.to_lowercase();
        i.quote_style.is_none() && (VALUES.contains(&v.as_str()) || self.pseudo.contains(&v.as_str()))
    }

    fn is_lambda_param(&self, i: &Ident) -> bool {
        let v = i.value.to_lowercase();
        self.lambda.iter().any(|ps| ps.contains(&v))
    }
}

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
            Expr::Identifier(i) if !self.is_value(i) && !self.is_lambda_param(i) => self.cols.push(vec![i.clone()]),
            Expr::CompoundIdentifier(v) if !v.first().is_some_and(|i| self.is_lambda_param(i)) => self.cols.push(v.clone()),
            Expr::Lambda(l) => {
                let names = match &l.params {
                    OneOrManyWithParens::One(p) => vec![p.name.value.to_lowercase()],
                    OneOrManyWithParens::Many(ps) => ps.iter().map(|p| p.name.value.to_lowercase()).collect(),
                };
                self.lambda.push(names);
            }
            Expr::Function(f) => {
                if f.over.is_none()
                    && let Some(ObjectNamePart::Identifier(n)) = f.name.0.last()
                    && AGGREGATES.contains(&n.value.to_lowercase().as_str())
                {
                    self.aggregate = true;
                }
            }
            _ => {}
        }
        ControlFlow::Continue(())
    }
    fn post_visit_expr(&mut self, e: &Expr) -> ControlFlow<()> {
        if self.depth == 0 && matches!(e, Expr::Lambda(_)) {
            self.lambda.pop();
        }
        ControlFlow::Continue(())
    }
}

/// What `e` reads, with the bare words of `kind`'s dialect that are values left out.
pub(crate) fn refs(e: &Expr, kind: ConnectorKind) -> Refs {
    refs_in(e, kind)
}

/// [`refs`] of any part of a statement (a table function's arguments).
pub(crate) fn refs_in<V: Visit + ?Sized>(v: &V, kind: ConnectorKind) -> Refs {
    let mut r = Refs { pseudo: pseudo(kind), ..Default::default() };
    let _ = v.visit(&mut r);
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

/// The parts of a name as identifiers (with their quotes).
pub(crate) fn idents(n: &ObjectName) -> Vec<Ident> {
    n.0.iter()
        .map(|p| match p {
            ObjectNamePart::Identifier(i) => i.clone(),
            other => Ident::new(other.to_string()),
        })
        .collect()
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
