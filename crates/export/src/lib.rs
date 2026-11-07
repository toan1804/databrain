//! Streaming exporters from Arrow batches to text formats.
//!
//! Values use the same display formatting as the grid, so an export matches
//! what the user sees. JSON keeps numbers and booleans typed.

pub mod load;

mod delimited;
mod error;
mod exporter;
mod file;
mod json;
mod kind;
mod markdown;
mod options;
mod snapshot;
mod sql;
#[cfg(test)]
mod test_util;
mod xlsx;

pub use error::{ExportError, Result};
pub use exporter::{Exporter, export, export_to_string};
pub use file::{FileWriter, XLSX_MAX_ROWS, export_file};
pub use options::{ExportFormat, ExportOptions};
pub use snapshot::{read_snapshot, write_snapshot};
