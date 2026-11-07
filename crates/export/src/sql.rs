use databrain_connector_core::ConnectorKind;

use crate::kind::Kind;
use crate::load;

/// A value for `INSERT … VALUES`, written the way the dialect reads it
/// (same rules as job loads: `crate::load`).
pub(crate) fn sql_value(cell: Option<&str>, kind: Kind, dialect: ConnectorKind) -> String {
    let lit = match kind {
        Kind::Number => load::Lit::Number,
        Kind::Bool => load::Lit::Bool,
        Kind::Binary => load::Lit::Binary,
        Kind::Array { .. } | Kind::Other => load::Lit::Text,
    };
    load::literal(cell, lit, dialect)
}

#[cfg(test)]
mod tests {
    use crate::test_util::*;
    use crate::*;

    #[test]
    fn sql_and_markdown() {
        let s = run(ExportFormat::SqlInsert);
        assert!(s.starts_with("INSERT INTO \"public\".\"t\" (\"id\", \"name\", \"ok\", \"raw\") VALUES (1, 'a,\"b\"', TRUE, '\\x00ff'::bytea);"), "{s}");
        assert!(s.contains("(NULL, 'it''s|x', NULL, NULL);"));
        let m = run(ExportFormat::Markdown);
        assert!(m.contains("| id | name | ok | raw |\n| ---: | --- | --- | --- |"));
        assert!(m.contains("it's\\|x"));
    }
}
