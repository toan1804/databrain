//! Properties of the lineage that must hold for any input. PII masking
//! reads this graph, so a panic or a dangling edge is a masking failure: they
//! are checked on generated scripts by a seeded test on every `cargo test`,
//! and by the cargo-fuzz target `lineage` in `fuzz/` for longer runs.
//!
//! Each function panics with the input when a property fails.

use std::collections::HashSet;

use databrain_connector_core::ConnectorKind;

use crate::{NoCatalog, Range, analyze, fingerprint, join_pairs};

/// Tracing `text` as `kind`:
/// - doesn't panic (nor does `join_pairs` or `fingerprint`);
/// - node ids are unique, and every edge and flow joins two of them;
/// - every range is inside the text, on character boundaries;
/// - statements are numbered in order.
pub fn check_lineage(text: &str, kind: ConnectorKind) {
    let g = analyze(text, kind, &NoCatalog);
    let _ = join_pairs(text, kind, &NoCatalog);
    let _ = fingerprint(text, kind);
    let ids: HashSet<&str> = g.nodes.iter().map(|n| n.id.as_str()).collect();
    assert_eq!(ids.len(), g.nodes.len(), "{kind:?}: node ids repeat in {text:?}");
    for e in &g.edges {
        assert!(ids.contains(e.from.as_str()) && ids.contains(e.to.as_str()), "{kind:?}: edge {} -> {} to no node in {text:?}", e.from, e.to);
    }
    for f in &g.flows {
        assert!(ids.contains(f.from.as_str()) && ids.contains(f.to.as_str()), "{kind:?}: flow {} -> {} to no node in {text:?}", f.from, f.to);
    }
    let ranges = g
        .nodes
        .iter()
        .flat_map(|n| n.span.iter().chain(n.columns.iter().filter_map(|c| c.span.as_ref())))
        .chain(g.edges.iter().filter_map(|e| e.span.as_ref()))
        .chain(g.warnings.iter().filter_map(|w| w.span.as_ref()))
        .chain(g.statements.iter().flat_map(|s| std::iter::once(&s.range).chain(s.query.as_ref())));
    for r in ranges {
        let Range { start, end } = *r;
        assert!(
            start <= end && end <= text.len() && text.is_char_boundary(start) && text.is_char_boundary(end),
            "{kind:?}: range {start}..{end} outside {text:?}"
        );
    }
    for w in g.statements.windows(2) {
        assert!(w[0].index <= w[1].index, "{kind:?}: statements out of order in {text:?}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pieces of SQL that exercise the tracer: sources, star options,
    /// table functions, column lists, session statements, odd names.
    const PIECES: &[&str] = &[
        "select ",
        "* ",
        "a, ",
        "u.email ",
        "email ",
        "from ",
        "users u ",
        "orders o ",
        " join ",
        " on u.id = o.uid ",
        " where ",
        " having ",
        " qualify ",
        " group by 1 ",
        "(",
        ")",
        ", ",
        ";",
        " union all ",
        "with c(a, b) as (select 1, 2) ",
        "with recursive r(n) as (select 1 union all select n + 1 from r) ",
        "lateral flatten(input => u.email) f ",
        "lateral string_to_table(email, '@') as g(v) ",
        "unnest(tags) as t(x) ",
        " unpivot (v for k in (a, b)) p ",
        " pivot (sum(a) for b in (1, 2)) p ",
        "* replace (email as name) ",
        "* exclude (a) ",
        "* rename (a as b) ",
        " lateral view explode(split(email, '@')) t as tag ",
        " cross apply (select u.email as e) x ",
        "list_transform(tags, x -> x + 1) ",
        "insert into t ",
        "create table x as ",
        "drop table x",
        "use s",
        "set search_path to s, public",
        "begin",
        "commit",
        "\"Q\".",
        "'s'",
        "-1",
        "level ",
        " start with p is null connect by prior id = p ",
        "values (1), (2) ",
        "update t set a = b from s ",
        "merge into t using s on t.id = s.id when matched then update set a = s.a ",
        "é",
        "--",
        "/*",
        "*/",
    ];

    /// A small deterministic generator (no extra crates): xorshift.
    struct Gen(u64);
    impl Gen {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn text(&mut self, max_pieces: u64) -> String {
            (0..self.next() % max_pieces).map(|_| PIECES[(self.next() % PIECES.len() as u64) as usize]).collect()
        }
    }

    #[test]
    fn lineage_holds_on_generated_scripts() {
        let mut g = Gen(0x2545_F491_4F6C_DD1D);
        for _ in 0..1500 {
            let text = g.text(12);
            for kind in ConnectorKind::ALL.into_iter().filter(|k| k.traits().language.is_sql()) {
                check_lineage(&text, kind);
            }
        }
    }
}
