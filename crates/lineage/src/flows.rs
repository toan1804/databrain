use std::collections::HashMap;

use crate::*;

/// Ends of the chain: tables and statement results.
fn endpoint(n: &Node) -> bool {
    matches!(n.kind, NodeKind::Table | NodeKind::Result) && !n.name.is_empty()
}

fn strength(k: EdgeKind) -> u8 {
    match k {
        EdgeKind::Join => 0,
        EdgeKind::Filter => 1,
        EdgeKind::Direct => 2,
        EdgeKind::Transform => 3,
        EdgeKind::Aggregate => 4,
    }
}

fn is_rows(k: EdgeKind) -> bool {
    matches!(k, EdgeKind::Filter | EdgeKind::Join)
}

/// `path` then `e`: a filter/join anywhere makes the whole path one (the
/// first one met from the target); values keep the strongest kind.
fn combine(path: Option<EdgeKind>, e: EdgeKind) -> EdgeKind {
    match path {
        None => e,
        Some(p) if is_rows(p) => p,
        Some(_) if is_rows(e) => e,
        Some(p) => {
            if strength(e) > strength(p) {
                e
            } else {
                p
            }
        }
    }
}

/// Better of two kinds for the same pair: values beat filters/joins.
fn better(a: EdgeKind, b: EdgeKind) -> EdgeKind {
    match (is_rows(a), is_rows(b)) {
        (true, false) => b,
        (false, true) => a,
        _ if strength(b) > strength(a) => b,
        _ => a,
    }
}

/// A step of the walk back: node, column reached (`None`: the whole node),
/// kind so far, statements so far.
type Walk<'g> = (usize, Option<&'g str>, Option<EdgeKind>, Vec<usize>);

/// Walk back from every table/result through the nodes between them,
/// following only the columns that reach it (and the filters/joins on the
/// rows of the nodes on the way).
pub(crate) fn flows(g: &Lineage) -> Vec<TableFlow> {
    let at: HashMap<&str, usize> = g.nodes.iter().enumerate().map(|(i, n)| (n.id.as_str(), i)).collect();
    let mut into: HashMap<usize, Vec<&Edge>> = HashMap::new();
    for e in &g.edges {
        if let (Some(&f), Some(&t)) = (at.get(e.from.as_str()), at.get(e.to.as_str())) {
            if f != t {
                into.entry(t).or_default().push(e);
            }
        }
    }
    let mut out: Vec<TableFlow> = Vec::new();
    let mut index: HashMap<(usize, usize), usize> = HashMap::new();
    for (to, n) in g.nodes.iter().enumerate().filter(|(_, n)| endpoint(n)) {
        let mut stack: Vec<Walk> = vec![(to, None, None, n.statement.into_iter().collect())];
        let mut seen: std::collections::HashSet<(usize, Option<&str>, Option<EdgeKind>)> = std::collections::HashSet::new();
        while let Some((cur, col, kind, stmts)) = stack.pop() {
            for e in into.get(&cur).into_iter().flatten() {
                // Into the column we came for (any, for the whole node), or a
                // filter/join on the node's rows.
                let wanted = match (col, e.to_column.as_deref()) {
                    (_, None) | (None, _) => true,
                    (Some(c), Some(t)) => c == t,
                };
                if !wanted {
                    continue;
                }
                let from = at[e.from.as_str()];
                let k = combine(kind, e.kind);
                let mut st = stmts.clone();
                for s in e.statement.iter().chain(g.nodes[from].statement.iter()) {
                    if !st.contains(s) {
                        st.push(*s);
                    }
                }
                if endpoint(&g.nodes[from]) {
                    if from == to {
                        continue;
                    }
                    st.sort();
                    match index.get(&(from, to)) {
                        Some(&i) => {
                            let f = &mut out[i];
                            f.kind = better(f.kind, k);
                            for s in st {
                                if !f.statements.contains(&s) {
                                    f.statements.push(s);
                                }
                            }
                            f.statements.sort();
                        }
                        None => {
                            index.insert((from, to), out.len());
                            out.push(TableFlow { from: g.nodes[from].id.clone(), to: n.id.clone(), kind: k, statements: st });
                        }
                    }
                } else {
                    let c = e.from_column.as_deref();
                    if seen.insert((from, c, Some(k))) {
                        stack.push((from, c, Some(k), st));
                    }
                }
            }
        }
    }
    out
}
