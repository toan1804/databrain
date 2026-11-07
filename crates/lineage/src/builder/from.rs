use sqlparser::ast::{Ident, Join, JoinConstraint, JoinOperator, TableAlias, TableFactor, TableWithJoins};

use super::*;
use crate::*;

impl<'a> Builder<'a> {
    pub(super) fn add_from(&mut self, twj: &TableWithJoins, env: &Env, sources: &mut Vec<Source>, n: usize) {
        self.factor(&twj.relation, env, sources, n);
        self.add_joins(&twj.joins, env, sources, n);
    }

    pub(super) fn add_joins(&mut self, joins: &[Join], env: &Env, sources: &mut Vec<Source>, n: usize) {
        for j in joins {
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

    pub(super) fn factor(&mut self, tf: &TableFactor, env: &Env, sources: &mut Vec<Source>, n: usize) {
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
                    let t = self.table_node(name);
                    self.expand_view(t, &parts);
                    t
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
}
