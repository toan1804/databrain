use databrain_connector_core::sql::dialect_for;
use sqlparser::ast::{
    ConnectByKind, Distinct, ExcludeSelectItem, Expr, GroupByExpr, Ident, LateralView, NamedWindowDefinition, NamedWindowExpr, ObjectName, OrderBy,
    OrderByKind, Query, RenameSelectItem, Select, SelectItem, SelectItemQualifiedWildcardKind, SetExpr, Statement,
};
use sqlparser::parser::Parser;
use sqlparser::tokenizer::Span;

use super::*;
use crate::*;

impl<'a> Builder<'a> {
    // ---- queries

    pub(super) fn query(&mut self, q: &Query, env: &Env) -> usize {
        let env = self.with_ctes(q, env);
        match &*q.body {
            // Its `ORDER BY` may name columns of the select's tables.
            SetExpr::Select(sel) => self.select(sel, &env, q.order_by.as_ref()),
            body => self.set_expr(body, &env),
        }
    }

    /// `env` plus the CTEs of `q`'s `WITH` (each analyzed in turn).
    pub(super) fn with_ctes(&mut self, q: &Query, env: &Env) -> Env {
        let mut env = env.clone();
        if let Some(with) = &q.with {
            for cte in &with.cte_tables {
                let name = cte.alias.name.value.clone();
                let key = self.fold(&cte.alias.name);
                // Recursive CTE: it may read itself; a placeholder stands in until it's known.
                // SQL Server and Oracle write it without RECURSIVE: a CTE that reads its own
                // name is one. Without a column list, its columns are the anchor's.
                let recursive = with.recursive || reads_itself(&cte.query, &name.to_lowercase());
                let placeholder = recursive.then(|| {
                    let p = self.node(NodeKind::Cte, name.clone(), None);
                    let names: Vec<String> =
                        if cte.alias.columns.is_empty() { anchor_names(&cte.query) } else { cte.alias.columns.iter().map(|c| c.name.value.clone()).collect() };
                    for c in names {
                        self.g.nodes[p].columns.push(Column::named(c));
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
        env
    }

    /// A database view read here: trace its definition (from the catalog)
    /// into the view's columns, once per view, at most 5 views deep. A view
    /// the script writes is its own definition, not the database's.
    pub(super) fn expand_view(&mut self, node: usize, parts: &[String]) {
        const MAX_DEPTH: usize = 5;
        if self.expanded.contains(&node) || self.g.nodes[node].written || self.created.contains(&node) || self.in_view >= MAX_DEPTH {
            return;
        }
        let Some(sql) = self.lookup(parts).0.and_then(|t| t.view_sql) else { return };
        self.expanded.insert(node);
        let name = self.g.nodes[node].name.clone();
        let parsed = Parser::parse_sql(&*dialect_for(self.kind), &sql).ok().and_then(|stmts| {
            stmts.into_iter().find_map(|st| match st {
                Statement::CreateView(v) => Some((v.query, v.columns.iter().map(|c| c.name.value.clone()).collect::<Vec<_>>())),
                Statement::Query(q) => Some((q, vec![])),
                _ => None,
            })
        });
        let Some((q, names)) = parsed else {
            self.warn(format!("The definition of view {name} could not be read; it is shown as a table"), None);
            return;
        };
        // Bare names in the definition are the view's schema's.
        let mut schema: Vec<String> = name.split('.').map(str::to_string).collect();
        schema.pop();
        self.view_schema.push(schema);
        self.in_view += 1;
        let r = self.query(&q, &Env::default());
        self.in_view -= 1;
        self.view_schema.pop();
        if !matches!(self.g.nodes[r].kind, NodeKind::Table | NodeKind::Cte) {
            self.g.nodes[r].name = format!("{name} definition");
        }
        let outs: Vec<String> = self.g.nodes[r].columns.iter().map(|c| c.name.clone()).collect();
        for (i, o) in outs.iter().enumerate() {
            let target = names.get(i).cloned().unwrap_or_else(|| o.clone());
            let vc = self.col_or_add(node, &target);
            self.edge(r, Some(o.clone()), node, Some(vc), EdgeKind::Direct, None);
        }
        self.g.nodes[node].view = true;
    }

    /// Point `from`'s edges at `to` and empty `from` (recursive CTE placeholder).
    pub(super) fn merge_into(&mut self, from: usize, to: usize) {
        let (fid, tid) = (self.g.nodes[from].id.clone(), self.g.nodes[to].id.clone());
        self.rewrite_edges(|e| {
            let mut changed = false;
            if e.from == fid {
                e.from = tid.clone();
                changed = true;
            }
            if e.to == fid {
                e.to = tid.clone();
                changed = true;
            }
            changed
        });
        self.g.nodes[from].columns.clear();
        self.g.nodes[from].name = String::new();
    }

    pub(super) fn set_expr(&mut self, s: &SetExpr, env: &Env) -> usize {
        match s {
            SetExpr::Select(sel) => self.select(sel, env, None),
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
                let name = ObjectName::from(t.schema_name.iter().chain(t.table_name.iter()).map(|s| Ident::new(s.clone())).collect::<Vec<_>>());
                let n = self.table_node(&name);
                self.expand_view(n, &name_parts(&name));
                n
            }
            other => {
                let n = self.node(NodeKind::Subquery, "…".into(), None);
                let span = self.range(other.span());
                self.warn("A nested INSERT/UPDATE/DELETE/MERGE is not traced".into(), span);
                n
            }
        }
    }

    pub(super) fn select(&mut self, sel: &Select, env: &Env, order_by: Option<&OrderBy>) -> usize {
        let n = self.node(NodeKind::Subquery, "SELECT".into(), None);
        let mut sources: Vec<Source> = Vec::new();
        for twj in &sel.from {
            self.add_from(twj, env, &mut sources, n);
        }
        for lv in &sel.lateral_views {
            self.lateral_view(lv, &mut sources, env);
        }
        let scope_env = Env { ctes: env.ctes.clone(), outer: sources.iter().cloned().chain(env.outer.iter().cloned()).collect() };

        for item in &sel.projection {
            match item {
                SelectItem::UnnamedExpr(e) => {
                    let name = item_name(e);
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
                    let mut done = Vec::new();
                    for s in sources.clone() {
                        self.expand_star(n, &s, item, &sources, &scope_env, &mut done);
                    }
                }
                SelectItem::QualifiedWildcard(kind, _) => match kind {
                    SelectItemQualifiedWildcardKind::ObjectName(on) => {
                        let parts = name_parts(on);
                        match self.find_source(&sources, &idents(on)) {
                            Some(s) => self.expand_star(n, &s, item, &sources, &scope_env, &mut Vec::new()),
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
                if kw == "WHERE" {
                    self.equi_joins(f, JoinKind::Where, &sources, &env.outer);
                }
                let at = self.g.edges.len();
                // `HAVING total > 5`, `QUALIFY rn = 1`: a select alias stands for its expression.
                let aliases = matches!(kw, "HAVING" | "QUALIFY");
                self.rows_of(n, f, EdgeKind::Filter, &sources, &scope_env, aliases);
                self.detail_since(at, format!("{kw} {f}"));
            }
        }
        // Oracle's hierarchical queries: `START WITH` picks the roots, `CONNECT BY` the children.
        for c in &sel.connect_by {
            let (kw, exprs): (&str, Vec<&Expr>) = match c {
                ConnectByKind::ConnectBy { relationships, .. } => ("CONNECT BY", relationships.iter().collect()),
                ConnectByKind::StartWith { condition, .. } => ("START WITH", vec![&**condition]),
            };
            for e in exprs {
                let at = self.g.edges.len();
                self.rows(n, e, EdgeKind::Filter, &sources, &scope_env);
                self.detail_since(at, format!("{kw} {e}"));
            }
        }
        // Grouped, made distinct or sorted by: used, though no value flows from it.
        if let GroupByExpr::Expressions(exprs, _) = &sel.group_by {
            for e in exprs {
                self.mark_refs(e, n, &sources, true);
            }
        }
        if let Some(Distinct::On(exprs)) = &sel.distinct {
            for e in exprs {
                self.mark_refs(e, n, &sources, true);
            }
        }
        // `WINDOW w AS (PARTITION BY … ORDER BY …)`, used by `OVER w`.
        for NamedWindowDefinition(_, w) in &sel.named_window {
            if let NamedWindowExpr::WindowSpec(spec) = w {
                for e in spec.partition_by.iter().chain(spec.order_by.iter().map(|o| &o.expr)) {
                    self.mark_refs(e, n, &sources, false);
                }
            }
        }
        if let Some(OrderBy { kind: OrderByKind::Expressions(exprs), .. }) = order_by {
            for o in exprs {
                self.mark_refs(&o.expr, n, &sources, false);
            }
        }
        n
    }

    /// One output column of `n` computed by `e`.
    pub(super) fn projected(&mut self, n: usize, name: String, e: &Expr, span: Option<Range>, sources: &[Source], env: &Env) {
        let r = refs(e, self.kind);
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
        self.missed = false;
        for parts in &r.cols {
            if let Some((src, col)) = self.resolve(sources, &env.outer, parts) {
                let s = self.range(Span::union_iter(parts.iter().map(|i| i.span)));
                self.edge(src, Some(col), n, Some(out.clone()), kind, s);
            }
        }
        // `o.id || email` with `email` not placed: the edges are not all it reads.
        if std::mem::take(&mut self.missed) {
            self.set_untraced(n, &out);
        }
        for q in &r.subqueries {
            let m = self.query(q, env);
            let first = self.g.nodes[m].columns.first().map(|c| c.name.clone());
            let s = self.range(q.span());
            self.edge(m, first, n, Some(out.clone()), EdgeKind::Transform, s);
        }
    }

    /// Label edges added since `at` with the clause they come from.
    pub(super) fn detail_since(&mut self, at: usize, detail: String) {
        let d = short(detail);
        for e in &mut self.g.edges[at..] {
            e.detail.get_or_insert_with(|| d.clone());
        }
    }

    /// `e` decides which rows of `n` are kept (filters, join conditions).
    pub(super) fn rows(&mut self, n: usize, e: &Expr, kind: EdgeKind, sources: &[Source], env: &Env) {
        self.rows_of(n, e, kind, sources, env, false);
    }

    /// [`Self::rows`]; with `aliases`, a bare name that is one of `n`'s
    /// outputs and no source's known column means that output: what feeds
    /// it decides the rows.
    pub(super) fn rows_of(&mut self, n: usize, e: &Expr, kind: EdgeKind, sources: &[Source], env: &Env, aliases: bool) {
        let r = refs(e, self.kind);
        for parts in &r.cols {
            if aliases && let [one] = &parts[..] {
                let known = sources.iter().any(|s| !self.open_columns(s.node) && self.col_index(s.node, &one.value).is_some());
                if !known && let Some(i) = self.col_index(n, &one.value) {
                    let (id, out) = (self.g.nodes[n].id.clone(), self.g.nodes[n].columns[i].name.clone());
                    let span = self.range(one.span);
                    let feeds: Vec<(String, Option<String>)> = self
                        .g
                        .edges
                        .iter()
                        .filter(|e| e.to == id && e.to_column.as_deref() == Some(out.as_str()) && !matches!(e.kind, EdgeKind::Filter | EdgeKind::Join))
                        .map(|e| (e.from.clone(), e.from_column.clone()))
                        .collect();
                    for (from, col) in feeds {
                        if let Some(f) = self.g.nodes.iter().position(|x| x.id == from) {
                            self.edge(f, col, n, None, kind, span);
                        }
                    }
                    continue;
                }
            }
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

    /// `*` / `t.*` of source `s`, with its options: `EXCLUDE` / `EXCEPT`
    /// leave columns out, `REPLACE (expr AS c)` computes `c` instead,
    /// `RENAME (a AS b)` names `a` `b`. Over a source whose columns aren't
    /// known, the replaced and renamed columns are outputs of their own
    /// (once per `*`: `done`), next to the `s.*` column.
    pub(super) fn expand_star(&mut self, n: usize, s: &Source, item: &SelectItem, sources: &[Source], env: &Env, done: &mut Vec<String>) {
        let opts = match item {
            SelectItem::Wildcard(o) | SelectItem::QualifiedWildcard(_, o) => Some(o),
            _ => None,
        };
        let lower = |i: &Ident| i.value.to_lowercase();
        let mut excluded: Vec<String> = Vec::new();
        let mut replaced: Vec<(String, &Expr)> = Vec::new();
        let mut renamed: Vec<(String, String)> = Vec::new();
        if let Some(o) = opts {
            match &o.opt_exclude {
                Some(ExcludeSelectItem::Single(c)) => excluded.extend(name_parts(c).last().map(|c| c.to_lowercase())),
                Some(ExcludeSelectItem::Multiple(cs)) => excluded.extend(cs.iter().filter_map(|c| name_parts(c).last().map(|c| c.to_lowercase()))),
                None => {}
            }
            if let Some(e) = &o.opt_except {
                excluded.extend(std::iter::once(&e.first_element).chain(&e.additional_elements).map(lower));
            }
            if let Some(r) = &o.opt_replace {
                replaced.extend(r.items.iter().map(|r| (lower(&r.column_name), &r.expr)));
            }
            match &o.opt_rename {
                Some(RenameSelectItem::Single(r)) => renamed.push((lower(&r.ident), r.alias.value.clone())),
                Some(RenameSelectItem::Multiple(rs)) => renamed.extend(rs.iter().map(|r| (lower(&r.ident), r.alias.value.clone()))),
                None => {}
            }
        }
        if self.open_columns(s.node) {
            let name = self.g.nodes[s.node].name.clone();
            let out = self.add_out(n, format!("{}.*", s.alias), self.range(item.span()), None);
            self.edge(s.node, None, n, Some(out), EdgeKind::Direct, None);
            let span = self.range(item.span());
            self.warn(format!("Columns of {name} are not cached: open it in the explorer (or connect) to expand *"), span);
            for (c, e) in &replaced {
                if !done.contains(c) {
                    done.push(c.clone());
                    self.projected(n, c.clone(), e, None, sources, env);
                }
            }
            for (old, new) in &renamed {
                if !done.contains(old) {
                    done.push(old.clone());
                    let col = self.ensure_col(s.node, old);
                    let out = self.add_out(n, new.clone(), None, None);
                    self.edge(s.node, Some(col), n, Some(out), EdgeKind::Direct, None);
                }
            }
            return;
        }
        for c in self.g.nodes[s.node].columns.clone() {
            let key = c.name.to_lowercase();
            if excluded.contains(&key) {
                continue;
            }
            if let Some((_, e)) = replaced.iter().find(|(r, _)| *r == key) {
                self.projected(n, c.name.clone(), e, None, sources, env);
                continue;
            }
            let name = renamed.iter().find(|(o, _)| *o == key).map(|(_, new)| new.clone()).unwrap_or_else(|| c.name.clone());
            let out = self.add_out(n, name, None, None);
            if c.untraced {
                self.set_untraced(n, &out);
            }
            self.edge(s.node, Some(c.name), n, Some(out), EdgeKind::Direct, None);
        }
    }

    /// Hive / Databricks `LATERAL VIEW explode(…) t AS a, b`: a source `t`
    /// whose columns are computed from what the expression reads.
    fn lateral_view(&mut self, lv: &LateralView, sources: &mut Vec<Source>, env: &Env) {
        let alias = name_parts(&lv.lateral_view_name).last().cloned().unwrap_or_default();
        let m = self.node(NodeKind::Subquery, alias.clone(), self.range(lv.lateral_view_name.span()));
        let names: Vec<String> = lv.lateral_col_alias.iter().map(|i| i.value.clone()).collect();
        if names.is_empty() {
            self.g.nodes[m].columns_known = false;
        }
        let cols: Vec<String> = names.into_iter().map(|c| self.add_out(m, c, None, None)).collect();
        let before: Vec<Source> = sources.iter().cloned().chain(env.outer.iter().cloned()).collect();
        self.missed = false;
        for parts in refs(&lv.lateral_view, self.kind).cols {
            if let Some((src, col)) = self.resolve(&before, &[], &parts) {
                if cols.is_empty() {
                    self.edge(src, Some(col.clone()), m, None, EdgeKind::Transform, None);
                }
                for c in &cols {
                    self.edge(src, Some(col.clone()), m, Some(c.clone()), EdgeKind::Transform, None);
                }
            }
        }
        if std::mem::take(&mut self.missed) {
            self.untraced_nodes.insert(m);
            for c in &mut self.g.nodes[m].columns {
                c.untraced = true;
            }
        }
        sources.push(Source { alias: fold_ident(self.kind, &Ident::new(alias)), node: m });
    }
}
