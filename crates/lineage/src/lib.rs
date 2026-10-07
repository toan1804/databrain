//! Column-level lineage of SQL scripts.
//!
//! Each statement is parsed (sqlparser, per dialect) and every output column
//! is traced through CTEs, subqueries, joins and set operations to the source
//! columns it comes from. `CREATE TABLE/VIEW … AS`, `INSERT … SELECT` link the
//! statements of a script: a table written by one statement is the same node
//! that a later statement reads.
//!
//! Table columns (for `*` and unqualified names) come from a [`Catalog`]
//! (DataBrain's explorer cache); names that can't be resolved are reported
//! as warnings, never guessed. Every node, column and edge carries the byte
//! range of the SQL it came from, so the UI can highlight it.

use std::collections::HashMap;
use std::ops::ControlFlow;

use databrain_connector_core::ConnectorKind;
use databrain_connector_core::sql::{dialect_for, split_statements};
use serde::Serialize;
use sqlparser::ast::{
    Expr, Ident, JoinConstraint, JoinOperator, ObjectName, ObjectNamePart, Query, Select, SelectItem, SelectItemQualifiedWildcardKind, SetExpr, Spanned,
    Statement, TableAlias, TableFactor, TableObject, TableWithJoins, Visit, Visitor,
};
use sqlparser::parser::Parser;
use sqlparser::tokenizer::{Location, Span, Token, Tokenizer};

// ------------------------------------------------------------------ model

/// Byte range in the script.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Range {
    pub start: usize,
    pub end: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeKind {
    /// A table or view in the database (read, or written by the script).
    Table,
    Cte,
    Subquery,
    /// `UNION` / `INTERSECT` / `EXCEPT`.
    SetOp,
    /// The rows a `SELECT` statement returns.
    Result,
}

#[derive(Debug, Clone, Serialize)]
pub struct Column {
    pub name: String,
    /// Where the column is defined (alias or expression).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub span: Option<Range>,
    /// The expression of a computed column (shortened).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expr: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Node {
    pub id: String,
    pub kind: NodeKind,
    pub name: String,
    pub columns: Vec<Column>,
    /// Table columns came from the catalog (else only the referenced ones are listed).
    pub columns_known: bool,
    /// Written by the script (`CREATE TABLE … AS`, `INSERT`).
    pub written: bool,
    /// Statement that defines it (tables: none).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub statement: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub span: Option<Range>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeKind {
    /// The value is copied as is.
    Direct,
    /// Computed from it (`upper(a)`, `a * b`, `case …`, window functions).
    Transform,
    /// Aggregated (`sum`, `count`, …).
    Aggregate,
    /// Decides which rows are kept (`WHERE`, `HAVING`, `QUALIFY`).
    Filter,
    /// Join condition.
    Join,
}

/// `from` column (or the whole node) feeds `to` column (or, for filters and
/// joins, the rows of the `to` node).
#[derive(Debug, Clone, Serialize)]
pub struct Edge {
    pub from: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from_column: Option<String>,
    pub to: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to_column: Option<String>,
    pub kind: EdgeKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub span: Option<Range>,
    /// Filters and joins: the clause it comes from (`LEFT JOIN customers c ON c.id = o.customer_id`,
    /// `WHERE o.status = 'paid'`), shortened.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StatementInfo {
    pub index: usize,
    /// `select`, `create_table_as`, `create_view`, `insert`, `other`, `error`.
    pub kind: String,
    pub range: Range,
    /// The query part to check with the database (`EXPLAIN`), when it is a query.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub query: Option<Range>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Warning {
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub span: Option<Range>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Lineage {
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    pub statements: Vec<StatementInfo>,
    pub warnings: Vec<Warning>,
    /// Some statement could not be parsed: tables are shown without lineage.
    pub partial: bool,
    /// Tables referenced whose columns are unknown (`schema.name` as written).
    pub unknown_tables: Vec<String>,
}

/// A table as the catalog knows it.
#[derive(Debug, Clone)]
pub struct CatalogTable {
    /// Qualified display name (`sales.orders`).
    pub name: String,
    pub columns: Vec<String>,
}

/// Table columns (explorer cache). `parts` = the name as written
/// (`["orders"]`, `["sales", "orders"]`, `["main", "sales", "orders"]`).
pub trait Catalog {
    fn table(&self, parts: &[String]) -> Option<CatalogTable>;
}

/// No schema information: only explicitly named columns are traced.
pub struct NoCatalog;

impl Catalog for NoCatalog {
    fn table(&self, _: &[String]) -> Option<CatalogTable> {
        None
    }
}

impl<F: Fn(&[String]) -> Option<CatalogTable>> Catalog for F {
    fn table(&self, parts: &[String]) -> Option<CatalogTable> {
        self(parts)
    }
}

// ------------------------------------------------------------------ entry

/// Lineage of every statement of `sql`.
pub fn analyze(sql: &str, kind: ConnectorKind, catalog: &dyn Catalog) -> Lineage {
    let mut b = Builder { g: Lineage::default(), cat: catalog, kind, tables: HashMap::new(), text: "", base: 0, lines: vec![], stmt: 0 };
    let spans = split_statements(sql, kind);
    let many = spans.len() > 1;
    for (i, s) in spans.iter().enumerate() {
        b.text = &sql[s.start..s.end];
        b.base = s.start;
        b.lines = line_starts(b.text);
        b.stmt = i;
        let range = Range { start: s.start, end: s.end };
        match Parser::parse_sql(&*dialect_for(kind), b.text) {
            Ok(stmts) => {
                for st in &stmts {
                    b.statement(st, range, many);
                }
            }
            Err(e) => {
                b.g.partial = true;
                b.g.statements.push(StatementInfo { index: i, kind: "error".into(), range, query: None, result: None, error: Some(e.to_string()), });
                b.tables_by_tokens();
            }
        }
    }
    let mut unknown: Vec<String> = b.g.nodes.iter().filter(|n| n.kind == NodeKind::Table && !n.columns_known && !n.written).map(|n| n.name.clone()).collect();
    unknown.sort();
    unknown.dedup();
    b.g.unknown_tables = unknown;
    b.g
}

// ------------------------------------------------------------------ builder

struct Builder<'a> {
    g: Lineage,
    cat: &'a dyn Catalog,
    kind: ConnectorKind,
    /// Table key (lower-case qualified name) → node index; shared by statements.
    tables: HashMap<String, usize>,
    text: &'a str,
    base: usize,
    lines: Vec<usize>,
    stmt: usize,
}

/// A relation visible in a `FROM` scope.
#[derive(Clone)]
struct Source {
    /// Name it is referred to by (alias or table name), lower case.
    alias: String,
    node: usize,
}

#[derive(Clone, Default)]
struct Env {
    /// CTE name (lower case) → node.
    ctes: Vec<(String, usize)>,
    /// Sources of enclosing queries (correlated subqueries, LATERAL).
    outer: Vec<Source>,
}

impl Env {
    fn cte(&self, name: &str) -> Option<usize> {
        self.ctes.iter().rev().find(|(n, _)| n == name).map(|(_, i)| *i)
    }
}

/// Column references, aggregates and subqueries of one expression (not
/// looking inside its subqueries, which are returned to be analyzed alone).
#[derive(Default)]
struct Refs {
    cols: Vec<Vec<Ident>>,
    aggregate: bool,
    window: bool,
    subqueries: Vec<Query>,
    depth: usize,
}

const AGGREGATES: &[&str] = &[
    "count", "sum", "avg", "min", "max", "array_agg", "string_agg", "listagg", "group_concat", "stddev", "stddev_pop", "stddev_samp", "variance",
    "var_pop", "var_samp", "median", "mode", "percentile_cont", "percentile_disc", "approx_count_distinct", "count_if", "bool_and", "bool_or",
    "every", "any_value", "collect_list", "collect_set", "json_agg", "jsonb_agg", "json_object_agg", "first", "last", "arbitrary", "approx_distinct",
    "countif", "logical_and", "logical_or", "bit_and", "bit_or", "corr", "covar_pop", "covar_samp",
];

/// Bare words that are values, not columns.
const PSEUDO: &[&str] = &[
    "sysdate", "systimestamp", "current_date", "current_timestamp", "current_time", "current_user", "session_user", "user", "rownum", "rowid",
    "level", "localtimestamp", "localtime", "true", "false", "null", "default",
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
                } else if let Some(ObjectNamePart::Identifier(n)) = f.name.0.last() {
                    if AGGREGATES.contains(&n.value.to_lowercase().as_str()) {
                        self.aggregate = true;
                    }
                }
            }
            _ => {}
        }
        ControlFlow::Continue(())
    }
}

fn refs(e: &Expr) -> Refs {
    let mut r = Refs::default();
    let _ = e.visit(&mut r);
    r
}

fn is_plain(e: &Expr) -> bool {
    match e {
        Expr::Identifier(_) | Expr::CompoundIdentifier(_) => true,
        Expr::Nested(x) => is_plain(x),
        _ => false,
    }
}

fn name_parts(n: &ObjectName) -> Vec<String> {
    n.0.iter()
        .map(|p| match p {
            ObjectNamePart::Identifier(i) => i.value.clone(),
            other => other.to_string(),
        })
        .collect()
}

fn join_keyword(op: &JoinOperator) -> &'static str {
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

fn line_starts(s: &str) -> Vec<usize> {
    std::iter::once(0).chain(s.match_indices('\n').map(|(i, _)| i + 1)).collect()
}

fn short(s: String) -> String {
    if s.chars().count() <= 160 {
        return s;
    }
    let mut t: String = s.chars().take(157).collect();
    t.push('…');
    t
}

impl<'a> Builder<'a> {
    // ---- spans

    fn offset(&self, l: Location) -> Option<usize> {
        if l.line == 0 {
            return None;
        }
        let start = *self.lines.get(l.line as usize - 1)?;
        let line = &self.text[start..];
        let col = (l.column as usize).saturating_sub(1);
        let off = line.char_indices().nth(col).map(|(i, _)| i).unwrap_or(line.len());
        Some(self.base + start + off)
    }

    fn range(&self, s: Span) -> Option<Range> {
        let (a, b) = (self.offset(s.start)?, self.offset(s.end)?);
        (b >= a).then_some(Range { start: a, end: b })
    }

    fn warn(&mut self, message: String, span: Option<Range>) {
        if !self.g.warnings.iter().any(|w| w.message == message) {
            self.g.warnings.push(Warning { message, span });
        }
    }

    // ---- nodes

    fn node(&mut self, kind: NodeKind, name: String, span: Option<Range>) -> usize {
        let i = self.g.nodes.len();
        self.g.nodes.push(Node {
            id: format!("n{i}"),
            kind,
            name,
            columns: vec![],
            columns_known: kind != NodeKind::Table,
            written: false,
            statement: (kind != NodeKind::Table).then_some(self.stmt),
            span,
        });
        i
    }

    fn table_node(&mut self, name: &ObjectName) -> usize {
        let parts = name_parts(name);
        let found = self.cat.table(&parts);
        let display = found.as_ref().map(|t| t.name.clone()).unwrap_or_else(|| parts.join("."));
        let key = display.to_lowercase();
        if let Some(&i) = self.tables.get(&key) {
            return i;
        }
        let span = self.range(name.span());
        let i = self.node(NodeKind::Table, display, span);
        if let Some(t) = found {
            self.g.nodes[i].columns = t.columns.into_iter().map(|c| Column { name: c, span: None, expr: None }).collect();
            self.g.nodes[i].columns_known = true;
        }
        self.tables.insert(key, i);
        i
    }

    fn col_index(&self, node: usize, name: &str) -> Option<usize> {
        let cols = &self.g.nodes[node].columns;
        cols.iter().position(|c| c.name == name).or_else(|| cols.iter().position(|c| c.name.eq_ignore_ascii_case(name)))
    }

    /// Column of a table node, added when only referenced (unknown columns).
    fn ensure_col(&mut self, node: usize, name: &str) -> String {
        match self.col_index(node, name) {
            Some(i) => self.g.nodes[node].columns[i].name.clone(),
            None => {
                self.g.nodes[node].columns.push(Column { name: name.to_string(), span: None, expr: None });
                name.to_string()
            }
        }
    }

    fn add_out(&mut self, node: usize, name: String, span: Option<Range>, expr: Option<String>) -> String {
        // Duplicate output names (`a, a`) get a suffix so edges stay distinct.
        let mut n = name.clone();
        let mut k = 2;
        while self.col_index(node, &n).is_some_and(|i| self.g.nodes[node].columns[i].name == n) {
            n = format!("{name}_{k}");
            k += 1;
        }
        self.g.nodes[node].columns.push(Column { name: n.clone(), span, expr });
        n
    }

    fn edge(&mut self, from: usize, from_column: Option<String>, to: usize, to_column: Option<String>, kind: EdgeKind, span: Option<Range>) {
        let (from, to) = (self.g.nodes[from].id.clone(), self.g.nodes[to].id.clone());
        let dup = self.g.edges.iter().any(|e| e.from == from && e.from_column == from_column && e.to == to && e.to_column == to_column && e.kind == kind);
        if !dup {
            self.g.edges.push(Edge { from, from_column, to, to_column, kind, span, detail: None });
        }
    }

    fn rename_columns(&mut self, node: usize, alias: &TableAlias) {
        for (i, c) in alias.columns.iter().enumerate() {
            if let Some(col) = self.g.nodes[node].columns.get_mut(i) {
                col.name = c.name.value.clone();
            }
        }
    }

    // ---- statements

    fn statement(&mut self, st: &Statement, range: Range, many: bool) {
        let qrange = |b: &Self, q: &Query| b.range(q.span());
        let index = self.stmt;
        let info = move |kind: &str, query: Option<Range>, result: Option<String>| StatementInfo {
            index,
            kind: kind.into(),
            range,
            query,
            result,
            error: None,
        };
        match st {
            Statement::Query(q) => {
                let r = self.query(q, &Env::default());
                let r = self.as_result(r);
                self.g.nodes[r].name = if many { format!("Result {}", self.stmt + 1) } else { "Result".into() };
                let id = self.g.nodes[r].id.clone();
                let q = Some(range);
                self.g.statements.push(info("select", q, Some(id)));
            }
            Statement::CreateTable(ct) if ct.query.is_some() => {
                let q = ct.query.as_ref().unwrap();
                let names: Vec<String> = ct.columns.iter().map(|c| c.name.value.clone()).collect();
                let id = self.write_into(&ct.name, q, &names, true);
                let qr = qrange(self, q);
                self.g.statements.push(info("create_table_as", qr, Some(id)));
            }
            Statement::CreateView(v) => {
                let names: Vec<String> = v.columns.iter().map(|c| c.name.value.clone()).collect();
                let id = self.write_into(&v.name, &v.query, &names, true);
                let qr = qrange(self, &v.query);
                self.g.statements.push(info("create_view", qr, Some(id)));
            }
            Statement::Insert(ins) if ins.source.is_some() => {
                let q = ins.source.as_ref().unwrap();
                let TableObject::TableName(name) = &ins.table else {
                    self.other(st, range);
                    return;
                };
                let names: Vec<String> = ins.columns.iter().map(|c| name_parts(c).last().cloned().unwrap_or_default()).collect();
                let id = self.write_into(name, q, &names, false);
                let qr = qrange(self, q);
                self.g.statements.push(info("insert", qr, Some(id)));
            }
            _ => self.other(st, range),
        }
    }

    /// Unsupported statement: its tables, without lineage.
    fn other(&mut self, st: &Statement, range: Range) {
        let mut names = Vec::new();
        let _ = sqlparser::ast::visit_relations(st, |r| {
            names.push(r.clone());
            ControlFlow::<()>::Continue(())
        });
        for n in &names {
            self.table_node(n);
        }
        let kw = st.to_string().split_whitespace().next().unwrap_or("").to_uppercase();
        self.warn(format!("{kw} statements are not traced yet; their tables are shown without columns"), Some(range));
        self.g.statements.push(StatementInfo { index: self.stmt, kind: "other".into(), range, query: None, result: None, error: None });
    }

    /// `CREATE TABLE/VIEW … AS q` / `INSERT INTO t (cols) q`: q's columns feed t's.
    /// `defines`: the statement creates the table, so its columns are exactly these.
    fn write_into(&mut self, name: &ObjectName, q: &Query, names: &[String], defines: bool) -> String {
        let r = self.query(q, &Env::default());
        let t = self.table_node(name);
        self.g.nodes[t].written = true;
        if defines && !self.g.nodes[t].columns_known {
            self.g.nodes[t].columns.clear();
            self.g.nodes[t].columns_known = true;
        }
        let outs: Vec<String> = self.g.nodes[r].columns.iter().map(|c| c.name.clone()).collect();
        let known: Vec<String> = self.g.nodes[t].columns.iter().map(|c| c.name.clone()).collect();
        for (i, o) in outs.iter().enumerate() {
            // Target column: listed names, else the table's own order (INSERT), else the query's name.
            let target = names.get(i).cloned().or_else(|| (!names.is_empty()).then(String::new)).filter(|s| !s.is_empty());
            let target = target.or_else(|| known.get(i).cloned()).unwrap_or_else(|| o.clone());
            let tc = self.ensure_col(t, &target);
            self.edge(r, Some(o.clone()), t, Some(tc), EdgeKind::Direct, None);
        }
        self.g.nodes[t].id.clone()
    }

    /// A statement's final relation as a Result node of its own.
    fn as_result(&mut self, r: usize) -> usize {
        if matches!(self.g.nodes[r].kind, NodeKind::Subquery | NodeKind::SetOp) {
            self.g.nodes[r].kind = NodeKind::Result;
            return r;
        }
        let n = self.node(NodeKind::Result, "Result".into(), None);
        for c in self.g.nodes[r].columns.clone() {
            let o = self.add_out(n, c.name.clone(), None, None);
            self.edge(r, Some(c.name), n, Some(o), EdgeKind::Direct, None);
        }
        n
    }

    // ---- queries

    fn query(&mut self, q: &Query, env: &Env) -> usize {
        let mut env = env.clone();
        if let Some(with) = &q.with {
            for cte in &with.cte_tables {
                let name = cte.alias.name.value.clone();
                let key = name.to_lowercase();
                // Recursive CTE: it may read itself; a placeholder stands in until it's known.
                let placeholder = with.recursive.then(|| {
                    let p = self.node(NodeKind::Cte, name.clone(), None);
                    for c in &cte.alias.columns {
                        self.g.nodes[p].columns.push(Column { name: c.name.value.clone(), span: None, expr: None });
                    }
                    p
                });
                let mut inner = env.clone();
                if let Some(p) = placeholder {
                    inner.ctes.push((key.clone(), p));
                }
                let n = self.query(&cte.query, &inner);
                let n = if matches!(self.g.nodes[n].kind, NodeKind::Table | NodeKind::Cte) {
                    // `WITH a AS (TABLE t)`: a node of its own.
                    let m = self.node(NodeKind::Cte, name.clone(), None);
                    for c in self.g.nodes[n].columns.clone() {
                        let o = self.add_out(m, c.name.clone(), None, None);
                        self.edge(n, Some(c.name), m, Some(o), EdgeKind::Direct, None);
                    }
                    m
                } else {
                    n
                };
                self.g.nodes[n].kind = NodeKind::Cte;
                self.g.nodes[n].name = name.clone();
                self.g.nodes[n].span = self.range(cte.alias.name.span);
                self.rename_columns(n, &cte.alias);
                if let Some(p) = placeholder {
                    self.merge_into(p, n);
                }
                env.ctes.push((key, n));
            }
        }
        self.set_expr(&q.body, &env)
    }

    /// Point `from`'s edges at `to` and empty `from` (recursive CTE placeholder).
    fn merge_into(&mut self, from: usize, to: usize) {
        let (fid, tid) = (self.g.nodes[from].id.clone(), self.g.nodes[to].id.clone());
        for e in &mut self.g.edges {
            if e.from == fid {
                e.from = tid.clone();
            }
            if e.to == fid {
                e.to = tid.clone();
            }
        }
        self.g.nodes[from].columns.clear();
        self.g.nodes[from].name = String::new();
    }

    fn set_expr(&mut self, s: &SetExpr, env: &Env) -> usize {
        match s {
            SetExpr::Select(sel) => self.select(sel, env),
            SetExpr::Query(q) => self.query(q, env),
            SetExpr::SetOperation { left, op, set_quantifier, right } => {
                let (l, r) = (self.set_expr(left, env), self.set_expr(right, env));
                let label = format!("{op} {set_quantifier}").trim().to_string();
                let u = self.node(NodeKind::SetOp, label, None);
                let lc: Vec<String> = self.g.nodes[l].columns.iter().map(|c| c.name.clone()).collect();
                let rc: Vec<String> = self.g.nodes[r].columns.iter().map(|c| c.name.clone()).collect();
                for (i, c) in lc.iter().enumerate() {
                    let o = self.add_out(u, c.clone(), None, None);
                    self.edge(l, Some(c.clone()), u, Some(o.clone()), EdgeKind::Direct, None);
                    if let Some(rcn) = rc.get(i) {
                        self.edge(r, Some(rcn.clone()), u, Some(o), EdgeKind::Direct, None);
                    }
                }
                if lc.len() != rc.len() && !lc.is_empty() && !rc.is_empty() {
                    self.warn(format!("{} has {} columns on the left and {} on the right", self.g.nodes[u].name, lc.len(), rc.len()), None);
                }
                u
            }
            SetExpr::Values(v) => {
                let n = self.node(NodeKind::Subquery, "VALUES".into(), None);
                let width = v.rows.first().map(|r| r.len()).unwrap_or(0);
                for i in 1..=width {
                    self.add_out(n, format!("column{i}"), None, None);
                }
                n
            }
            SetExpr::Table(t) => {
                let name = ObjectName::from(
                    t.schema_name.iter().chain(t.table_name.iter()).map(|s| Ident::new(s.clone())).collect::<Vec<_>>(),
                );
                self.table_node(&name)
            }
            other => {
                let n = self.node(NodeKind::Subquery, "…".into(), None);
                let span = self.range(other.span());
                self.warn("A nested INSERT/UPDATE/DELETE/MERGE is not traced".into(), span);
                n
            }
        }
    }

    fn select(&mut self, sel: &Select, env: &Env) -> usize {
        let n = self.node(NodeKind::Subquery, "SELECT".into(), None);
        let mut sources: Vec<Source> = Vec::new();
        for twj in &sel.from {
            self.add_from(twj, env, &mut sources, n);
        }
        let scope_env = Env { ctes: env.ctes.clone(), outer: sources.iter().cloned().chain(env.outer.iter().cloned()).collect() };

        for item in &sel.projection {
            match item {
                SelectItem::UnnamedExpr(e) => {
                    let name = match e {
                        Expr::Identifier(i) => i.value.clone(),
                        Expr::CompoundIdentifier(v) => v.last().map(|i| i.value.clone()).unwrap_or_default(),
                        other => short(other.to_string()),
                    };
                    self.projected(n, name, e, None, &sources, &scope_env);
                }
                SelectItem::ExprWithAlias { expr, alias } => {
                    let span = self.range(alias.span);
                    self.projected(n, alias.value.clone(), expr, span, &sources, &scope_env);
                }
                SelectItem::ExprWithAliases { expr, aliases } => {
                    for a in aliases {
                        let span = self.range(a.span);
                        self.projected(n, a.value.clone(), expr, span, &sources, &scope_env);
                    }
                }
                SelectItem::Wildcard(_) => {
                    for s in sources.clone() {
                        self.expand_star(n, &s, item);
                    }
                }
                SelectItem::QualifiedWildcard(kind, _) => match kind {
                    SelectItemQualifiedWildcardKind::ObjectName(on) => {
                        let parts = name_parts(on);
                        match self.find_source(&sources, &parts) {
                            Some(s) => self.expand_star(n, &s, item),
                            None => {
                                let span = self.range(on.span());
                                self.warn(format!("Unknown table {}.* ", parts.join(".")), span);
                            }
                        }
                    }
                    SelectItemQualifiedWildcardKind::Expr(e) => {
                        self.projected(n, short(e.to_string()), e, None, &sources, &scope_env);
                    }
                },
            }
        }
        for (kw, f) in [("WHERE", &sel.selection), ("HAVING", &sel.having), ("QUALIFY", &sel.qualify), ("PREWHERE", &sel.prewhere)] {
            if let Some(f) = f {
                let at = self.g.edges.len();
                self.rows(n, f, EdgeKind::Filter, &sources, &scope_env);
                self.detail_since(at, format!("{kw} {f}"));
            }
        }
        n
    }

    /// One output column of `n` computed by `e`.
    fn projected(&mut self, n: usize, name: String, e: &Expr, span: Option<Range>, sources: &[Source], env: &Env) {
        let r = refs(e);
        let kind = if is_plain(e) {
            EdgeKind::Direct
        } else if r.aggregate {
            EdgeKind::Aggregate
        } else {
            EdgeKind::Transform
        };
        let span = span.or_else(|| self.range(e.span()));
        let expr = (!is_plain(e)).then(|| short(e.to_string()));
        let out = self.add_out(n, name, span, expr);
        for parts in &r.cols {
            if let Some((src, col)) = self.resolve(sources, &env.outer, parts) {
                let s = self.range(Span::union_iter(parts.iter().map(|i| i.span)));
                self.edge(src, Some(col), n, Some(out.clone()), kind, s);
            }
        }
        for q in &r.subqueries {
            let m = self.query(q, env);
            let first = self.g.nodes[m].columns.first().map(|c| c.name.clone());
            let s = self.range(q.span());
            self.edge(m, first, n, Some(out.clone()), EdgeKind::Transform, s);
        }
    }

    /// Label edges added since `at` with the clause they come from.
    fn detail_since(&mut self, at: usize, detail: String) {
        let d = short(detail);
        for e in &mut self.g.edges[at..] {
            e.detail.get_or_insert_with(|| d.clone());
        }
    }

    /// `e` decides which rows of `n` are kept (filters, join conditions).
    fn rows(&mut self, n: usize, e: &Expr, kind: EdgeKind, sources: &[Source], env: &Env) {
        let r = refs(e);
        for parts in &r.cols {
            if let Some((src, col)) = self.resolve(sources, &env.outer, parts) {
                let s = self.range(Span::union_iter(parts.iter().map(|i| i.span)));
                self.edge(src, Some(col), n, None, kind, s);
            }
        }
        for q in &r.subqueries {
            let m = self.query(q, env);
            let cols: Vec<String> = self.g.nodes[m].columns.iter().map(|c| c.name.clone()).collect();
            let s = self.range(q.span());
            for c in cols {
                self.edge(m, Some(c), n, None, kind, s);
            }
        }
    }

    fn expand_star(&mut self, n: usize, s: &Source, item: &SelectItem) {
        let node = &self.g.nodes[s.node];
        if node.kind == NodeKind::Table && !node.columns_known {
            let name = node.name.clone();
            let out = self.add_out(n, format!("{}.*", s.alias), self.range(item.span()), None);
            self.edge(s.node, None, n, Some(out), EdgeKind::Direct, None);
            let span = self.range(item.span());
            self.warn(format!("Columns of {name} are not cached: open it in the explorer (or connect) to expand *"), span);
            return;
        }
        for c in node.columns.clone() {
            let out = self.add_out(n, c.name.clone(), None, None);
            self.edge(s.node, Some(c.name), n, Some(out), EdgeKind::Direct, None);
        }
    }

    fn add_from(&mut self, twj: &TableWithJoins, env: &Env, sources: &mut Vec<Source>, n: usize) {
        self.factor(&twj.relation, env, sources, n);
        for j in &twj.joins {
            let left = sources.len();
            self.factor(&j.relation, env, sources, n);
            let constraint = match &j.join_operator {
                JoinOperator::Join(c)
                | JoinOperator::Inner(c)
                | JoinOperator::Left(c)
                | JoinOperator::LeftOuter(c)
                | JoinOperator::Right(c)
                | JoinOperator::RightOuter(c)
                | JoinOperator::FullOuter(c)
                | JoinOperator::CrossJoin(c)
                | JoinOperator::Semi(c)
                | JoinOperator::LeftSemi(c)
                | JoinOperator::RightSemi(c)
                | JoinOperator::Anti(c)
                | JoinOperator::LeftAnti(c)
                | JoinOperator::RightAnti(c)
                | JoinOperator::StraightJoin(c) => Some(c),
                JoinOperator::AsOf { constraint, .. } => Some(constraint),
                _ => None,
            };
            let scope = Env { ctes: env.ctes.clone(), outer: env.outer.clone() };
            let at = self.g.edges.len();
            let op = join_keyword(&j.join_operator);
            let cond = match constraint {
                Some(JoinConstraint::On(e)) => format!(" ON {e}"),
                Some(JoinConstraint::Using(c)) => format!(" USING ({})", c.iter().map(|x| x.to_string()).collect::<Vec<_>>().join(", ")),
                _ => String::new(),
            };
            let detail = format!("{op} {}{cond}", j.relation);
            match constraint {
                Some(JoinConstraint::On(e)) => self.rows(n, e, EdgeKind::Join, sources, &scope),
                Some(JoinConstraint::Using(cols)) => {
                    for c in cols {
                        let name = name_parts(c).last().cloned().unwrap_or_default();
                        let ident = vec![Ident::new(name)];
                        // `USING (id)`: the column on both sides.
                        for side in [&sources[..left], &sources[left..]] {
                            if let Some((src, col)) = self.resolve(side, &[], &ident) {
                                self.edge(src, Some(col), n, None, EdgeKind::Join, self.range(c.span()));
                            }
                        }
                    }
                }
                _ => {}
            }
            self.detail_since(at, detail);
        }
    }

    fn factor(&mut self, tf: &TableFactor, env: &Env, sources: &mut Vec<Source>, n: usize) {
        let alias_of = |a: &Option<TableAlias>| a.as_ref().map(|a| a.name.value.to_lowercase());
        match tf {
            TableFactor::Table { name, alias, args, .. } => {
                let parts = name_parts(name);
                let node = if args.is_some() {
                    let m = self.node(NodeKind::Table, short(tf.to_string()), self.range(name.span()));
                    self.g.nodes[m].statement = None;
                    m
                } else if let Some(c) = (parts.len() == 1).then(|| env.cte(&parts[0].to_lowercase())).flatten() {
                    c
                } else {
                    self.table_node(name)
                };
                let a = alias_of(alias).unwrap_or_else(|| parts.last().cloned().unwrap_or_default().to_lowercase());
                sources.push(Source { alias: a, node });
            }
            TableFactor::Derived { lateral, subquery, alias, .. } => {
                let inner = if *lateral {
                    Env { ctes: env.ctes.clone(), outer: sources.iter().cloned().chain(env.outer.iter().cloned()).collect() }
                } else {
                    env.clone()
                };
                let m = self.query(subquery, &inner);
                let m = if matches!(self.g.nodes[m].kind, NodeKind::Table | NodeKind::Cte) { m } else {
                    self.g.nodes[m].name = alias.as_ref().map(|a| a.name.value.clone()).unwrap_or_else(|| "subquery".into());
                    if let Some(a) = alias {
                        self.g.nodes[m].span = self.range(a.name.span);
                        self.rename_columns(m, a);
                    }
                    m
                };
                sources.push(Source { alias: alias_of(alias).unwrap_or_default(), node: m });
            }
            TableFactor::NestedJoin { table_with_joins, .. } => self.add_from(table_with_joins, env, sources, n),
            TableFactor::UNNEST { alias, array_exprs, .. } => {
                let m = self.node(NodeKind::Subquery, "UNNEST".into(), self.range(tf.span()));
                let names: Vec<String> = alias.as_ref().map(|a| a.columns.iter().map(|c| c.name.value.clone()).collect()).unwrap_or_default();
                let name = names.first().cloned().or_else(|| alias.as_ref().map(|a| a.name.value.clone())).unwrap_or_else(|| "unnest".into());
                let out = self.add_out(m, name, None, None);
                let outer: Vec<Source> = sources.iter().cloned().chain(env.outer.iter().cloned()).collect();
                for e in array_exprs {
                    for parts in refs(e).cols {
                        if let Some((src, col)) = self.resolve(&outer, &[], &parts) {
                            self.edge(src, Some(col), m, Some(out.clone()), EdgeKind::Transform, None);
                        }
                    }
                }
                sources.push(Source { alias: alias_of(alias).unwrap_or_else(|| "unnest".into()), node: m });
            }
            other => {
                // Table functions, PIVOT, JSON_TABLE…: a node without known columns.
                let m = self.node(NodeKind::Table, short(other.to_string()), self.range(other.span()));
                self.g.nodes[m].statement = None;
                let alias = match other {
                    TableFactor::TableFunction { alias, .. } | TableFactor::Function { alias, .. } | TableFactor::Pivot { alias, .. } => alias_of(alias),
                    _ => None,
                };
                sources.push(Source { alias: alias.unwrap_or_default(), node: m });
            }
        }
    }

    fn find_source(&self, sources: &[Source], qual: &[String]) -> Option<Source> {
        let q = qual.last()?.to_lowercase();
        let full = qual.join(".").to_lowercase();
        sources
            .iter()
            .find(|s| s.alias == q)
            .or_else(|| sources.iter().find(|s| {
                let name = self.g.nodes[s.node].name.to_lowercase();
                name == full || name.ends_with(&format!(".{full}"))
            }))
            .cloned()
    }

    /// The source column a reference means: (node, column name).
    fn resolve(&mut self, sources: &[Source], outer: &[Source], parts: &[Ident]) -> Option<(usize, String)> {
        let span = self.range(Span::union_iter(parts.iter().map(|i| i.span)));
        let written = parts.iter().map(|i| i.value.as_str()).collect::<Vec<_>>().join(".");
        let col = parts.last()?.value.clone();
        if parts.len() >= 2 {
            let qual: Vec<String> = parts[..parts.len() - 1].iter().map(|i| i.value.clone()).collect();
            for scope in [sources, outer] {
                if let Some(s) = self.find_source(scope, &qual) {
                    return self.column_of(&s, &col, &written, span);
                }
            }
            // `col.field` (struct / JSON access) on an unqualified column.
            if let Some(r) = self.resolve(sources, outer, &parts[..1]) {
                return Some(r);
            }
            self.warn(format!("Unknown table or alias in {written}"), span);
            return None;
        }
        for scope in [sources, outer] {
            let with: Vec<&Source> = scope.iter().filter(|s| self.col_index(s.node, &col).is_some()).collect();
            if with.len() > 1 {
                let names: Vec<&str> = with.iter().map(|s| s.alias.as_str()).collect();
                self.warn(format!("Column {col} is ambiguous (in {})", names.join(", ")), span);
            }
            if let Some(s) = with.first() {
                let s = (*s).clone();
                return self.column_of(&s, &col, &written, span);
            }
            // Only tables with unknown columns could have it.
            let unknown: Vec<&Source> = scope.iter().filter(|s| self.g.nodes[s.node].kind == NodeKind::Table && !self.g.nodes[s.node].columns_known).collect();
            if unknown.len() == 1 {
                let s = unknown[0].clone();
                return self.column_of(&s, &col, &written, span);
            }
            if unknown.len() > 1 {
                self.warn(format!("Can't tell which table {col} belongs to (columns not cached); qualify it as alias.{col}"), span);
                return None;
            }
        }
        self.warn(format!("Unknown column {written}"), span);
        None
    }

    fn column_of(&mut self, s: &Source, col: &str, written: &str, span: Option<Range>) -> Option<(usize, String)> {
        let node = &self.g.nodes[s.node];
        if self.col_index(s.node, col).is_some() || (node.kind == NodeKind::Table && !node.columns_known) {
            return Some((s.node, self.ensure_col(s.node, col)));
        }
        if node.kind == NodeKind::Table {
            let name = node.name.clone();
            self.warn(format!("{written}: {name} has no column {col} (in the cached schema)"), span);
            return Some((s.node, self.ensure_col(s.node, col)));
        }
        let name = node.name.clone();
        self.warn(format!("{written}: {name} has no column {col}"), span);
        None
    }

    /// A statement that does not parse: tables after FROM/JOIN/INTO/UPDATE/TABLE, without lineage.
    fn tables_by_tokens(&mut self) {
        let dialect = dialect_for(self.kind);
        let Ok(tokens) = Tokenizer::new(&*dialect, self.text).tokenize() else { return };
        let words: Vec<&Token> = tokens.iter().filter(|t| !matches!(t, Token::Whitespace(_))).collect();
        let mut i = 0;
        while i < words.len() {
            let after = matches!(words[i], Token::Word(w) if ["FROM", "JOIN", "INTO", "UPDATE", "TABLE"].contains(&w.value.to_uppercase().as_str()));
            i += 1;
            let trigger = |t: Option<&&Token>| matches!(t, Some(Token::Word(w)) if w.quote_style.is_none() && ["FROM", "JOIN", "INTO", "UPDATE", "TABLE"].contains(&w.value.to_uppercase().as_str()));
            if !after || trigger(words.get(i)) {
                continue;
            }
            let mut parts: Vec<Ident> = Vec::new();
            while let Some(Token::Word(w)) = words.get(i) {
                parts.push(Ident { value: w.value.clone(), quote_style: w.quote_style, span: Span::empty() });
                if !matches!(words.get(i + 1), Some(Token::Period)) {
                    i += 1;
                    break;
                }
                i += 2;
            }
            let keyword = parts.len() == 1 && parts[0].quote_style.is_none() && ["SELECT", "LATERAL", "ONLY", "IF"].contains(&parts[0].value.to_uppercase().as_str());
            if !parts.is_empty() && !keyword {
                self.table_node(&ObjectName::from(parts));
            }
        }
    }
}

#[cfg(test)]
mod tests;
