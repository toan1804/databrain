//! The corpus in `tests/corpus/`: statements that must never be a read, and
//! reads that must stay reads, per connector kind. Also: classifying a whole
//! text is never weaker than classifying the statements it splits into.

use databrain_connector_core::ConnectorKind;
use databrain_connector_core::sql::{StatementKind, classify, read_only_allows, split_statements};

fn corpus(text: &str) -> Vec<(ConnectorKind, String)> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| {
            let (k, sql) = l.split_once(" | ").unwrap_or_else(|| panic!("bad corpus line: {l}"));
            let kind: ConnectorKind = serde_json::from_str(&format!("\"{}\"", k.trim())).unwrap_or_else(|_| panic!("unknown kind {k}"));
            (kind, sql.replace("\\n", "\n"))
        })
        .collect()
}

fn never_read() -> Vec<(ConnectorKind, String)> {
    corpus(include_str!("corpus/never_read.txt"))
}

fn reads() -> Vec<(ConnectorKind, String)> {
    corpus(include_str!("corpus/reads.txt"))
}

/// The strictest kind among the statements a text splits into.
fn pieces_kind(sql: &str, kind: ConnectorKind) -> StatementKind {
    split_statements(sql, kind).iter().map(|s| classify(&s.sql, kind).kind).max_by_key(|k| rank(*k)).unwrap_or(StatementKind::Read)
}

/// How much a kind is trusted: reads run anywhere; session and transaction
/// control get no production prompt; everything else is refused on a
/// read-only connection and asked about on a production one, alike.
fn rank(k: StatementKind) -> u8 {
    match k {
        StatementKind::Read => 0,
        StatementKind::Session => 1,
        StatementKind::Transaction => 2,
        StatementKind::Dml | StatementKind::Ddl | StatementKind::Other | StatementKind::Unknown => 3,
    }
}

#[test]
fn never_read_statements_are_not_reads_and_read_only_refuses_them() {
    let mut bad = vec![];
    for (k, sql) in never_read() {
        // As a run splits it: no piece may be a read that a read-only connection runs.
        let worst = pieces_kind(&sql, k);
        let refused = split_statements(&sql, k).iter().any(|s| !read_only_allows(&s.sql, &classify(&s.sql, k)));
        if worst == StatementKind::Read || !refused {
            bad.push(format!("{k:?} | {sql:?} → {worst:?}, read-only refuses: {refused}"));
        }
    }
    assert!(bad.is_empty(), "classified as reads:\n{}", bad.join("\n"));
}

#[test]
fn reads_stay_reads() {
    let bad: Vec<String> = reads()
        .into_iter()
        .filter(|(k, sql)| classify(sql, *k).kind != StatementKind::Read)
        .map(|(k, sql)| format!("{k:?} | {sql} → {:?}", classify(&sql, k).kind))
        .collect();
    assert!(bad.is_empty(), "no longer reads:\n{}", bad.join("\n"));
}

/// Whole text vs its pieces: classifying the text as one statement (what a
/// caller that doesn't split sees) is never weaker than its strictest piece,
/// for every corpus entry and every pair of entries joined by `;` or a newline.
#[test]
fn a_whole_text_is_never_weaker_than_its_pieces() {
    let all: Vec<(ConnectorKind, String)> = never_read().into_iter().chain(reads()).collect();
    let mut bad = vec![];
    let mut check = |k: ConnectorKind, sql: &str| {
        let pieces = split_statements(sql, k);
        if pieces.len() < 2 {
            return;
        }
        let whole = classify(sql, k).kind;
        let worst = pieces_kind(sql, k);
        if rank(whole) < rank(worst) {
            bad.push(format!("{k:?} | {sql:?}: whole {whole:?} < piece {worst:?}"));
        }
    };
    for (k, sql) in &all {
        check(*k, sql);
    }
    for (k1, a) in &all {
        for (k2, b) in &all {
            if k1 == k2 {
                check(*k1, &format!("{a};\n{b}"));
                check(*k1, &format!("{a}\n{b}"));
            }
        }
    }
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}
