//! Column-level lineage of SQL scripts.
//!
//! Each statement is parsed (sqlparser, per dialect) and every output column
//! is traced through CTEs, subqueries, joins and set operations to the source
//! columns it comes from. Statements that write tables link the statements of
//! a script into one chain: a table written by one statement (`CREATE TABLE …
//! AS`, `CREATE VIEW`, `INSERT … SELECT`, `SELECT … INTO`, `UPDATE … SET … FROM`,
//! `MERGE … USING`, `DELETE … USING`, plain `CREATE TABLE`) is the same node a
//! later statement reads, and [`Lineage::flows`] sums the chain up table to
//! table (through the CTEs and subqueries between them).
//!
//! Table columns (for `*` and unqualified names) come from a [`Catalog`]
//! (DataBrain's explorer cache); names that can't be resolved are reported
//! as warnings, never guessed. Every node, column and edge carries the byte
//! range of the SQL it came from, so the UI can highlight it.

mod builder;
mod flows;
mod model;
mod refs;

#[cfg(test)]
mod tests;

pub use builder::analyze;
pub(crate) use flows::flows;
pub use model::*;
pub(crate) use refs::*;
