//! Jobs: a graph of SQL steps (nodes) with dependencies (edges), run in
//! order by the app, by hand or on a schedule, with a run history.
//!
//! A job is stored as one JSON document (nodes, edges, schedule), like
//! notebooks. Runs are rows of `job_runs` with each node's outcome.

mod export;
mod model;
mod node;
mod schedule;
mod store;

pub use export::*;
pub use model::*;
pub use node::*;
pub use schedule::*;

/// Run history kept per job.
pub const RUNS_KEPT: i64 = 200;
/// Runs per job that keep their full log (the rest keep their summary).
pub const RUN_LOGS_KEPT: i64 = 20;
/// A run whose runner hasn't reported for this long is taken as stopped.
pub const STALE_RUN_MS: i64 = 120_000;
