use databrain_connector_core::ConnectorKind;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
#[ts(export)]
#[serde(rename_all = "snake_case")]
pub enum ExportFormat {
    Csv,
    Tsv,
    Json,
    Ndjson,
    Markdown,
    SqlInsert,
    /// Binary formats: written to files only (see [`export_file`]).
    Parquet,
    Xlsx,
}

impl ExportFormat {
    pub fn is_binary(self) -> bool {
        matches!(self, ExportFormat::Parquet | ExportFormat::Xlsx)
    }

    pub fn extension(self) -> &'static str {
        match self {
            ExportFormat::Parquet => "parquet",
            ExportFormat::Xlsx => "xlsx",
            ExportFormat::Csv => "csv",
            ExportFormat::Tsv => "tsv",
            ExportFormat::Json => "json",
            ExportFormat::Ndjson => "ndjson",
            ExportFormat::Markdown => "md",
            ExportFormat::SqlInsert => "sql",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, ts_rs::TS)]
#[ts(export)]
#[ts(optional_fields = nullable)]
pub struct ExportOptions {
    pub format: ExportFormat,
    /// Write a header row (CSV/TSV).
    #[serde(default = "yes")]
    pub header: bool,
    /// Table name for SQL INSERT statements.
    #[serde(default)]
    pub table_name: Option<String>,
    /// Identifier quoting dialect for SQL INSERT.
    #[serde(default)]
    pub dialect: Option<ConnectorKind>,
    /// Restrict to these column indices (in order). Empty = all.
    #[serde(default)]
    #[ts(as = "Option<Vec<usize>>", optional)]
    pub columns: Vec<usize>,
    /// CSV/TSV for spreadsheets: text starting with `=`, `+`, `-`, `@`
    /// (a formula when opened in Excel) gets a leading `'`. Numbers, also
    /// negative ones, are left alone.
    #[serde(default)]
    #[ts(as = "Option<bool>", optional)]
    pub excel_safe: bool,
}

fn yes() -> bool {
    true
}

impl ExportOptions {
    pub fn new(format: ExportFormat) -> Self {
        Self { format, header: true, table_name: None, dialect: None, columns: Vec::new(), excel_safe: false }
    }
}
