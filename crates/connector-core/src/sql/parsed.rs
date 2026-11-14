//! One statement read once. DataBrain's splitter decides where statements
//! end (it reads strings and comments as each server does); each piece is
//! then tokenized and parsed by sqlparser once, and classification, the
//! read-only rule, the AI's function check and table references all work
//! from that one reading instead of parsing the text again each.

use sqlparser::ast::Statement;
use sqlparser::dialect::Dialect;
use sqlparser::parser::{Parser, ParserError};
use sqlparser::tokenizer::{Token, Tokenizer};

use super::{Classification, StatementSpan, classify, dialect_for, read_only, split_statements};
use crate::tables::{self, TableRefs};
use crate::types::ConnectorKind;

/// The parser's reading of a text: its tokens (whitespace and comments
/// included) and the statements they parse to, or why not.
#[derive(Debug, Clone)]
pub(crate) struct Parse {
    /// `None` when the text can't be tokenized (a string that never ends).
    pub(crate) tokens: Option<Vec<Token>>,
    pub(crate) statements: Result<Vec<Statement>, ParserError>,
}

impl Parse {
    /// Tokenized once; the parser reads those tokens (what
    /// `Parser::parse_sql` does, without tokenizing again).
    pub(crate) fn new(sql: &str, dialect: &dyn Dialect) -> Self {
        match Tokenizer::new(dialect, sql).tokenize_with_location() {
            Ok(located) => {
                let tokens = located.iter().map(|t| t.token.clone()).collect();
                let statements = Parser::new(dialect).with_tokens_with_locations(located).parse_statements();
                Self { tokens: Some(tokens), statements }
            }
            Err(e) => Self { tokens: None, statements: Err(e.into()) },
        }
    }

    pub(crate) fn of(sql: &str, kind: ConnectorKind) -> Self {
        Self::new(sql, dialect_for(kind).as_ref())
    }
}

/// A statement of a script, read once: its place in the script, the
/// parser's reading and its classification.
#[derive(Debug, Clone)]
pub struct ParsedStatement {
    span: StatementSpan,
    kind: ConnectorKind,
    /// `None` for query languages other than SQL.
    parse: Option<Parse>,
    classification: Classification,
}

impl ParsedStatement {
    /// `sql` as one statement.
    pub fn new(sql: &str, kind: ConnectorKind) -> Self {
        Self::of(StatementSpan { start: 0, end: sql.len(), sql: sql.to_string() }, kind)
    }

    fn of(span: StatementSpan, kind: ConnectorKind) -> Self {
        let parse = kind.traits().language.is_sql().then(|| Parse::of(&span.sql, kind));
        let classification = classify::classify_with(&span.sql, kind, parse.as_ref());
        Self { span, kind, parse, classification }
    }

    pub fn span(&self) -> &StatementSpan {
        &self.span
    }

    pub fn sql(&self) -> &str {
        &self.span.sql
    }

    pub fn classification(&self) -> &Classification {
        &self.classification
    }

    /// What the parser read, when it could.
    pub fn statements(&self) -> Option<&[Statement]> {
        self.parse.as_ref().and_then(|p| p.statements.as_deref().ok())
    }

    /// [`super::read_only_allows`] for this statement.
    pub fn read_only_allows(&self) -> bool {
        read_only::read_only_allows(&self.span.sql, &self.classification)
    }

    /// [`super::unknown_functions`] for this statement.
    pub fn unknown_functions(&self) -> Vec<String> {
        self.parse.as_ref().map(read_only::functions_in).unwrap_or_default()
    }

    /// The tables this statement reads and writes.
    pub fn table_refs(&self) -> TableRefs {
        let mut out = TableRefs::default();
        tables::collect_statement(self, &mut out);
        out
    }

    pub(crate) fn kind(&self) -> ConnectorKind {
        self.kind
    }
}

/// The statements of a script as DataBrain splits it, each read once.
pub fn parse_script(text: &str, kind: ConnectorKind) -> Vec<ParsedStatement> {
    split_statements(text, kind).into_iter().map(|s| ParsedStatement::of(s, kind)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::{StatementKind, read_only_allows, unknown_functions};

    /// One reading gives what the text functions give, piece by piece.
    #[test]
    fn one_reading_agrees_with_the_text_functions() {
        use ConnectorKind::*;
        for (kind, script) in [
            (Postgres, "select 1; update t set a = 1 where id = 2; set search_path = x; select my_fn(a) from t"),
            (Mysql, "SET NAMES utf8mb4; select 'a\\' into outfile ', 1 -- '; select * from t /*! into outfile 'x' */"),
            (Mssql, "select 1 shutdown\nGO\nexec sp_who; select next value for s"),
            (Snowflake, "select 1 ->> delete from t; use role admin"),
            (Duckdb, "select e'\\''; drop table t"),
            (Postgres, "selec * fro t; select 'unterminated"),
            (Mongodb, "db.orders.find({})"),
        ] {
            let parsed = parse_script(script, kind);
            assert!(!parsed.is_empty(), "{script}");
            for p in &parsed {
                assert_eq!(p.classification(), &crate::sql::classify(p.sql(), kind), "{kind:?}: {}", p.sql());
                assert_eq!(p.read_only_allows(), read_only_allows(p.sql(), &crate::sql::classify(p.sql(), kind)), "{}", p.sql());
                assert_eq!(p.unknown_functions(), unknown_functions(p.sql(), kind), "{}", p.sql());
            }
            let mut merged = TableRefs::default();
            for p in &parsed {
                tables::merge(&mut merged, p.table_refs());
            }
            assert_eq!(merged, tables::table_refs(script, kind), "{script}");
        }
        // The allowed and the refused case, from one reading.
        let ok = ParsedStatement::new("select * from t", ConnectorKind::Postgres);
        assert!(ok.read_only_allows() && ok.statements().is_some_and(|s| s.len() == 1));
        let refused = ParsedStatement::new("select pg_terminate_backend(1)", ConnectorKind::Postgres);
        assert!(!refused.read_only_allows() && refused.classification().kind != StatementKind::Read);
        let unparsed = ParsedStatement::new("selec * fro t", ConnectorKind::Postgres);
        assert!(unparsed.statements().is_none() && !unparsed.read_only_allows() && unparsed.unknown_functions() == ["(SQL that could not be parsed)"]);
    }
}
