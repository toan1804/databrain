//! What a session may run. [`Session::execute`](crate::Session::execute)
//! (and `check_sql`, `estimate_cost`) take a [`Statement`], which comes
//! either from [`Checked`] text (user, AI or client SQL that was
//! classified, its parameters bound) or from [`Statement::internal`]
//! (SQL DataBrain writes itself). Text someone typed can't reach a
//! database without going through the classifier the safety checks read.

use std::collections::HashMap;

use serde::Serialize;

use super::{Classification, ParamValue, StatementKind, classify, substitute_parameters};
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
}

impl Checked {
    /// `sql` classified for `kind`, its parameters as typed.
    pub fn new(sql: impl Into<String>, kind: ConnectorKind) -> Self {
        let sql = sql.into();
        let classification = classify(&sql, kind);
        Self { sql, classification }
    }

    /// With the values of its `:name` parameters put in, classified again;
    /// the names without a value when some are missing.
    pub fn bind(self, kind: ConnectorKind, params: &HashMap<String, ParamValue>) -> Result<Self, Vec<String>> {
        let sql = substitute_parameters(&self.sql, kind, params)?;
        Ok(if sql == self.sql { self } else { Self::new(sql, kind) })
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
}
