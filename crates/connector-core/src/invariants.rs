//! Properties of the statement splitter, the classifier and identifier
//! quoting that must hold for any input. They are what keeps the read/write
//! checks (read-only connections, the AI policy) from being bypassed, so
//! they are checked on generated inputs: by a seeded test on every
//! `cargo test`, and by the cargo-fuzz targets in `fuzz/` for longer runs.
//!
//! Each function panics with the input when a property fails.

use crate::sql::{classify, split_statements};
use crate::{ConnectorKind, quote_ident, quote_path, split_path};

/// Splitting and classifying `text` as `kind`:
/// - spans are in order, don't overlap, and are the text they cover;
/// - a statement that isn't a read is still not a read with a comment
///   before it or whitespace around it (comments must not hide a keyword).
pub fn check_split_classify(text: &str, kind: ConnectorKind) {
    let spans = split_statements(text, kind);
    let mut prev_end = 0;
    for s in &spans {
        assert!(s.start >= prev_end && s.start <= s.end && s.end <= text.len(), "{kind:?}: span {}..{} out of order in {text:?}", s.start, s.end);
        assert_eq!(text.get(s.start..s.end), Some(s.sql.as_str()), "{kind:?}: span text differs in {text:?}");
        prev_end = s.end;
    }
    if !kind.traits().language.is_sql() {
        for s in &spans {
            classify(&s.sql, kind);
        }
        return;
    }
    for s in &spans {
        let c = classify(&s.sql, kind);
        if c.kind.is_read() {
            continue;
        }
        for variant in [format!("/* note */ {}", s.sql), format!("\n  {}  \n", s.sql), format!("-- note\n{}", s.sql)] {
            let v = classify(&variant, kind);
            assert!(!v.kind.is_read(), "{kind:?}: {:?} is {:?} but {variant:?} is a read", s.sql, c.kind);
        }
    }
}

/// Quoting `ident` as one identifier and splitting it back gives `ident`
/// (a name can't close its quotes and add SQL), and quoting a path twice
/// changes nothing.
pub fn check_quoting(ident: &str, kind: ConnectorKind) {
    if !kind.traits().language.is_sql() || ident.is_empty() || ident.contains('\0') {
        return;
    }
    let q = quote_ident(kind, ident);
    // BigQuery-style whole-path quotes split a quoted name on its dots.
    if !(kind.traits().whole_path_quotes && ident.contains('.')) {
        assert_eq!(split_path(kind, &q), vec![ident.to_string()], "{kind:?}: {ident:?} quoted as {q}");
    }
    let once = quote_path(kind, &q);
    assert_eq!(quote_path(kind, &once), once, "{kind:?}: quoting {q} twice changes it");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pieces the generator joins: keywords, quotes, comment markers,
    /// dialect delimiters and escapes, the things past bypasses were made of.
    const PIECES: &[&str] = &[
        "SELECT ",
        "select",
        "1",
        " FROM t",
        " WHERE x=1",
        "INSERT INTO t VALUES (1)",
        "UPDATE t SET a=1",
        "DELETE FROM t",
        "DROP TABLE t",
        "EXEC xp_cmdshell 'x'",
        "EXPLAIN ",
        "ANALYZE ",
        "(",
        ")",
        "WITH c AS (",
        "CALL p()",
        "COPY t TO 'f'",
        "BEGIN",
        "END",
        "$$",
        "$a$",
        ";",
        "\n",
        " ",
        "\t",
        "'",
        "\"",
        "`",
        "[",
        "]",
        "\\",
        "--",
        "/*",
        "*/",
        "#",
        "\nGO\n",
        "\n/\n",
        "DELIMITER //\n",
        "//",
        "é",
        "\u{2028}",
        "db.update({})",
        "MATCH (n) DELETE n",
        "SET search_path=x",
        "GRANT ALL",
        "e'\\''",
        "N'x'",
        "''",
        "`a``b`",
        "{\"$out\":\"c\"}",
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
    fn split_and_classify_hold_on_generated_scripts() {
        let mut g = Gen(0x9E37_79B9_7F4A_7C15);
        for _ in 0..3000 {
            let text = g.text(14);
            for kind in ConnectorKind::ALL {
                check_split_classify(&text, kind);
            }
        }
    }

    #[test]
    fn quoting_holds_on_generated_names() {
        let mut g = Gen(0xD1B5_4A32_D192_ED03);
        for _ in 0..3000 {
            let name = g.text(5);
            for kind in ConnectorKind::ALL {
                check_quoting(&name, kind);
            }
        }
    }
}
