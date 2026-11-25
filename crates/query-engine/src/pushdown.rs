//! Pushdown for combined queries (docs/design/cross-connection-queries.md,
//! §7): what each part's server can be asked for less of, worked out from
//! the outer query. Two rules make it safe by construction:
//!
//! 1. **Wrap, never rewrite.** A part is sent as written inside
//!    `SELECT <its used columns> FROM (<part>) databrain_part WHERE <terms>`
//!    in its own dialect; nothing of the part is edited.
//! 2. **The outer query keeps every term.** DuckDB applies its whole `WHERE`
//!    and `ON` again to what arrives, so a pushed term only has to be **no
//!    stricter** than DuckDB's reading: it may keep rows DuckDB then drops,
//!    never drop one DuckDB would keep. The type rules below are that
//!    guarantee (a text range is never pushed: the server's collation orders
//!    text otherwise and could drop rows).
//!
//! When unsure (an unknown column type, a shape not listed, a part used
//! twice, a dialect that can't take the part as a derived table) nothing is
//! pushed into that part: it runs as written, as in phase 1.

use std::collections::HashMap;

use databrain_connector_core::ConnectorKind;
use databrain_connector_core::arrow::datatypes::DataType;
use databrain_connector_core::quote_ident;
use databrain_connector_core::sql::quote_string;
use databrain_connector_core::traits::RowLimit;
use sqlparser::ast::{
    BinaryOperator, DataType as SqlType, Expr, GroupByExpr, Ident, JoinConstraint, JoinOperator, LimitClause, ObjectNamePart, Query, Select, SelectItem,
    SelectItemQualifiedWildcardKind, SetExpr, Statement, TableFactor, UnaryOperator, Value,
};
use sqlparser::parser::Parser;

/// What a part's column is, for the comparisons that are safe on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TypeClass {
    Integer,
    Decimal,
    Bool,
    Date,
    /// Without a time zone.
    Timestamp,
    Text,
    Float,
    /// Anything else: nothing is pushed on it.
    Other,
}

impl TypeClass {
    /// The class of a column as it arrives (its Arrow type).
    pub(crate) fn of(t: &DataType) -> TypeClass {
        use DataType::*;
        match t {
            Int8 | Int16 | Int32 | Int64 | UInt8 | UInt16 | UInt32 | UInt64 => TypeClass::Integer,
            Decimal128(..) | Decimal256(..) => TypeClass::Decimal,
            Boolean => TypeClass::Bool,
            Date32 | Date64 => TypeClass::Date,
            Timestamp(_, None) => TypeClass::Timestamp,
            Utf8 | LargeUtf8 | Utf8View => TypeClass::Text,
            Float16 | Float32 | Float64 => TypeClass::Float,
            _ => TypeClass::Other,
        }
    }

    fn numeric(self) -> bool {
        matches!(self, TypeClass::Integer | TypeClass::Decimal | TypeClass::Float)
    }

    /// `<>`, `NOT`, `NOT IN`, `NOT BETWEEN`: the server's `=` must be no
    /// looser than DuckDB's, which a case-insensitive collation breaks.
    fn negation_safe(self) -> bool {
        matches!(self, TypeClass::Integer | TypeClass::Decimal | TypeClass::Bool | TypeClass::Date | TypeClass::Timestamp)
    }
}

/// One part, as the planner sees it.
pub(crate) struct PartInput<'a> {
    /// Its CTE's name in the outer query.
    pub name: &'a str,
    pub kind: ConnectorKind,
    /// The part as written (parameters bound).
    pub sql: &'a str,
    /// Its output columns and their classes, when known (else nothing is pushed).
    pub columns: Option<&'a [(String, TypeClass)]>,
}

/// What is sent for a part when something was pushed into it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Pushed {
    /// The wrapped SQL, in the part's dialect.
    pub sent: String,
    /// What was pushed, for the UI and the history ("columns id, total",
    /// "total > 5", "limit 10").
    pub what: Vec<String>,
}

/// The alias around a wrapped part.
const ALIAS: &str = "databrain_part";

/// For each part (in order), what to send instead of it, or `None` to send
/// it as written. `outer` is the outer query as DuckDB runs it (each part's
/// body already a read of its output, the parameters bound).
pub(crate) fn plan(outer: &str, parts: &[PartInput]) -> Vec<Option<Pushed>> {
    let none = || parts.iter().map(|_| None).collect();
    let Ok(mut stmts) = Parser::parse_sql(&sqlparser::dialect::DuckDbDialect {}, outer) else { return none() };
    if stmts.len() != 1 {
        return none();
    }
    let Statement::Query(q) = stmts.remove(0) else { return none() };
    let Some(with) = &q.with else { return none() };
    if with.recursive {
        return none();
    }
    // Each part's CTE: its outer column names (a column list renames them).
    let mut outer_cols: Vec<Option<OuterColumns>> = Vec::with_capacity(parts.len());
    for p in parts {
        let cte = with.cte_tables.iter().find(|c| same_name(&c.alias.name, p.name));
        let cols = match (cte, p.columns) {
            (Some(cte), Some(cols)) if unique(cols) => {
                if cte.alias.columns.is_empty() {
                    Some(cols.iter().map(|(n, t)| (n.clone(), n.clone(), *t)).collect())
                } else if cte.alias.columns.len() == cols.len() {
                    Some(cte.alias.columns.iter().zip(cols).map(|(a, (n, t))| (a.name.value.clone(), n.clone(), *t)).collect())
                } else {
                    None
                }
            }
            _ => None,
        };
        outer_cols.push(cols);
    }
    let SetExpr::Select(sel) = q.body.as_ref() else { return none() };
    // A part used anywhere but once in the main FROM (another CTE, a
    // subquery, twice): nothing is pushed into it.
    let mut uses: HashMap<usize, usize> = HashMap::new();
    let _ = sqlparser::ast::visit_relations(&q, |name| {
        if let [ObjectNamePart::Identifier(id)] = name.0.as_slice()
            && let Some(i) = parts.iter().position(|p| same_name(id, p.name))
        {
            *uses.entry(i).or_default() += 1;
        }
        std::ops::ControlFlow::<()>::Continue(())
    });
    let rels = relations(sel, parts);
    let in_from = |i: usize| rels.iter().filter(|r| r.part == Some(i)).count();
    let pushable: Vec<bool> = (0..parts.len()).map(|i| outer_cols[i].is_some() && uses.get(&i) == Some(&1) && in_from(i) == 1).collect();
    let ctx = Ctx { rels: &rels, cols: &outer_cols };

    // Columns each part must still send: all of them unless every use of
    // its columns is known.
    let mut used: Vec<Option<Vec<usize>>> = (0..parts.len()).map(|_| Some(Vec::new())).collect();
    for item in &sel.projection {
        match item {
            SelectItem::Wildcard(_) => used.iter_mut().for_each(|u| *u = None),
            SelectItem::QualifiedWildcard(SelectItemQualifiedWildcardKind::ObjectName(n), _) => match n.0.as_slice() {
                [ObjectNamePart::Identifier(a)] => {
                    if let Some(r) = rels.iter().find(|r| same_name(a, &r.alias))
                        && let Some(i) = r.part
                    {
                        used[i] = None;
                    }
                }
                _ => used.iter_mut().for_each(|u| *u = None),
            },
            SelectItem::QualifiedWildcard(..) => used.iter_mut().for_each(|u| *u = None),
            _ => {}
        }
    }
    let _ = sqlparser::ast::visit_expressions(&q, |e| {
        let path: Option<Vec<&Ident>> = match e {
            Expr::Identifier(i) => Some(vec![i]),
            Expr::CompoundIdentifier(v) => Some(v.iter().collect()),
            _ => None,
        };
        if let Some(path) = path {
            for (i, c) in ctx.uses_of(&path) {
                if let Some(u) = used[i].as_mut()
                    && !u.contains(&c)
                {
                    u.push(c);
                }
            }
        }
        std::ops::ControlFlow::<()>::Continue(())
    });

    // Terms: the top-level AND terms of WHERE and of inner joins' ON.
    let mut terms: Vec<&Expr> = Vec::new();
    if let Some(w) = &sel.selection {
        and_terms(w, &mut terms);
    }
    let where_terms = terms.len();
    for twj in &sel.from {
        for j in &twj.joins {
            if let JoinOperator::Join(JoinConstraint::On(e)) | JoinOperator::Inner(JoinConstraint::On(e)) = &j.join_operator {
                and_terms(e, &mut terms);
            }
        }
    }
    let mut filters: Vec<Vec<String>> = vec![Vec::new(); parts.len()];
    let mut where_pushed = 0;
    for (k, t) in terms.iter().enumerate() {
        if let Some((i, text)) = ctx.term(t, parts)
            && pushable[i]
            && !rels.iter().any(|r| r.part == Some(i) && r.nullable)
        {
            filters[i].push(text);
            if k < where_terms {
                where_pushed += 1;
            }
        }
    }
    // The row limit: `SELECT cols FROM one_part [WHERE pushed] LIMIT n` and nothing else.
    let limit =
        (rels.len() == 1 && where_pushed == where_terms && plain_select(sel) && q.order_by.is_none() && q.fetch.is_none()).then(|| plain_limit(&q)).flatten();

    parts
        .iter()
        .enumerate()
        .map(|(i, p)| {
            if !pushable[i] {
                return None;
            }
            let all = outer_cols[i].as_ref()?;
            let mut what = Vec::new();
            let cols: Option<Vec<String>> = used[i].as_ref().filter(|u| u.len() < all.len()).map(|u| {
                let mut u = u.clone();
                u.sort();
                // At least one column: `SELECT count(*) FROM part` uses none.
                if u.is_empty() {
                    u.push(0);
                }
                u.iter().map(|&c| all[c].1.clone()).collect()
            });
            if let Some(c) = &cols {
                what.push(format!("columns {}", c.join(", ")));
            }
            what.extend(filters[i].iter().cloned());
            let n = limit.filter(|_| rels[0].part == Some(i));
            if let Some(n) = n {
                what.push(format!("limit {n}"));
            }
            if what.is_empty() {
                return None;
            }
            let sent = wrap(p, cols.as_deref(), &filters[i], n)?;
            Some(Pushed { sent, what })
        })
        .collect()
}

/// A part's columns as the outer query sees them: (outer name, the part's
/// own name, class).
type OuterColumns = Vec<(String, String, TypeClass)>;

/// A relation of the main FROM: its alias, the part it is (if one), and
/// whether an outer join can make its columns NULL.
struct Rel {
    alias: String,
    part: Option<usize>,
    nullable: bool,
}

struct Ctx<'a> {
    rels: &'a [Rel],
    /// Per part: (outer name, the part's own name, class).
    cols: &'a [Option<OuterColumns>],
}

impl Ctx<'_> {
    /// The parts' columns an identifier may be: `p.col` is the part's (when
    /// `p` is one), `col` alone is every part's that has one so named.
    fn uses_of(&self, path: &[&Ident]) -> Vec<(usize, usize)> {
        let find = |i: usize, c: &Ident| self.cols[i].as_ref().and_then(|cols| cols.iter().position(|(n, _, _)| same_name(c, n)));
        match path {
            [c] => (0..self.cols.len()).filter_map(|i| find(i, c).map(|k| (i, k))).collect(),
            [a, c] => self.rels.iter().filter(|r| same_name(a, &r.alias)).filter_map(|r| r.part).filter_map(|i| find(i, c).map(|k| (i, k))).collect(),
            _ => Vec::new(),
        }
    }

    /// The one part column `e` is, for a filter: `p.col`, or `col` when the
    /// main FROM has only that part.
    fn column(&self, e: &Expr) -> Option<(usize, usize)> {
        let path: Vec<&Ident> = match e {
            Expr::Identifier(i) => vec![i],
            Expr::CompoundIdentifier(v) => v.iter().collect(),
            Expr::Nested(e) => return self.column(e),
            _ => return None,
        };
        match path.as_slice() {
            [_] if self.rels.len() != 1 => None,
            [c] => {
                let i = self.rels[0].part?;
                let k = self.cols[i].as_ref()?.iter().position(|(n, _, _)| same_name(c, n))?;
                Some((i, k))
            }
            [_, _] => match self.uses_of(&path).as_slice() {
                [one] => Some(*one),
                _ => None,
            },
            _ => None,
        }
    }

    /// A term that reads one part's columns only, as that part's dialect
    /// writes it, when its shape and types allow (see the module's rules).
    fn term(&self, e: &Expr, parts: &[PartInput]) -> Option<(usize, String)> {
        let mut part = None;
        let text = self.render(e, parts, &mut part, false)?;
        Some((part?, text))
    }

    fn render(&self, e: &Expr, parts: &[PartInput], part: &mut Option<usize>, negated: bool) -> Option<String> {
        // The column of a comparison, all in one part.
        let mut col = |c: &Expr| -> Option<(usize, String, TypeClass)> {
            let (i, k) = self.column(c)?;
            if part.is_some_and(|p| p != i) {
                return None;
            }
            *part = Some(i);
            let (_, own, t) = &self.cols[i].as_ref()?[k];
            Some((i, quote_ident(parts[i].kind, own), *t))
        };
        match e {
            Expr::Nested(inner) => Some(format!("({})", self.render(inner, parts, part, negated)?)),
            Expr::BinaryOp { left, op: BinaryOperator::Or, right } => {
                Some(format!("({} OR {})", self.render(left, parts, part, negated)?, self.render(right, parts, part, negated)?))
            }
            Expr::BinaryOp { left, op: BinaryOperator::And, right } => {
                Some(format!("({} AND {})", self.render(left, parts, part, negated)?, self.render(right, parts, part, negated)?))
            }
            Expr::UnaryOp { op: UnaryOperator::Not, expr } => Some(format!("NOT ({})", self.render(expr, parts, part, true)?)),
            Expr::BinaryOp { left, op, right } => {
                let (c, v, op) = if self.column(left).is_some() { (left, right, op.clone()) } else { (right, left, flip(op)?) };
                let (i, name, t) = col(c)?;
                let sym = match op {
                    BinaryOperator::Eq => "=",
                    BinaryOperator::NotEq => "<>",
                    BinaryOperator::Lt => "<",
                    BinaryOperator::LtEq => "<=",
                    BinaryOperator::Gt => ">",
                    BinaryOperator::GtEq => ">=",
                    _ => return None,
                };
                let range = !matches!(sym, "=" | "<>");
                let ok = match t {
                    TypeClass::Integer | TypeClass::Decimal | TypeClass::Bool | TypeClass::Date | TypeClass::Timestamp => true,
                    TypeClass::Text => sym == "=",
                    TypeClass::Float => range,
                    TypeClass::Other => false,
                } && (sym != "<>" && !negated || t.negation_safe());
                if !ok {
                    return None;
                }
                Some(format!("{name} {sym} {}", literal(v, t, parts[i].kind)?))
            }
            Expr::InList { expr, list, negated: not_in } => {
                let (i, name, t) = col(expr)?;
                let neg = *not_in || negated;
                let ok = match t {
                    TypeClass::Text => !neg,
                    TypeClass::Float | TypeClass::Other => false,
                    _ => !neg || t.negation_safe(),
                };
                if !ok || list.is_empty() {
                    return None;
                }
                let vals: Option<Vec<String>> = list.iter().map(|v| literal(v, t, parts[i].kind)).collect();
                Some(format!("{name} {}IN ({})", if *not_in { "NOT " } else { "" }, vals?.join(", ")))
            }
            Expr::Between { expr, negated: not_between, low, high } => {
                let (i, name, t) = col(expr)?;
                let neg = *not_between || negated;
                let ok = match t {
                    TypeClass::Integer | TypeClass::Decimal | TypeClass::Date | TypeClass::Timestamp => true,
                    TypeClass::Float => !neg,
                    _ => false,
                };
                if !ok {
                    return None;
                }
                let k = parts[i].kind;
                Some(format!("{name} {}BETWEEN {} AND {}", if *not_between { "NOT " } else { "" }, literal(low, t, k)?, literal(high, t, k)?))
            }
            Expr::IsNull(c) | Expr::IsNotNull(c) => {
                let (_, name, t) = col(c)?;
                if t == TypeClass::Other || negated && !t.negation_safe() {
                    return None;
                }
                Some(format!("{name} IS {}NULL", if matches!(e, Expr::IsNotNull(_)) { "NOT " } else { "" }))
            }
            _ => None,
        }
    }
}

/// `5 < x` as `x > 5`.
fn flip(op: &BinaryOperator) -> Option<BinaryOperator> {
    Some(match op {
        BinaryOperator::Eq => BinaryOperator::Eq,
        BinaryOperator::NotEq => BinaryOperator::NotEq,
        BinaryOperator::Lt => BinaryOperator::Gt,
        BinaryOperator::LtEq => BinaryOperator::GtEq,
        BinaryOperator::Gt => BinaryOperator::Lt,
        BinaryOperator::GtEq => BinaryOperator::LtEq,
        _ => return None,
    })
}

/// A literal compared with a column of class `t`, rebuilt for `kind` from
/// its parsed value (never the text as typed); `None` when it isn't a
/// literal of that column's kind or the dialect has no form for it.
fn literal(v: &Expr, t: TypeClass, kind: ConnectorKind) -> Option<String> {
    match (v, t) {
        (Expr::UnaryOp { op: UnaryOperator::Minus, expr }, _) if t.numeric() => Some(format!("-{}", literal(expr, t, kind)?)),
        (Expr::Value(v), _) => match (&v.value, t) {
            (Value::Number(n, _), _) if t.numeric() && is_number(n) => Some(n.clone()),
            (Value::Boolean(b), TypeClass::Bool) => match kind {
                ConnectorKind::Mssql | ConnectorKind::Oracle => None,
                _ => Some(if *b { "TRUE" } else { "FALSE" }.into()),
            },
            (Value::SingleQuotedString(s), TypeClass::Text) => Some(quote_string(s, kind)),
            (Value::SingleQuotedString(s), TypeClass::Date | TypeClass::Timestamp) => typed(s, t, kind),
            _ => None,
        },
        (Expr::TypedString(ts), TypeClass::Date | TypeClass::Timestamp) => {
            let same = match (&ts.data_type, t) {
                (SqlType::Date, TypeClass::Date) => true,
                (SqlType::Timestamp(_, tz), TypeClass::Timestamp) => {
                    matches!(tz, sqlparser::ast::TimezoneInfo::None | sqlparser::ast::TimezoneInfo::WithoutTimeZone)
                }
                _ => false,
            };
            let Value::SingleQuotedString(s) = &ts.value.value else { return None };
            same.then(|| typed(s, t, kind)).flatten()
        }
        _ => None,
    }
}

/// A date or timestamp literal in `kind`'s typed form, for text that is one
/// (`2024-01-31`, `2024-01-31 10:00:00[.123]`).
fn typed(s: &str, t: TypeClass, kind: ConnectorKind) -> Option<String> {
    let b = s.as_bytes();
    let date = b.len() >= 10 && b[..10].iter().enumerate().all(|(i, c)| if i == 4 || i == 7 { *c == b'-' } else { c.is_ascii_digit() });
    let ok = match t {
        TypeClass::Date => date && b.len() == 10,
        _ => {
            date && (b.len() == 10 || (b.len() >= 19 && matches!(b[10], b' ' | b'T') && b[11..].iter().all(|c| c.is_ascii_digit() || matches!(c, b':' | b'.'))))
        }
    };
    if !ok {
        return None;
    }
    let s = s.replace('T', " ");
    use ConnectorKind::*;
    Some(match (t, kind) {
        (TypeClass::Date, Postgres | Duckdb | Mysql | Oracle | Snowflake | Databricks | Bigquery) => format!("DATE '{s}'"),
        (TypeClass::Date, Mssql) => format!("CAST('{s}' AS date)"),
        (TypeClass::Timestamp, Postgres | Duckdb | Mysql | Oracle | Snowflake | Databricks) => format!("TIMESTAMP '{s}'"),
        (TypeClass::Timestamp, Bigquery) => format!("DATETIME '{s}'"),
        (TypeClass::Timestamp, Mssql) => format!("CAST('{s}' AS datetime2)"),
        _ => return None,
    })
}

fn is_number(n: &str) -> bool {
    !n.is_empty()
        && n.chars().all(|c| c.is_ascii_digit() || matches!(c, '.' | 'e' | 'E' | '+' | '-'))
        && n.starts_with(|c: char| c.is_ascii_digit() || c == '.')
}

/// The part's SQL wrapped: its used columns, the terms, the limit; `None`
/// when its dialect can't take it as a derived table.
fn wrap(p: &PartInput, cols: Option<&[String]>, terms: &[String], limit: Option<u64>) -> Option<String> {
    let sql = p.sql.trim().trim_end_matches(';').trim_end();
    if !wrappable(sql, p.kind) {
        return None;
    }
    let list = cols.map_or_else(|| "*".to_string(), |c| c.iter().map(|n| quote_ident(p.kind, n)).collect::<Vec<_>>().join(", "));
    let row_limit = p.kind.traits().row_limit;
    let top = match (limit, row_limit) {
        (Some(n), RowLimit::Top) => format!("TOP ({n}) "),
        _ => String::new(),
    };
    let mut out = format!("SELECT {top}{list} FROM (\n{sql}\n) {ALIAS}");
    if !terms.is_empty() {
        out.push_str("\nWHERE ");
        out.push_str(&terms.join("\n  AND "));
    }
    match (limit, row_limit) {
        (Some(n), RowLimit::Limit) => out.push_str(&format!("\nLIMIT {n}")),
        (Some(n), RowLimit::FetchFirst) => out.push_str(&format!("\nFETCH FIRST {n} ROWS ONLY")),
        _ => {}
    }
    Some(out)
}

/// A query that returns `sql`'s columns and no row (its types for
/// [`plan`]), or `None` when it can't be wrapped.
pub(crate) fn describe_sql(sql: &str, kind: ConnectorKind) -> Option<String> {
    let sql = sql.trim().trim_end_matches(';').trim_end();
    wrappable(sql, kind).then(|| format!("SELECT * FROM (\n{sql}\n) {ALIAS} WHERE 1 = 0"))
}

/// Whether `kind` takes `sql` as a derived table: one plain query its
/// parser reads (when unsure: no); SQL Server refuses one starting with
/// `WITH`, or with `ORDER BY` without `TOP`.
fn wrappable(sql: &str, kind: ConnectorKind) -> bool {
    let Ok(stmts) = Parser::parse_sql(databrain_connector_core::sql::dialect_for(kind).as_ref(), sql) else { return false };
    let [Statement::Query(q)] = stmts.as_slice() else { return false };
    if q.for_clause.is_some() || !q.locks.is_empty() {
        return false;
    }
    if kind == ConnectorKind::Mssql {
        let top = matches!(q.body.as_ref(), SetExpr::Select(s) if s.top.is_some());
        if q.with.is_some() || (q.order_by.is_some() && !top) {
            return false;
        }
    }
    true
}

/// The main FROM's relations; a part is one whose name is a part's CTE.
fn relations(sel: &Select, parts: &[PartInput]) -> Vec<Rel> {
    let rel = |f: &TableFactor| -> Rel {
        match f {
            TableFactor::Table { name, alias, .. } => {
                let base = match name.0.as_slice() {
                    [ObjectNamePart::Identifier(id)] => Some(id),
                    _ => None,
                };
                Rel {
                    alias: alias.as_ref().map(|a| a.name.value.clone()).or_else(|| base.map(|b| b.value.clone())).unwrap_or_default(),
                    // A table alias with column names renames the part's columns: not read here.
                    part: base.filter(|_| alias.as_ref().is_none_or(|a| a.columns.is_empty())).and_then(|b| parts.iter().position(|p| same_name(b, p.name))),
                    nullable: false,
                }
            }
            TableFactor::Derived { alias, .. } => Rel { alias: alias.as_ref().map(|a| a.name.value.clone()).unwrap_or_default(), part: None, nullable: false },
            _ => Rel { alias: String::new(), part: None, nullable: false },
        }
    };
    let mut out: Vec<Rel> = Vec::new();
    for twj in &sel.from {
        out.push(rel(&twj.relation));
        for j in &twj.joins {
            let mut r = rel(&j.relation);
            match &j.join_operator {
                JoinOperator::Left(_) | JoinOperator::LeftOuter(_) => r.nullable = true,
                JoinOperator::Right(_) | JoinOperator::RightOuter(_) => out.iter_mut().for_each(|x| x.nullable = true),
                JoinOperator::FullOuter(_) => {
                    r.nullable = true;
                    out.iter_mut().for_each(|x| x.nullable = true);
                }
                JoinOperator::Join(_) | JoinOperator::Inner(_) | JoinOperator::CrossJoin(_) => {}
                // Anything else (semi, anti, APPLY, ASOF): nothing pushed there.
                _ => r.nullable = true,
            }
            out.push(r);
        }
    }
    out
}

fn and_terms<'a>(e: &'a Expr, out: &mut Vec<&'a Expr>) {
    match e {
        Expr::BinaryOp { left, op: BinaryOperator::And, right } => {
            and_terms(left, out);
            and_terms(right, out);
        }
        Expr::Nested(inner) if matches!(inner.as_ref(), Expr::BinaryOp { op: BinaryOperator::And, .. }) => and_terms(inner, out),
        e => out.push(e),
    }
}

/// No DISTINCT, GROUP BY, HAVING, QUALIFY, window, function or subquery in
/// the select list: a limit on the part is then a limit on the result.
fn plain_select(sel: &Select) -> bool {
    let grouped = match &sel.group_by {
        GroupByExpr::All(_) => true,
        GroupByExpr::Expressions(e, _) => !e.is_empty(),
    };
    if sel.distinct.is_some() || grouped || sel.having.is_some() || sel.qualify.is_some() || !sel.named_window.is_empty() || sel.top.is_some() {
        return false;
    }
    let mut plain = true;
    let _ = sqlparser::ast::visit_expressions(&sel.projection, |e| {
        if matches!(e, Expr::Function(_) | Expr::Subquery(_) | Expr::Exists { .. } | Expr::InSubquery { .. }) {
            plain = false;
        }
        std::ops::ControlFlow::<()>::Continue(())
    });
    plain
}

/// `LIMIT n` with no offset.
fn plain_limit(q: &Query) -> Option<u64> {
    match &q.limit_clause {
        Some(LimitClause::LimitOffset { limit: Some(Expr::Value(v)), offset: None, limit_by }) if limit_by.is_empty() => match &v.value {
            Value::Number(n, _) => n.parse().ok(),
            _ => None,
        },
        _ => None,
    }
}

/// An identifier as DuckDB compares it with a name: quoted exactly, else
/// in any case.
fn same_name(id: &Ident, name: &str) -> bool {
    if id.quote_style.is_some() { id.value == name } else { id.value.eq_ignore_ascii_case(name) }
}

fn unique(cols: &[(String, TypeClass)]) -> bool {
    let mut seen = std::collections::HashSet::new();
    cols.iter().all(|(n, _)| seen.insert(n.to_lowercase()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ConnectorKind::*;
    use TypeClass::*;

    fn cols(c: &[(&str, TypeClass)]) -> Vec<(String, TypeClass)> {
        c.iter().map(|(n, t)| (n.to_string(), *t)).collect()
    }

    /// The outer query reads each part as `(SELECT * FROM results.rN)`.
    type Part<'a> = (&'a str, ConnectorKind, &'a str, Option<&'a [(String, TypeClass)]>);

    fn run(outer: &str, parts: &[Part]) -> Vec<Option<Pushed>> {
        let inputs: Vec<PartInput> = parts.iter().map(|(name, kind, sql, columns)| PartInput { name, kind: *kind, sql, columns: *columns }).collect();
        plan(outer, &inputs)
    }

    fn orders() -> Vec<(String, TypeClass)> {
        cols(&[("id", Integer), ("total", Decimal), ("note", Text), ("day", Date), ("score", Float), ("ok", Bool)])
    }

    #[test]
    fn columns_and_filters_go_into_the_part() {
        let o = orders();
        let p = run(
            "with o as (select * from results.r1) select o.id from o where o.total > 5 and note = 'it''s' and day >= '2024-01-31'",
            &[("o", Postgres, "select * from orders", Some(&o))],
        );
        let p = p[0].as_ref().unwrap();
        assert_eq!(
            p.sent,
            "SELECT \"id\", \"total\", \"note\", \"day\" FROM (\nselect * from orders\n) databrain_part\nWHERE \"total\" > 5\n  AND \"note\" = 'it''s'\n  AND \"day\" >= DATE '2024-01-31'"
        );
        assert_eq!(p.what[0], "columns id, total, note, day");
    }

    #[test]
    fn only_comparisons_that_are_no_stricter_than_duckdb() {
        let o = orders();
        let one = |w: &str| {
            run(&format!("with o as (select * from results.r1) select * from o where {w}"), &[("o", Postgres, "select * from orders", Some(&o))])[0].clone()
        };
        // Pushed.
        for w in [
            "id = 3",
            "3 < id",
            "id <> 3",
            "id in (1, 2)",
            "id not in (1, 2)",
            "total between 1 and -2.5",
            "note in ('a', 'b')",
            "note is null",
            "score > 1.5",
            "ok = true",
            "not (id = 1)",
            "(id = 1 or total > 2)",
            "day is not null",
            "day > date '2024-01-01'",
        ] {
            assert!(one(w).is_some(), "{w}");
        }
        // Not: text ranges and negations (collations), float equality, functions, LIKE,
        // two columns, a text compared with a number, a malformed date, NOT on text.
        for w in [
            "note > 'a'",
            "note <> 'a'",
            "note not in ('a')",
            "not (note = 'a')",
            "score = 1.5",
            "lower(note) = 'a'",
            "note like 'a%'",
            "id = total",
            "note = 5",
            "day > '2024-1-1'",
            "(id = 1 or note > 'a')",
            "id = (select 1)",
        ] {
            assert!(one(w).is_none_or(|p| !p.what.iter().any(|x| !x.starts_with("columns"))), "{w}: {:?}", one(w));
        }
    }

    #[test]
    fn joins_push_into_each_side_but_never_join_keys_or_into_a_nullable_side() {
        let o = orders();
        let c = cols(&[("id", Integer), ("name", Text), ("country", Text)]);
        let parts = [("o", Postgres, "select * from orders", Some(o.as_slice())), ("c", Mysql, "select * from customers", Some(c.as_slice()))];
        let p = run(
            "with o as (select * from results.r1), c as (select * from results.r2) select c.name, o.total from o join c on c.id = o.id and c.country = 'VN' where o.total > 10",
            &parts,
        );
        assert!(p[0].as_ref().unwrap().sent.ends_with("WHERE \"total\" > 10"), "{:?}", p[0]);
        assert!(p[1].as_ref().unwrap().sent.ends_with("WHERE `country` = 'VN'"), "{:?}", p[1]);
        assert!(!p[1].as_ref().unwrap().sent.contains("`id` ="), "the join key stays in DuckDB");
        // c is the nullable side: its column list still shrinks, its filter isn't sent.
        let p = run(
            "with o as (select * from results.r1), c as (select * from results.r2) select o.id, c.name from o left join c on c.id = o.id where c.name is null",
            &parts,
        );
        assert!(p[1].as_ref().is_none_or(|p| !p.sent.contains("WHERE")), "{:?}", p[1]);
        assert!(p[0].as_ref().unwrap().sent.starts_with("SELECT \"id\" FROM"));
        // An unqualified name with two parts in FROM: used by both, filtered by none.
        let p = run("with o as (select * from results.r1), c as (select * from results.r2) select name from o join c on c.id = o.id where id > 3", &parts);
        assert!(p.iter().all(|p| p.as_ref().is_none_or(|p| !p.sent.contains("WHERE"))), "{p:?}");
    }

    #[test]
    fn the_row_limit_only_for_one_plain_part() {
        let o = orders();
        let one = |q: &str, kind| run(q, &[("o", kind, "select * from orders", Some(&o))])[0].clone();
        assert!(
            one("with o as (select * from results.r1) select id from o where id > 1 limit 10", Postgres).unwrap().sent.ends_with("WHERE \"id\" > 1\nLIMIT 10")
        );
        assert!(one("with o as (select * from results.r1) select id from o limit 10", Mssql).unwrap().sent.starts_with("SELECT TOP (10) [id] FROM"));
        assert!(one("with o as (select * from results.r1) select id from o limit 10", Oracle).unwrap().sent.ends_with("FETCH FIRST 10 ROWS ONLY"));
        for q in [
            "with o as (select * from results.r1) select id from o order by id limit 10",
            "with o as (select * from results.r1) select count(*) from o limit 10",
            "with o as (select * from results.r1) select distinct id from o limit 10",
            "with o as (select * from results.r1) select id from o where lower(note) = 'a' limit 10",
            "with o as (select * from results.r1) select id from o limit 10 offset 5",
        ] {
            assert!(one(q, Postgres).is_none_or(|p| !p.what.iter().any(|w| w.starts_with("limit"))), "{q}");
        }
    }

    #[test]
    fn nothing_is_pushed_when_unsure() {
        let o = orders();
        let q = "with o as (select * from results.r1) select id from o where id = 1";
        // Unknown columns, two columns of one name, a part read twice or elsewhere.
        assert_eq!(run(q, &[("o", Postgres, "select * from orders", None)]), vec![None]);
        let dup = cols(&[("id", Integer), ("ID", Integer)]);
        assert_eq!(run(q, &[("o", Postgres, "select * from orders", Some(&dup))]), vec![None]);
        assert_eq!(
            run("with o as (select * from results.r1) select a.id from o a join o b on a.id = b.id where a.id = 1", &[("o", Postgres, "select 1", Some(&o))]),
            vec![None]
        );
        assert_eq!(
            run("with o as (select * from results.r1), x as (select * from o) select id from o where id = 1", &[("o", Postgres, "select 1", Some(&o))]),
            vec![None]
        );
        // A dialect that can't take it as a derived table, or text its parser doesn't read.
        assert_eq!(run(q, &[("o", Mssql, "with t as (select 1 as id) select * from t", Some(&o))]), vec![None]);
        assert_eq!(run(q, &[("o", Mssql, "select id from t order by id", Some(&o))]), vec![None]);
        assert_eq!(run(q, &[("o", Postgres, "selec id fro t", Some(&o))]), vec![None]);
        // Everything is used: nothing to push.
        assert_eq!(run("with o as (select * from results.r1) select * from o", &[("o", Postgres, "select * from orders", Some(&o))]), vec![None]);
        // A boolean where the dialect has no boolean literal.
        assert!(run("with o as (select * from results.r1) select * from o where ok = true", &[("o", Mssql, "select * from orders", Some(&o))])[0].is_none());
    }

    #[test]
    fn a_column_list_renames_the_parts_columns() {
        let c = cols(&[("id", Integer), ("name", Text)]);
        let p = run(
            "with c(cid, who) as (select * from results.r2) select who from c where cid = 7",
            &[("c", Postgres, "select id, name from customers", Some(&c))],
        );
        // Both are used (`who`, and `cid` in WHERE): every column, filtered on the part's own name.
        assert_eq!(p[0].as_ref().unwrap().sent, "SELECT * FROM (\nselect id, name from customers\n) databrain_part\nWHERE \"id\" = 7");
        let p = run("with c(cid, who) as (select * from results.r2) select who from c", &[("c", Postgres, "select id, name from customers", Some(&c))]);
        assert_eq!(p[0].as_ref().unwrap().what, vec!["columns name"]);
    }

    #[test]
    fn literals_are_rebuilt_in_the_parts_dialect() {
        let o = orders();
        let one = |kind| {
            run(
                "with o as (select * from results.r1) select id from o where note = 'a''b' and day = date '2024-02-29'",
                &[("o", kind, "select * from orders", Some(&o))],
            )[0]
            .clone()
            .unwrap()
            .sent
        };
        assert!(one(Mysql).contains("`note` = 'a\\'b'") || one(Mysql).contains("`note` = 'a''b'"), "{}", one(Mysql));
        assert!(one(Mssql).contains("[day] = CAST('2024-02-29' AS date)"), "{}", one(Mssql));
        assert!(one(Postgres).contains("\"day\" = DATE '2024-02-29'"));
    }
}
