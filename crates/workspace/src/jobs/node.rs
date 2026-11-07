use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, ts_rs::TS)]
#[ts(export)]
#[serde(rename_all = "snake_case")]
pub enum JobNodeKind {
    /// Run `sql` on `connection_id`; the result is the output `name`.
    #[default]
    Query,
    /// Run `sql` on DuckDB (it reads upstream outputs as `results.<name>`)
    /// and write the rows into `target_table` of `target_connection_id`.
    Load,
    /// Run `sql` on DuckDB and write the rows to a file in `export_folder`
    /// (DuckDB `COPY … TO`).
    Export,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, ts_rs::TS)]
#[ts(export)]
#[serde(rename_all = "snake_case")]
pub enum FileFormat {
    #[default]
    Csv,
    Parquet,
    /// One JSON object per line.
    Json,
}

impl FileFormat {
    pub fn extension(self) -> &'static str {
        match self {
            FileFormat::Csv => "csv",
            FileFormat::Parquet => "parquet",
            FileFormat::Json => "json",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, ts_rs::TS)]
#[ts(export)]
#[serde(rename_all = "snake_case")]
pub enum LoadMode {
    /// Insert the rows. A missing table is created from the rows' columns
    /// when `create_table` is on.
    #[default]
    Append,
    /// Delete the table's rows, then insert (missing table: as `Append`).
    Truncate,
    /// Drop the table (if it exists), create it from the rows' columns, insert.
    Replace,
    /// Update rows of an existing table whose `key_columns` match; rows
    /// without a match are ignored.
    Update,
    /// Update rows whose `key_columns` match and insert the others (upsert);
    /// the table must exist.
    Merge,
    /// Make the table hold exactly the rows by key: Merge, then delete the
    /// table's rows whose key isn't among the rows. The table must exist.
    Sync,
}

impl LoadMode {
    /// Modes matching rows by `key_columns`.
    pub fn needs_keys(self) -> bool {
        matches!(self, LoadMode::Update | LoadMode::Merge | LoadMode::Sync)
    }
    /// Modes writing into a table that must already exist.
    pub fn needs_table(self) -> bool {
        self.needs_keys()
    }
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default, ts_rs::TS)]
#[ts(export)]
#[ts(optional_fields = nullable)]
pub struct NodeRunSummary {
    #[serde(default)]
    #[ts(type = "\"pending\" | \"running\" | \"success\" | \"error\" | \"skipped\" | \"cancelled\" | \"\"")]
    pub status: String,
    #[serde(default)]
    #[ts(as = "f64")]
    pub finished_at: i64,
    #[serde(default)]
    #[ts(as = "f64")]
    pub duration_ms: i64,
    #[serde(default)]
    #[ts(as = "Option<f64>")]
    pub rows: Option<i64>,
    #[serde(default)]
    pub error: Option<String>,
    /// File written by an export step.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub file: Option<String>,
    /// Things the user should know (a load step created its table, …).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[ts(as = "Option<Vec<String>>", optional)]
    pub notices: Vec<String>,
    /// While running: rows written so far out of the total (load steps).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub progress: Option<StepProgress>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default, ts_rs::TS)]
#[ts(export)]
pub struct StepProgress {
    /// What is being done ("Inserting rows", "Staging rows").
    pub phase: String,
    #[ts(as = "f64")]
    pub done: u64,
    #[ts(as = "f64")]
    pub total: u64,
}

/// One line of a run's log, for tracing what ran and what came back.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default, ts_rs::TS)]
#[ts(export)]
#[ts(optional_fields = nullable)]
pub struct RunLogEntry {
    /// Order in the run (from 1).
    #[ts(as = "f64")]
    pub seq: u64,
    /// Epoch ms.
    #[ts(as = "f64")]
    pub at: i64,
    /// Step it belongs to (`None` for the run itself).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub node_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub step: Option<String>,
    /// `info`, `success`, `warning`, `error`.
    #[ts(type = "\"info\" | \"success\" | \"warning\" | \"error\"")]
    pub level: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub sql: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(as = "Option<f64>")]
    pub rows: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(as = "Option<f64>")]
    pub duration_ms: Option<i64>,
}

/// Lines kept per run log (the rest is summarised in one line).
pub const LOG_KEPT: usize = 5000;
/// Longest SQL text kept per log line.
pub const LOG_SQL_CHARS: usize = 8000;

/// `sql` cut to [`LOG_SQL_CHARS`].
pub fn log_sql(sql: &str) -> String {
    // Passwords and keys (`IDENTIFIED BY …`) are not kept.
    let redacted = databrain_connector_core::sql::redact_text(sql);
    let sql = redacted.as_str();
    if sql.len() <= LOG_SQL_CHARS {
        return sql.to_string();
    }
    let mut end = LOG_SQL_CHARS;
    while !sql.is_char_boundary(end) {
        end -= 1;
    }
    format!("{} … ({} more characters)", &sql[..end], sql.len() - end)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ts_rs::TS)]
#[ts(export)]
#[ts(optional_fields = nullable)]
pub struct JobNode {
    pub id: String,
    /// Output name: downstream DuckDB steps read it as `results.<name>`.
    pub name: String,
    #[serde(default)]
    pub kind: JobNodeKind,
    /// Where `sql` runs (Query). Load steps run on the Results DuckDB.
    #[serde(default)]
    pub connection_id: Option<String>,
    #[serde(default)]
    pub sql: String,
    #[serde(default)]
    pub x: f64,
    #[serde(default)]
    pub y: f64,
    #[serde(default)]
    pub target_connection_id: Option<String>,
    #[serde(default)]
    pub target_table: Option<String>,
    #[serde(default)]
    pub load_mode: LoadMode,
    /// Load: SQL run on the target connection before the rows are written
    /// (one or more statements).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub load_before_sql: Option<String>,
    /// Load: SQL run on the target connection after the rows are written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub load_after_sql: Option<String>,
    /// Load (Update/Merge): columns identifying a row.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[ts(as = "Option<Vec<String>>", optional)]
    pub key_columns: Vec<String>,
    /// Load (Append/Truncate): create a missing table from the rows' columns.
    #[serde(default = "yes")]
    #[ts(as = "Option<bool>", optional)]
    pub create_table: bool,
    /// Load: rows per INSERT statement (`None` = the connection's default;
    /// kept within its maximum).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch_rows: Option<u32>,
    /// Load: most KB of SQL text per INSERT statement (`None` = default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch_kb: Option<u32>,
    /// Export: folder of the file (empty = Downloads).
    #[serde(default)]
    pub export_folder: Option<String>,
    /// Export: file name, `{step}`, `{job}`, `{date}`, `{time}` replaced
    /// (empty = `{step}_{date}_{time}`); the extension is added when missing.
    #[serde(default)]
    pub export_file: Option<String>,
    #[serde(default)]
    #[ts(as = "Option<FileFormat>", optional)]
    pub export_format: FileFormat,
    /// The step's SQL (and a load's before/after SQL) was written by the AI
    /// assistant or an MCP client. Such a step runs as the AI's SQL whoever
    /// starts the run (by hand, on the schedule, from the command line): on
    /// DuckDB the AI sandbox applies. Set when the AI creates or changes the
    /// step; a step the user changes becomes the user's, and the user can
    /// clear it ("Trust this SQL"), never set it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    #[ts(as = "Option<bool>", optional)]
    pub ai_sql: bool,
    #[serde(default)]
    pub last_run: Option<NodeRunSummary>,
}

impl JobNode {
    /// Whether `other` runs the same SQL in the same place (what decides
    /// who the SQL belongs to, see [`JobNode::ai_sql`]).
    pub fn same_sql(&self, other: &JobNode) -> bool {
        self.kind == other.kind
            && self.sql.trim() == other.sql.trim()
            && self.connection_id == other.connection_id
            && self.target_connection_id == other.target_connection_id
            && self.load_before_sql.as_deref().map(str::trim) == other.load_before_sql.as_deref().map(str::trim)
            && self.load_after_sql.as_deref().map(str::trim) == other.load_after_sql.as_deref().map(str::trim)
    }

    /// A step does one thing: drop the settings of the other actions, so a
    /// query step can't also carry a load target or a file name.
    pub fn keep_own_settings(&mut self) {
        let load = self.kind == JobNodeKind::Load;
        let export = self.kind == JobNodeKind::Export;
        if self.kind != JobNodeKind::Query {
            // Load and export steps select their rows on DuckDB.
            self.connection_id = None;
        }
        if !load {
            self.target_connection_id = None;
            self.target_table = None;
            self.load_mode = LoadMode::default();
            self.load_before_sql = None;
            self.load_after_sql = None;
            self.key_columns.clear();
            self.create_table = true;
            self.batch_rows = None;
            self.batch_kb = None;
        } else if !self.load_mode.needs_keys() {
            self.key_columns.clear();
        }
        if !export {
            self.export_folder = None;
            self.export_file = None;
            self.export_format = FileFormat::default();
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
#[ts(export)]
pub struct JobEdge {
    pub from: String,
    pub to: String,
}
