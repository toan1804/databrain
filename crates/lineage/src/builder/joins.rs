//! Equi-join keys between database tables, for learning how tables join.
//!
//! While the lineage is built, every `a.x = b.y` conjunct of a join
//! condition (`ON`, `USING`, `MERGE … ON`) or of a `WHERE` that links two
//! different sources is recorded with the source each side came from. When
//! the script is done, each side is traced back through CTEs and subqueries
//! along `Direct` edges to a database table column; anything else (computed
//! values, unions, tables the script creates, table functions, ambiguous or
//! unknown columns) drops the whole condition.

use std::collections::HashMap;

use sqlparser::ast::{BinaryOperator, Expr, Ident, JoinOperator};

use super::*;
use crate::*;

/// One side of a recorded equality: the source it was found in and the
/// column of that source's node.
#[derive(Clone)]
pub(super) struct Side {
    /// Position of the source in scope (inner sources first, then outer):
    /// orders the two sides as written.
    pos: usize,
    node: usize,
    col: String,
}

pub(super) struct RawJoin {
    /// One condition (one `ON`, one `WHERE`): its equalities between the
    /// same two sources form one (possibly composite) key.
    clause: usize,
    stmt: usize,
    kind: JoinKind,
    left: Side,
    right: Side,
}

/// Kind of a join operator; `None` when it has no condition to learn from.
pub(super) fn join_kind(op: &JoinOperator) -> Option<JoinKind> {
    Some(match op {
        JoinOperator::Left(_) | JoinOperator::LeftOuter(_) => JoinKind::Left,
        JoinOperator::Right(_) | JoinOperator::RightOuter(_) => JoinKind::Right,
        JoinOperator::FullOuter(_) => JoinKind::Full,
        JoinOperator::Semi(_) | JoinOperator::LeftSemi(_) | JoinOperator::RightSemi(_) => JoinKind::Semi,
        JoinOperator::Anti(_) | JoinOperator::LeftAnti(_) | JoinOperator::RightAnti(_) => JoinKind::Anti,
        JoinOperator::Join(_) | JoinOperator::Inner(_) | JoinOperator::StraightJoin(_) | JoinOperator::CrossJoin(_) | JoinOperator::AsOf { .. } => {
            JoinKind::Inner
        }
        _ => return None,
    })
}

/// Top-level `AND` conjuncts of `e` (parentheses unwrapped). An `OR` is one
/// conjunct, which is never an equality, so nothing under it is learned.
fn conjuncts<'e>(e: &'e Expr, out: &mut Vec<&'e Expr>) {
    match e {
        Expr::BinaryOp { left, op: BinaryOperator::And, right } => {
            conjuncts(left, out);
            conjuncts(right, out);
        }
        Expr::Nested(x) => conjuncts(x, out),
        other => out.push(other),
    }
}

/// A plain column reference, through parentheses and casts (`o.id::text`).
fn column_ref(e: &Expr) -> Option<Vec<Ident>> {
    match e {
        Expr::Identifier(i) => Some(vec![i.clone()]),
        Expr::CompoundIdentifier(v) => Some(v.clone()),
        Expr::Nested(x) => column_ref(x),
        Expr::Cast { expr, .. } => column_ref(expr),
        _ => None,
    }
}

impl<'a> Builder<'a> {
    /// Record the equalities of `e` between two different sources.
    pub(super) fn equi_joins(&mut self, e: &Expr, kind: JoinKind, sources: &[Source], outer: &[Source]) {
        // A view's definition is not the user's SQL.
        if self.in_view > 0 {
            return;
        }
        self.clause += 1;
        let mut conj = Vec::new();
        conjuncts(e, &mut conj);
        for c in conj {
            let Expr::BinaryOp { left, op: BinaryOperator::Eq, right } = c else { continue };
            let (Some(l), Some(r)) = (column_ref(left), column_ref(right)) else { continue };
            let (Some(a), Some(b)) = (self.join_side(sources, outer, &l), self.join_side(sources, outer, &r)) else { continue };
            // `a.x = a.y`: a filter on one source, not a join.
            if a.pos == b.pos {
                continue;
            }
            let (left, right) = if a.pos < b.pos { (a, b) } else { (b, a) };
            self.joins.push(RawJoin { clause: self.clause, stmt: self.stmt, kind, left, right });
        }
    }

    /// `USING (a, b)`: each column on both sides of the join.
    pub(super) fn using_joins(&mut self, cols: &[Vec<Ident>], kind: JoinKind, left: &[Source], right: &[Source], outer: &[Source]) {
        if self.in_view > 0 {
            return;
        }
        self.clause += 1;
        for c in cols {
            let (Some(a), Some(mut b)) = (self.join_side(left, outer, c), self.join_side(right, outer, c)) else { continue };
            b.pos += left.len();
            self.joins.push(RawJoin { clause: self.clause, stmt: self.stmt, kind, left: a, right: b });
        }
    }

    fn join_side(&mut self, sources: &[Source], outer: &[Source], parts: &[Ident]) -> Option<Side> {
        let col = parts.last()?.value.clone();
        // An unqualified column more than one source has: ambiguous.
        if parts.len() == 1 && sources.iter().filter(|s| self.col_index(s.node, &col).is_some()).count() > 1 {
            return None;
        }
        let (src, node, name) = self.resolve_source(sources, outer, parts)?;
        // `x.field` read as struct access on a column `x`: not this column.
        if !name.eq_ignore_ascii_case(&col) {
            return None;
        }
        let pos = sources
            .iter()
            .position(|s| s.alias == src.alias && s.node == src.node)
            .or_else(|| outer.iter().position(|s| s.alias == src.alias && s.node == src.node).map(|i| sources.len() + i))?;
        Some(Side { pos, node, col: name })
    }

    /// The database table column `node.col` is, through `Direct` edges only.
    fn base_column(&self, mut node: usize, mut col: String, into: &HashMap<(&str, &str), Vec<&Edge>>, ids: &HashMap<&str, usize>) -> Option<(usize, String)> {
        for _ in 0..32 {
            let n = &self.g.nodes[node];
            if n.kind == NodeKind::Table {
                let database_table = !self.created.contains(&node) && self.tables.values().any(|&i| i == node);
                return database_table.then_some((node, col));
            }
            let [e] = into.get(&(n.id.as_str(), col.as_str()))?.as_slice() else { return None };
            if e.kind != EdgeKind::Direct {
                return None;
            }
            node = *ids.get(e.from.as_str())?;
            col = e.from_column.clone()?;
        }
        None
    }

    /// The recorded equalities as join pairs between database tables.
    pub(super) fn join_pairs(&self) -> Vec<JoinPair> {
        let ids: HashMap<&str, usize> = self.g.nodes.iter().enumerate().map(|(i, n)| (n.id.as_str(), i)).collect();
        let mut into: HashMap<(&str, &str), Vec<&Edge>> = HashMap::new();
        for e in &self.g.edges {
            if let Some(c) = &e.to_column {
                into.entry((e.to.as_str(), c.as_str())).or_default().push(e);
            }
        }
        // Group by condition and the two sources: one key per group.
        let mut groups: Vec<Vec<&RawJoin>> = Vec::new();
        for j in &self.joins {
            let same = |g: &&mut Vec<&RawJoin>| (g[0].clause, g[0].left.pos, g[0].right.pos) == (j.clause, j.left.pos, j.right.pos);
            match groups.iter_mut().find(same) {
                Some(g) => g.push(j),
                None => groups.push(vec![j]),
            }
        }
        let mut out: Vec<JoinPair> = Vec::new();
        'group: for g in groups {
            let mut tables: Option<(usize, usize)> = None;
            let mut cols: Vec<(String, String)> = Vec::new();
            for j in &g {
                let (Some((ln, lc)), Some((rn, rc))) =
                    (self.base_column(j.left.node, j.left.col.clone(), &into, &ids), self.base_column(j.right.node, j.right.col.clone(), &into, &ids))
                else {
                    // Part of a composite key can't be placed: the rest alone would be a wrong key.
                    continue 'group;
                };
                match tables {
                    None => tables = Some((ln, rn)),
                    Some(t) if t == (ln, rn) => {}
                    // The equalities lead to different tables (a CTE mixing two): not one key.
                    Some(_) => continue 'group,
                }
                if !cols.contains(&(lc.clone(), rc.clone())) {
                    cols.push((lc, rc));
                }
            }
            let Some((ln, rn)) = tables else { continue };
            // A table on its own key (`o2.id = o.id` in an EXISTS, dedupe
            // self-joins): says nothing about how tables relate.
            if ln == rn && cols.iter().all(|(l, r)| l.eq_ignore_ascii_case(r)) {
                continue;
            }
            let pair = JoinPair {
                left: self.g.nodes[ln].name.clone(),
                left_columns: cols.iter().map(|(l, _)| l.clone()).collect(),
                right: self.g.nodes[rn].name.clone(),
                right_columns: cols.iter().map(|(_, r)| r.clone()).collect(),
                kind: g[0].kind,
                statement: g[0].stmt,
            };
            if !out.contains(&pair) {
                out.push(pair);
            }
        }
        out
    }
}
