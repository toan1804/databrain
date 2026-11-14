//! Query outputs: handles, names, versions, pinning and the memory budget
//! ([`registry`]), and outputs as DuckDB tables ([`duckdb`]). Comparing two
//! outputs is in [`crate::compare`].

pub mod duckdb;
pub mod registry;

pub use duckdb::*;
pub use registry::*;
