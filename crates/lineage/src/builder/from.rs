use sqlparser::ast::{Expr, Ident, Join, JoinConstraint, JoinOperator, TableAlias, TableFactor, TableWithJoins, Visit};
use sqlparser::tokenizer::Span;

use super::*;
use crate::*;

impl<'a> Builder<'a> {
    pub(super) fn add_from(&mut self, twj: &TableWithJoins, env: &Env, sources: &mut Vec<Source>, n: usize) {
        self.factor(&twj.relation, env, sources, n, false);
        self.add_joins(&twj.joins, env, sources, n);
    }

    pub(super) fn add_joins(&mut self, joins: &[Join], env: &Env, sources: &mut Vec<Source>, n: usize) {
        for j in joins {
            let left = sources.len();
            // `CROSS APPLY` / `OUTER APPLY` see the tables before them, as `LATERAL` does.
            let apply = matches!(j.join_operator, JoinOperator::CrossApply | JoinOperator::OuterApply);
            self.factor(&j.relation, env, sources, n, apply);
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
            if let Some(kind) = joins::join_kind(&j.join_operator) {
                match constraint {
                    Some(JoinConstraint::On(e)) => self.equi_joins(e, kind, sources, &env.outer),
                    Some(JoinConstraint::Using(cols)) => {
                        let idents: Vec<Vec<Ident>> = cols.iter().map(|c| vec![Ident::new(name_parts(c).last().cloned().unwrap_or_default())]).collect();
                        let (l, r) = sources.split_at(left);
                        self.using_joins(&idents, kind, l, r, &env.outer);
                    }
                    _ => {}
                }
            }
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

    /// One item of a `FROM`. `lateral`: it sees the items before it (`CROSS APPLY`).
    pub(super) fn factor(&mut self, tf: &TableFactor, env: &Env, sources: &mut Vec<Source>, n: usize, lateral: bool) {
        let kind = self.kind;
        let alias_of = |a: &Option<TableAlias>| a.as_ref().map(|a| fold_ident(kind, &a.name));
        // What a lateral item (and any table function) may read: the items before it, then the enclosing queries'.
        let before: Vec<Source> = sources.iter().cloned().chain(env.outer.iter().cloned()).collect();
        match tf {
            TableFactor::Table { name, alias, args: Some(args), .. } => {
                // `FROM generate_series(…)`, `FROM flatten(input => u.email)`.
                let label = name_parts(name).join(".");
                let m = self.function_node(label, &args.args, alias, &before, env, self.range(tf.span()));
                sources.push(Source { alias: alias_of(alias).unwrap_or_else(|| name_parts(name).last().cloned().unwrap_or_default().to_lowercase()), node: m });
            }
            TableFactor::Table { name, alias, .. } => {
                let parts = name_parts(name);
                let node = if let Some(c) = (parts.len() == 1).then(|| env.cte(&self.fold_name(name)[0])).flatten() {
                    c
                } else {
                    let t = self.table_node(name);
                    self.expand_view(t, &parts);
                    t
                };
                let a = alias_of(alias).unwrap_or_else(|| self.fold_name(name).last().cloned().unwrap_or_default());
                sources.push(Source { alias: a, node });
            }
            TableFactor::Derived { lateral: is_lateral, subquery, alias, .. } => {
                let inner = if *is_lateral || lateral { Env { ctes: env.ctes.clone(), outer: before } } else { env.clone() };
                let m = self.query(subquery, &inner);
                let m = if matches!(self.g.nodes[m].kind, NodeKind::Table | NodeKind::Cte) {
                    m
                } else {
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
                self.missed = false;
                for e in array_exprs {
                    for parts in refs(e, self.kind).cols {
                        if let Some((src, col)) = self.resolve(&before, &[], &parts) {
                            self.edge(src, Some(col), m, Some(out.clone()), EdgeKind::Transform, None);
                        }
                    }
                }
                if std::mem::take(&mut self.missed) {
                    self.set_untraced(m, &out);
                }
                sources.push(Source { alias: alias_of(alias).unwrap_or_else(|| "unnest".into()), node: m });
            }
            TableFactor::Function { name, args, alias, .. } => {
                let m = self.function_node(name_parts(name).join("."), args, alias, &before, env, self.range(tf.span()));
                sources.push(Source { alias: alias_of(alias).unwrap_or_else(|| name_parts(name).last().cloned().unwrap_or_default().to_lowercase()), node: m });
            }
            TableFactor::TableFunction { expr, alias } => {
                let m = self.function_node("TABLE".into(), expr, alias, &before, env, self.range(tf.span()));
                sources.push(Source { alias: alias_of(alias).unwrap_or_default(), node: m });
            }
            TableFactor::JsonTable { json_expr, alias, .. } | TableFactor::OpenJsonTable { json_expr, alias, .. } => {
                let label = if matches!(tf, TableFactor::JsonTable { .. }) { "JSON_TABLE" } else { "OPENJSON" };
                let m = self.function_node(label.into(), json_expr, alias, &before, env, self.range(tf.span()));
                sources.push(Source { alias: alias_of(alias).unwrap_or_default(), node: m });
            }
            TableFactor::Pivot { table, aggregate_functions, value_column, alias, .. } => {
                // The table's other columns pass through; the aggregated ones
                // and the pivot column make the new columns.
                let mut inner = Vec::new();
                self.factor(table, env, &mut inner, n, lateral);
                let m = self.open_node("PIVOT", alias, self.range(tf.span()));
                for s in &inner {
                    self.edge(s.node, None, m, None, EdgeKind::Direct, None);
                }
                let exprs: Vec<&Expr> = aggregate_functions.iter().map(|a| &a.expr).chain(value_column.iter()).collect();
                self.missed = false;
                for e in exprs {
                    let r = refs(e, self.kind);
                    let kind = if r.aggregate { EdgeKind::Aggregate } else { EdgeKind::Transform };
                    for parts in &r.cols {
                        if let Some((src, col)) = self.resolve(&inner, &env.outer, parts) {
                            self.edge(src, Some(col), m, None, kind, None);
                        }
                    }
                }
                if std::mem::take(&mut self.missed) {
                    self.untraced_nodes.insert(m);
                }
                sources.push(Source { alias: alias_of(alias).unwrap_or_default(), node: m });
            }
            TableFactor::Unpivot { table, value, name, columns, alias, .. } => {
                // `UNPIVOT (val FOR col IN (a, b))`: the listed columns' values in `val`, their names in `col`.
                let mut inner = Vec::new();
                self.factor(table, env, &mut inner, n, lateral);
                let m = self.open_node("UNPIVOT", alias, self.range(tf.span()));
                for s in &inner {
                    self.edge(s.node, None, m, None, EdgeKind::Direct, None);
                }
                let value_cols: Vec<String> = match value {
                    Expr::Tuple(v) => v.iter().map(item_name).collect(),
                    other => vec![item_name(other)],
                };
                let outs: Vec<String> = value_cols.iter().map(|v| self.col_or_add(m, v)).collect();
                self.col_or_add(m, &name.value);
                self.missed = false;
                for c in columns {
                    for parts in refs(&c.expr, self.kind).cols {
                        if let Some((src, col)) = self.resolve(&inner, &env.outer, &parts) {
                            for o in &outs {
                                self.edge(src, Some(col.clone()), m, Some(o.clone()), EdgeKind::Direct, None);
                            }
                        }
                    }
                }
                if std::mem::take(&mut self.missed) {
                    for o in &outs {
                        self.set_untraced(m, o);
                    }
                }
                sources.push(Source { alias: alias_of(alias).unwrap_or_default(), node: m });
            }
            other => {
                // Anything else (`MATCH_RECOGNIZE`, `XMLTABLE`, …): its tables
                // feed it whole and every column is untraced.
                let label = short(other.to_string());
                let m = self.function_node(label, other, &None, &before, env, self.range(other.span()));
                let _ = sqlparser::ast::visit_relations(other, |r| {
                    let t = self.table_node(r);
                    self.edge(t, None, m, None, EdgeKind::Transform, None);
                    std::ops::ControlFlow::<()>::Continue(())
                });
                self.untraced_nodes.insert(m);
                for c in &mut self.g.nodes[m].columns {
                    c.untraced = true;
                }
                sources.push(Source { alias: String::new(), node: m });
            }
        }
    }

    /// A node whose columns are known only by the names the SQL uses (and
    /// those of its alias's column list).
    fn open_node(&mut self, label: &str, alias: &Option<TableAlias>, span: Option<Range>) -> usize {
        let name = alias.as_ref().map(|a| a.name.value.clone()).unwrap_or_else(|| label.to_string());
        let m = self.node(NodeKind::Subquery, name, span);
        match alias {
            Some(a) if !a.columns.is_empty() => {
                for c in &a.columns {
                    self.add_out(m, c.name.value.clone(), None, None);
                }
            }
            _ => self.g.nodes[m].columns_known = false,
        }
        m
    }

    /// A table function (`generate_series(…)`, `flatten(input => u.email)`,
    /// `TABLE(…)`, `JSON_TABLE(…)`): every column it reads from `before`
    /// feeds every column it returns.
    fn function_node<V: Visit + ?Sized>(
        &mut self,
        label: String,
        args: &V,
        alias: &Option<TableAlias>,
        before: &[Source],
        env: &Env,
        span: Option<Range>,
    ) -> usize {
        let r = refs_in(args, self.kind);
        let m = self.open_node(&label, alias, span);
        let kind = if r.aggregate { EdgeKind::Aggregate } else { EdgeKind::Transform };
        self.missed = false;
        let cols: Vec<String> = self.g.nodes[m].columns.iter().map(|c| c.name.clone()).collect();
        for parts in &r.cols {
            if let Some((src, col)) = self.resolve(before, &[], parts) {
                let s = self.range(Span::union_iter(parts.iter().map(|i| i.span)));
                if cols.is_empty() {
                    self.edge(src, Some(col.clone()), m, None, kind, s);
                }
                for c in &cols {
                    self.edge(src, Some(col.clone()), m, Some(c.clone()), kind, s);
                }
            }
        }
        if std::mem::take(&mut self.missed) {
            self.untraced_nodes.insert(m);
            for c in &mut self.g.nodes[m].columns {
                c.untraced = true;
            }
        }
        for q in &r.subqueries {
            let inner = Env { ctes: env.ctes.clone(), outer: before.to_vec() };
            let s = self.query(q, &inner);
            for c in self.g.nodes[s].columns.iter().map(|c| c.name.clone()).collect::<Vec<_>>() {
                self.edge(s, Some(c), m, None, EdgeKind::Transform, None);
            }
        }
        m
    }
}
