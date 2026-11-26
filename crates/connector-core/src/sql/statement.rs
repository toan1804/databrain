//! What a session may run. [`Session::execute`](crate::Session::execute)
//! (and `check_sql`, `estimate_cost`) take a [`Statement`], which comes
//! either from [`Checked`] text (user, AI or client SQL that was
//! classified, its parameters bound) or from [`Statement::internal`]
//! (SQL DataBrain writes itself). Text someone typed can't reach a
//! database without going through the classifier the safety checks read.

use std::collections::HashMap;

use serde::Serialize;

use super::{Classification, ParamValue, StatementKind, classify};
use crate::types::ConnectorKind;

/// SQL handed to a session: made only by [`Checked::statement`] or
/// [`Statement::internal`].
#[derive(Debug, Clone, Copy)]
pub struct Statement<'a> {
    sql: &'a str,
}

impl<'a> Statement<'a> {
    /// SQL DataBrain writes itself: catalog and metadata queries,
    /// `BEGIN`/`COMMIT`, grid edits and loads (names and values quoted),
    /// `EXPLAIN` around a [`Checked`] statement. Never text a user, the AI
    /// or a client typed: that is a [`Checked`].
    pub const fn internal(sql: &'a str) -> Self {
        Self { sql }
    }

    pub fn sql(&self) -> &'a str {
        self.sql
    }
}

/// User, AI or client text that may run: classified for its database, and
/// classified again when its `:name` parameters are bound (the values are
/// part of the SQL then). The only way such text becomes a [`Statement`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ts_rs::TS)]
pub struct Checked {
    sql: String,
    classification: Classification,
    /// Bound from other text: it and where its values went, so a server's
    /// error position maps back to what was written.
    #[serde(skip)]
    #[ts(skip)]
    written: Option<(String, super::params::ParamMap)>,
}

impl Checked {
    /// `sql` classified for `kind`, its parameters as typed.
    pub fn new(sql: impl Into<String>, kind: ConnectorKind) -> Self {
        let sql = sql.into();
        let classification = classify(&sql, kind);
        Self { sql, classification, written: None }
    }

    /// With the values of its `:name` parameters put in, classified again;
    /// the names without a value when some are missing.
    pub fn bind(self, kind: ConnectorKind, params: &HashMap<String, ParamValue>) -> Result<Self, Vec<String>> {
        let (sql, map) = super::params::substitute_with_map(&self.sql, kind, params)?;
        Ok(if sql == self.sql { self } else { Self { written: Some((self.sql, map)), ..Self::new(sql, kind) } })
    }

    /// A server's 1-based character position in this text as one in the
    /// text as written (before its parameters were bound).
    pub fn written_position(&self, pos: u32) -> u32 {
        match &self.written {
            Some((w, map)) => super::params::written_position(w, &self.sql, map, pos),
            None => pos,
        }
    }

    pub fn sql(&self) -> &str {
        &self.sql
    }

    pub fn classification(&self) -> &Classification {
        &self.classification
    }

    pub fn kind(&self) -> StatementKind {
        self.classification.kind
    }

    /// The statement to run.
    pub fn statement(&self) -> Statement<'_> {
        Statement { sql: &self.sql }
    }

    /// A combined query ([`super::parse_combined`]), whose own text never
    /// runs: its parts run on their connections and the outer query in
    /// DuckDB, each checked there (`checked`). A read when every one of them
    /// is, so the tab's connection (where none of it runs) lets it through;
    /// `None` otherwise.
    pub fn combined<'c>(sql: impl Into<String>, checked: impl IntoIterator<Item = &'c Checked>) -> Option<Self> {
        let mut any = false;
        for c in checked {
            if c.kind() != StatementKind::Read {
                return None;
            }
            any = true;
        }
        any.then(|| Self {
            sql: sql.into(),
            classification: Classification { kind: StatementKind::Read, missing_where: false, keyword: "WITH".into() },
            written: None,
        })
    }

    /// As the table explorer runs it: Databricks' `DESCRIBE HISTORY` and
    /// `DESCRIBE DETAIL` of a table ([`super::delta_describe`]), which
    /// [`classify`] leaves unknown for the AI and MCP, are reads there
    /// (the user opened the table's History or Statistics).
    pub fn explorer_read(self, kind: ConnectorKind) -> Self {
        if kind == ConnectorKind::Databricks && self.classification.kind == StatementKind::Unknown && super::delta_describe(&self.sql) {
            Self { classification: Classification { kind: StatementKind::Read, ..self.classification }, ..self }
        } else {
            self
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checked_text_is_classified_and_bound() {
        let pg = ConnectorKind::Postgres;
        let c = Checked::new("select * from t where id = :id", pg);
        assert_eq!(c.kind(), StatementKind::Read);
        assert_eq!(c.statement().sql(), c.sql());
        let bound = c.clone().bind(pg, &HashMap::from([("id".to_string(), ParamValue { value: "7".into(), raw: false })])).unwrap();
        assert_eq!(bound.sql(), "select * from t where id = 7");
        assert_eq!(bound.kind(), StatementKind::Read);
        // A value that changes what runs is classified with it.
        let w = Checked::new("select :x", pg).bind(pg, &HashMap::from([("x".to_string(), ParamValue { value: "1; drop table t".into(), raw: true })]));
        assert_ne!(w.unwrap().kind(), StatementKind::Read);
        assert_eq!(c.bind(pg, &HashMap::new()).unwrap_err(), vec!["id".to_string()]);
        assert_eq!(Statement::internal("select 1").sql(), "select 1");
    }

    /// A combined query is a read only when every part (and the outer
    /// query) checked is one.
    #[test]
    fn a_combined_query_reads_only_when_every_piece_does() {
        let pg = ConnectorKind::Postgres;
        let (read, write) = (Checked::new("select 1", pg), Checked::new("delete from t", pg));
        let q = "with a as (@pg select 1) select * from a";
        assert_eq!(Checked::combined(q, [&read, &read]).unwrap().kind(), StatementKind::Read);
        assert!(Checked::combined(q, [&read, &write]).is_none());
        assert!(Checked::combined(q, []).is_none());
    }

    /// The table explorer's Delta reads: a read there, asked elsewhere.
    #[test]
    fn delta_describe_is_a_read_for_the_explorer_only() {
        let db = ConnectorKind::Databricks;
        let h = Checked::new("DESCRIBE HISTORY `main`.`s`.`t` LIMIT 50", db);
        assert_eq!(h.kind(), StatementKind::Unknown);
        assert_eq!(h.explorer_read(db).kind(), StatementKind::Read);
        assert_ne!(Checked::new("DESCRIBE HISTORY t LIMIT 5 DROP TABLE t", db).explorer_read(db).kind(), StatementKind::Read);
        assert_eq!(Checked::new("drop table t", db).explorer_read(db).kind(), StatementKind::Ddl);
        let pg = ConnectorKind::Postgres;
        assert_ne!(Checked::new("DESCRIBE HISTORY t", pg).explorer_read(pg).kind(), StatementKind::Read);
    }
}
