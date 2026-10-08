//! Job load steps: write rows into a table of a connection, with SQL run
//! before and after, in one of the [`LoadMode`]s, and a dry run that checks
//! every statement without keeping any change.
//!
//! - The target table is probed first (`SELECT * … WHERE 1 = 0`): it tells
//!   whether it exists and its column names. The rows' columns are matched
//!   to them by name (ignoring case), so `id` loads into Oracle's `ID`.
//! - Append/Truncate create a missing table from the rows' columns when
//!   asked to (`CREATE TABLE IF NOT EXISTS`, so before SQL may create it)
//!   and report it as a notice. Update/Merge need an existing table: rows
//!   go into a staging table with the target's own types, then one
//!   `MERGE` (SQL Server, Oracle, Snowflake, Databricks, BigQuery) or
//!   `UPDATE … FROM`/`JOIN` + `INSERT … WHERE NOT EXISTS` (others).
//! - Postgres, SQLite, DuckDB and SQL Server run the whole step (before SQL
//!   included) in one transaction; MySQL too when nothing in it is
//!   permanent DDL (which commits there). Elsewhere statements commit one
//!   by one.
//! - INSERT text is built one statement at a time while the rows are sent.
//! - Dry run: where the transaction covers everything, the step really runs
//!   on sample rows and is rolled back; elsewhere each statement is checked
//!   with `EXPLAIN` when the engine has it, the rest is listed as unchecked.

use std::sync::Arc;

use databrain_connector_core::arrow::array::RecordBatch;
use databrain_connector_core::arrow::datatypes::{Field, Schema, SchemaRef};
use databrain_connector_core::sql::{StatementKind, classify, split_statements};
use databrain_connector_core::{CancellationToken, ConnectorKind, ErrorKind, ExecOptions, Session};
use databrain_export::load as sqlgen;
use databrain_workspace::LoadMode;
use serde::Serialize;

use crate::{EngineError, QueryEngine, Result};

/// What a load step writes and how.
#[derive(Debug, Clone, Default)]
pub struct LoadSpec {
    pub table: String,
    pub mode: LoadMode,
    pub key_columns: Vec<String>,
    /// Append/Truncate: create a missing table.
    pub create_table: bool,
    pub before_sql: Option<String>,
    pub after_sql: Option<String>,
    /// Rows / KB per INSERT (`None` = the dialect's default).
    pub batch_rows: Option<u32>,
    pub batch_kb: Option<u32>,
}

/// Outcome of a load.
#[derive(Debug, Clone, Default, Serialize)]
pub struct LoadReport {
    /// Rows inserted (Append/Truncate/Replace), or updated/merged as the
    /// database reports them (the staged rows when it doesn't).
    pub rows: u64,
    pub created_table: bool,
    pub transaction: bool,
    pub notices: Vec<String>,
}

/// One statement (or group of INSERTs) of a dry run.
#[derive(Debug, Clone, Serialize)]
pub struct LoadCheck {
    pub label: String,
    pub sql: String,
    /// `ok`, `error`, `warning`, `unchecked`.
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// Dry run report of a load step.
#[derive(Debug, Clone, Default, Serialize)]
pub struct LoadDryRun {
    /// No statement failed.
    pub ok: bool,
    /// `transaction`: everything ran on the sample rows and was rolled back.
    /// `explain`: statements were checked one by one without running.
    /// `none`: the step could not be planned (see `checks`).
    pub method: String,
    pub connection: String,
    pub table: String,
    pub table_exists: Option<bool>,
    pub creates_table: bool,
    /// Rows the check used (a sample of the step's rows).
    pub sample_rows: Option<u64>,
    /// Why the step's rows could not be selected (upstream not run yet, …).
    pub rows_error: Option<String>,
    pub checks: Vec<LoadCheck>,
    pub notices: Vec<String>,
    /// INSERT size the run uses (after the connection's maximums).
    pub batch: Option<sqlgen::BatchSize>,
}

/// What a load is doing, for the job's run log and progress bar.
#[derive(Debug, Clone)]
pub enum LoadEvent {
    /// Something to know (the plan, a created table, the transaction).
    Info(String),
    /// One statement finished (before/after SQL, DDL, update/merge…).
    Statement { label: String, sql: String, rows: Option<u64>, duration_ms: u64, error: Option<String> },
    /// Rows written so far by a group of INSERTs (sent after each statement).
    Progress { phase: String, done: u64, total: u64 },
    /// A group of INSERTs finished (or failed: `error`, `sql` = the failing one).
    Inserted { label: String, table: String, rows: u64, statements: usize, sql: String, duration_ms: u64, error: Option<String> },
}

/// Receives [`LoadEvent`]s while a load runs.
pub trait LoadObserver: Send + Sync {
    fn event(&self, e: LoadEvent);
}

/// Observer that ignores everything.
pub struct NoLoadObserver;
impl LoadObserver for NoLoadObserver {
    fn event(&self, _: LoadEvent) {}
}

/// Rows to load.
pub struct LoadRows {
    pub schema: SchemaRef,
    pub batches: Vec<RecordBatch>,
}

impl LoadRows {
    pub fn count(&self) -> u64 {
        self.batches.iter().map(|b| b.num_rows() as u64).sum()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActKind {
    /// Before/after SQL of the user.
    User,
    /// DDL this step adds (create/drop the target).
    Ddl,
    /// Staging table create/drop (temporary on most engines).
    Stage,
    /// Update/merge from the staging table: its affected rows are the step's rows.
    Write,
    /// Other DML (delete all rows).
    Dml,
}

#[derive(Debug, Clone)]
enum Act {
    Sql { label: String, sql: String, kind: ActKind },
    /// Insert the rows into `table`; `target` = the step's table (not staging).
    Insert { label: String, table: String, target: bool },
    /// Can't be built (no rows to plan with).
    Skip { label: String, reason: String },
}

impl Act {
    fn label(&self) -> &str {
        match self {
            Act::Sql { label, .. } | Act::Insert { label, .. } | Act::Skip { label, .. } => label,
        }
    }
}

fn mode_text(m: LoadMode) -> &'static str {
    match m {
        LoadMode::Append => "insert",
        LoadMode::Truncate => "delete rows, then insert",
        LoadMode::Replace => "replace table",
        LoadMode::Update => "update by key",
        LoadMode::Merge => "merge by key",
    }
}

/// [`exec`], reported to `obs` as one statement.
async fn timed(session: &Arc<dyn Session>, label: &str, sql: &str, cancel: &CancellationToken, obs: &dyn LoadObserver) -> Result<Option<u64>> {
    let t = std::time::Instant::now();
    let r = exec(session, sql, cancel).await;
    obs.event(LoadEvent::Statement {
        label: label.into(),
        sql: sql.into(),
        rows: r.as_ref().ok().copied().flatten(),
        duration_ms: t.elapsed().as_millis() as u64,
        error: r.as_ref().err().map(|e| e.message.clone()),
    });
    r
}

struct Tx {
    begin: &'static str,
    commit: &'static str,
    rollback: &'static str,
}

struct Prepared {
    dialect: ConnectorKind,
    connection: String,
    table: String,
    session: Arc<dyn Session>,
    exists: Option<bool>,
    creates: bool,
    plan: Vec<Act>,
    /// Rows' schema with the target's column names.
    schema: Option<SchemaRef>,
    tx: Option<Tx>,
    batch: sqlgen::BatchSize,
    /// Staging table that is a real table (dropped after a failure too).
    permanent_stage: Option<String>,
    notices: Vec<String>,
}

/// Longest SQL text kept per dry run check.
const CHECK_SQL_CHARS: usize = 4000;

fn short(sql: &str) -> String {
    if sql.chars().count() <= CHECK_SQL_CHARS {
        return sql.to_string();
    }
    let mut s: String = sql.chars().take(CHECK_SQL_CHARS).collect();
    s.push_str(" …");
    s
}

fn engine_name(k: ConnectorKind) -> &'static str {
    use ConnectorKind as K;
    match k {
        K::Postgres => "PostgreSQL",
        K::Mysql => "MySQL",
        K::Mssql => "SQL Server",
        K::Oracle => "Oracle",
        K::Sqlite => "SQLite",
        K::Duckdb => "DuckDB",
        K::Snowflake => "Snowflake",
        K::Databricks => "Databricks",
        K::Bigquery => "BigQuery",
        #[allow(unreachable_patterns)]
        _ => "this database",
    }
}

/// Statements of before/after SQL.
fn user_statements(sql: Option<&str>, dialect: ConnectorKind) -> Vec<String> {
    sql.map(|s| split_statements(s, dialect).into_iter().map(|st| st.sql.trim().to_string()).filter(|s| !s.is_empty()).collect()).unwrap_or_default()
}

/// Transaction statements when the whole plan can run in one.
fn transaction(dialect: ConnectorKind, plan: &[Act]) -> Option<Tx> {
    use ConnectorKind as K;
    match dialect {
        K::Postgres | K::Sqlite | K::Duckdb => Some(Tx { begin: "BEGIN", commit: "COMMIT", rollback: "ROLLBACK" }),
        K::Mssql => Some(Tx { begin: "BEGIN TRANSACTION", commit: "COMMIT TRANSACTION", rollback: "IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION" }),
        K::Mysql => {
            // DDL commits implicitly in MySQL (temporary tables don't).
            let ddl = plan.iter().any(|a| match a {
                Act::Sql { kind: ActKind::Ddl, .. } => true,
                Act::Sql { kind: ActKind::User, sql, .. } => classify(sql, dialect).kind == StatementKind::Ddl,
                _ => false,
            });
            (!ddl).then_some(Tx { begin: "START TRANSACTION", commit: "COMMIT", rollback: "ROLLBACK" })
        }
        _ => None,
    }
}

/// Run one statement; rows affected when the driver reports them.
async fn exec(session: &Arc<dyn Session>, sql: &str, cancel: &CancellationToken) -> Result<Option<u64>> {
    if cancel.is_cancelled() {
        return Err(EngineError::new("cancelled", "Cancelled"));
    }
    let opts = ExecOptions { cancel: cancel.clone(), ..Default::default() };
    let c = session.execute(sql, opts).await?.collect().await?;
    Ok(c.summary.rows_affected)
}

fn labelled(label: &str, e: EngineError) -> EngineError {
    if e.kind == "cancelled" {
        return e;
    }
    EngineError { message: format!("{label}: {}", e.message), ..e }
}

/// `name` of the rows → the target's column, by exact name then ignoring case.
fn match_column<'a>(name: &str, target: &'a [String]) -> Option<&'a String> {
    target.iter().find(|c| *c == name).or_else(|| target.iter().find(|c| c.eq_ignore_ascii_case(name)))
}

impl QueryEngine {
    /// Rows of an output (all of them), for a load.
    pub fn output_rows(&self, reference: &str) -> Result<LoadRows> {
        let o = self.outputs.ensure_loaded(reference)?;
        self.result_rows(&o.result_id)
    }

    /// Rows of a result in the store.
    pub fn result_rows(&self, result_id: &str) -> Result<LoadRows> {
        let rs = self.results.get(result_id)?;
        let mut rs = rs.lock();
        let schema = rs.schema();
        let batches = rs.view_batches(&databrain_result_store::ViewSpec::default(), 65_536)?;
        Ok(LoadRows { schema, batches })
    }

    /// Copy the rows of an output into a table of another connection (job
    /// load steps), with the step's before/after SQL, on the session `tab`.
    pub async fn write_output(&self, reference: &str, connection_id: &str, tab: &str, spec: &LoadSpec, cancel: &CancellationToken, obs: &dyn LoadObserver) -> Result<LoadReport> {
        let rows = self.output_rows(reference)?;
        self.load_rows(&rows, connection_id, tab, spec, cancel, obs).await
    }

    /// Write `rows` as `spec` says.
    pub async fn load_rows(&self, rows: &LoadRows, connection_id: &str, tab: &str, spec: &LoadSpec, cancel: &CancellationToken, obs: &dyn LoadObserver) -> Result<LoadReport> {
        let p = self.prepare_load(connection_id, tab, spec, Some(rows), false).await?;
        obs.event(LoadEvent::Info(format!(
            "Writing {} rows into {} on {} ({}, INSERTs of up to {} rows / {} KB): {}",
            rows.count(),
            p.table,
            p.connection,
            mode_text(spec.mode),
            p.batch.rows,
            p.batch.bytes / 1024,
            p.plan.iter().map(Act::label).collect::<Vec<_>>().join(" → ")
        )));
        for n in &p.notices {
            obs.event(LoadEvent::Info(n.clone()));
        }
        if let Some(tx) = &p.tx {
            timed(&p.session, "Start the transaction", tx.begin, cancel, obs).await.map_err(|e| labelled("Start the transaction", e))?;
        }
        let result = async {
            let n = run_plan(&p, rows, cancel, None, obs).await?;
            if let Some(tx) = &p.tx {
                timed(&p.session, "Commit", tx.commit, cancel, obs).await.map_err(|e| labelled("Commit", e))?;
            }
            Ok::<u64, EngineError>(n)
        }
        .await;
        match result {
            Ok(n) => {
                let mut notices = p.notices.clone();
                if p.creates {
                    let c = created_notice(&p, rows);
                    obs.event(LoadEvent::Info(c.clone()));
                    notices.push(c);
                }
                Ok(LoadReport { rows: n, created_table: p.creates, transaction: p.tx.is_some(), notices })
            }
            Err(e) => {
                if p.tx.is_some() && e.kind != "cancelled" {
                    obs.event(LoadEvent::Info("Rolling back the transaction: nothing is kept".into()));
                }
                self.abandon(&p, connection_id, tab).await;
                Err(match &p.tx {
                    Some(_) if e.kind != "cancelled" => EngineError { message: format!("{} (rolled back: nothing was changed)", e.message), ..e },
                    Some(_) => e,
                    None => EngineError {
                        message: format!("{} ({} commits each statement: the statements before this one were kept)", e.message, engine_name(p.dialect)),
                        ..e
                    },
                })
            }
        }
    }

    /// After a failure: roll back, drop a real staging table, and close the
    /// session so its state can't leak into the next run.
    async fn abandon(&self, p: &Prepared, connection_id: &str, tab: &str) {
        let fresh = CancellationToken::new();
        let wait = std::time::Duration::from_secs(10);
        if let Some(tx) = &p.tx {
            let _ = tokio::time::timeout(wait, exec(&p.session, tx.rollback, &fresh)).await;
        }
        if let Some(stage) = &p.permanent_stage {
            let _ = tokio::time::timeout(wait, exec(&p.session, &sqlgen::drop_stage_sql(p.dialect, stage), &fresh)).await;
        }
        self.drop_session(connection_id, tab);
    }

    /// Check a load step without keeping any change. `rows`: a sample of the
    /// step's rows (`None` with `rows_error` when they couldn't be selected).
    pub async fn dry_run_load(&self, rows: Option<&LoadRows>, rows_error: Option<String>, connection_id: &str, tab: &str, spec: &LoadSpec) -> LoadDryRun {
        let mut report = LoadDryRun {
            method: "none".into(),
            table: spec.table.trim().to_string(),
            sample_rows: rows.map(LoadRows::count),
            rows_error,
            connection: self.workspace.get_connection(connection_id).map(|p| p.name).unwrap_or_default(),
            ..Default::default()
        };
        let cancel = CancellationToken::new();
        let p = match self.prepare_load(connection_id, tab, spec, rows, true).await {
            Ok(p) => p,
            Err(e) => {
                report.checks.push(LoadCheck { label: "Plan".into(), sql: String::new(), status: "error".into(), message: Some(e.message) });
                self.drop_session(connection_id, tab);
                return report;
            }
        };
        report.table_exists = p.exists;
        report.creates_table = p.creates;
        report.notices = p.notices.clone();
        report.batch = Some(p.batch);
        let empty = LoadRows { schema: Arc::new(Schema::empty()), batches: vec![] };
        let data = rows.unwrap_or(&empty);
        let mut checks = Vec::new();
        match &p.tx {
            Some(tx) => {
                report.method = "transaction".into();
                match exec(&p.session, tx.begin, &cancel).await {
                    Ok(_) => {
                        let _ = run_plan(&p, data, &cancel, Some(&mut checks), &NoLoadObserver).await;
                        if let Err(e) = exec(&p.session, tx.rollback, &cancel).await {
                            checks.push(LoadCheck { label: "Roll back".into(), sql: tx.rollback.into(), status: "error".into(), message: Some(e.message) });
                        }
                    }
                    Err(e) => checks.push(LoadCheck { label: "Start the transaction".into(), sql: tx.begin.into(), status: "error".into(), message: Some(e.message) }),
                }
            }
            None => {
                report.method = "explain".into();
                explain_plan(&p, data, &mut checks).await;
            }
        }
        report.ok = !checks.iter().any(|c| c.status == "error") && report.rows_error.is_none();
        report.checks = checks;
        // Nothing of the check stays around (temporary tables, locks).
        self.drop_session(connection_id, tab);
        report
    }

    /// Validate the spec, probe the table and build the statements.
    async fn prepare_load(&self, connection_id: &str, tab: &str, spec: &LoadSpec, rows: Option<&LoadRows>, dry: bool) -> Result<Prepared> {
        let table = spec.table.trim().to_string();
        if table.is_empty() {
            return Err(EngineError::new("invalid", "Choose the table to write into"));
        }
        let profile = self.workspace.get_connection(connection_id)?;
        if profile.config.read_only {
            return Err(EngineError::new("invalid", format!("Connection \"{}\" is read-only; nothing was written.", profile.name)));
        }
        let dialect = profile.config.kind;
        let keys_in: Vec<String> = spec.key_columns.iter().map(|k| k.trim().to_string()).filter(|k| !k.is_empty()).collect();
        if spec.mode.needs_keys() && keys_in.is_empty() {
            return Err(EngineError::new("invalid", "Choose the key columns that identify a row (Update and Merge match rows by them)"));
        }
        let session = self.session(connection_id, tab).await?;

        // Does the table exist, and with which columns?
        let (exists, target_cols, missing_reason) = match session.execute(&sqlgen::probe_sql(dialect, &table), ExecOptions::default()).await {
            Ok(s) => match s.collect().await {
                Ok(c) => (true, c.schema.map(|s| s.fields().iter().map(|f| f.name().clone()).collect::<Vec<_>>()), None),
                Err(e) if matches!(e.kind, ErrorKind::Connection | ErrorKind::Auth | ErrorKind::Cancelled) => return Err(e.into()),
                Err(e) => (false, None, Some(e.message)),
            },
            Err(e) if matches!(e.kind, ErrorKind::Connection | ErrorKind::Auth | ErrorKind::Cancelled) => return Err(e.into()),
            Err(e) => (false, None, Some(e.message)),
        };
        let before = user_statements(spec.before_sql.as_deref(), dialect);
        let after = user_statements(spec.after_sql.as_deref(), dialect);
        let mut notices = Vec::new();
        let conn = profile.name.clone();
        let reason = || missing_reason.clone().map(|r| format!(" ({r})")).unwrap_or_default();

        // Rows' columns named as in the target (when it exists and isn't recreated).
        let keep_names = matches!(spec.mode, LoadMode::Replace) || !exists;
        let schema: Option<SchemaRef> = match (rows, &target_cols) {
            (Some(r), Some(cols)) if !keep_names => {
                let mut missing = Vec::new();
                let fields: Vec<Field> = r
                    .schema
                    .fields()
                    .iter()
                    .map(|f| match match_column(f.name(), cols) {
                        Some(c) => f.as_ref().clone().with_name(c.clone()),
                        None => {
                            missing.push(f.name().clone());
                            f.as_ref().clone()
                        }
                    })
                    .collect();
                if !missing.is_empty() {
                    return Err(EngineError::new(
                        "invalid",
                        format!("{table} in {conn} has no column {} (its columns: {}). Rename them in the step's SQL, or use Replace to recreate the table.", missing.join(", "), cols.join(", ")),
                    ));
                }
                Some(Arc::new(Schema::new(fields)))
            }
            (Some(r), _) => Some(r.schema.clone()),
            (None, _) => None,
        };
        let row_cols: Option<Vec<String>> = schema.as_ref().map(|s| s.fields().iter().map(|f| f.name().clone()).collect());

        // Key columns, named as in the rows (target names when matched).
        let keys: Vec<String> = if spec.mode.needs_keys() {
            let pool: Option<&Vec<String>> = row_cols.as_ref().or(target_cols.as_ref());
            match pool {
                Some(pool) => {
                    let mut out = Vec::new();
                    for k in &keys_in {
                        match match_column(k, pool) {
                            Some(c) => out.push(c.clone()),
                            None => {
                                return Err(EngineError::new("invalid", format!("Key column {k} is not among the {} columns: {}", if row_cols.is_some() { "rows'" } else { "table's" }, pool.join(", "))));
                            }
                        }
                    }
                    out
                }
                None => keys_in.clone(),
            }
        } else {
            vec![]
        };

        let mut plan = Vec::new();
        for (i, s) in before.iter().enumerate() {
            plan.push(Act::Sql { label: format!("Before SQL {}", i + 1), sql: s.clone(), kind: ActKind::User });
        }
        let mut creates = false;
        let mut permanent_stage = None;
        let no_rows = |label: &str| Act::Skip { label: label.into(), reason: "Needs the step's rows, which couldn't be selected".into() };
        match spec.mode {
            LoadMode::Append | LoadMode::Truncate => {
                if !exists {
                    if !spec.create_table {
                        if before.is_empty() {
                            return Err(EngineError::new("invalid", format!("Table {table} doesn't exist in {conn}{}. Turn on \"Create the table if it's missing\", or create it first.", reason())));
                        }
                        notices.push(format!("{table} doesn't exist in {conn} yet: the before SQL must create it."));
                    } else {
                        creates = true;
                        notices.push(format!("{table} doesn't exist in {conn}: {} creates it from the rows' columns.", if dry { "the run" } else { "this run" }));
                        plan.push(match &schema {
                            Some(s) => Act::Sql { label: "Create table".into(), sql: sqlgen::create_table_if_missing_sql(dialect, &table, s), kind: ActKind::Ddl },
                            None => no_rows("Create table"),
                        });
                    }
                }
                if spec.mode == LoadMode::Truncate && !creates {
                    plan.push(Act::Sql { label: "Delete all rows".into(), sql: sqlgen::delete_all_sql(dialect, &table), kind: ActKind::Dml });
                }
                plan.push(match &schema {
                    Some(_) => Act::Insert { label: "Insert rows".into(), table: table.clone(), target: true },
                    None => no_rows("Insert rows"),
                });
            }
            LoadMode::Replace => {
                plan.push(Act::Sql { label: "Drop table".into(), sql: sqlgen::drop_table_sql(dialect, &table), kind: ActKind::Ddl });
                match &schema {
                    Some(s) => {
                        plan.push(Act::Sql { label: "Create table".into(), sql: sqlgen::create_table_sql(dialect, &table, s), kind: ActKind::Ddl });
                        plan.push(Act::Insert { label: "Insert rows".into(), table: table.clone(), target: true });
                    }
                    None => plan.push(no_rows("Create table and insert rows")),
                }
            }
            LoadMode::Update | LoadMode::Merge => {
                let what = if spec.mode == LoadMode::Update { "Update" } else { "Merge" };
                if !exists {
                    if before.is_empty() {
                        return Err(EngineError::new("invalid", format!("{what} needs an existing table: {table} doesn't exist in {conn}{}.", reason())));
                    }
                    notices.push(format!("{table} doesn't exist in {conn} yet: the before SQL must create it ({what} doesn't create tables)."));
                }
                match &row_cols {
                    Some(cols) => {
                        if spec.mode == LoadMode::Update && cols.iter().all(|c| keys.contains(c)) {
                            return Err(EngineError::new("invalid", "Update needs at least one column besides the key columns"));
                        }
                        let suffix = uuid::Uuid::new_v4().simple().to_string()[..12].to_string();
                        let stage = sqlgen::stage_table_name(dialect, &table, &suffix);
                        if !sqlgen::stage_is_temporary(dialect) {
                            permanent_stage = Some(stage.clone());
                        }
                        plan.push(Act::Sql { label: "Create staging table".into(), sql: sqlgen::create_stage_sql(dialect, &stage, &table, cols), kind: ActKind::Stage });
                        plan.push(Act::Insert { label: "Stage rows".into(), table: stage.clone(), target: false });
                        let writes = if spec.mode == LoadMode::Update {
                            sqlgen::update_from_stage_sql(dialect, &table, &stage, cols, &keys)
                        } else {
                            sqlgen::merge_from_stage_sql(dialect, &table, &stage, cols, &keys)
                        };
                        let n = writes.len();
                        for (i, sql) in writes.into_iter().enumerate() {
                            let label = match (spec.mode, n, i) {
                                (LoadMode::Update, _, _) => "Update matching rows".to_string(),
                                (_, 1, _) => "Merge rows".to_string(),
                                (_, _, 0) => "Update matching rows".to_string(),
                                _ => "Insert new rows".to_string(),
                            };
                            plan.push(Act::Sql { label, sql, kind: ActKind::Write });
                        }
                        plan.push(Act::Sql { label: "Drop staging table".into(), sql: sqlgen::drop_stage_sql(dialect, &stage), kind: ActKind::Stage });
                    }
                    None => plan.push(no_rows(&format!("{what} rows"))),
                }
            }
        }
        for (i, s) in after.iter().enumerate() {
            plan.push(Act::Sql { label: format!("After SQL {}", i + 1), sql: s.clone(), kind: ActKind::User });
        }
        let (batch, lowered) = sqlgen::batch_limits(dialect).resolve(spec.batch_rows, spec.batch_kb);
        notices.extend(lowered);
        let tx = transaction(dialect, &plan);
        if tx.is_none() {
            notices.push(match dialect {
                ConnectorKind::Mysql => "MySQL commits DDL at once, so this step doesn't run in a transaction: if it fails partway, what ran before stays.".to_string(),
                d => format!("{} doesn't run this step in a transaction: if it fails partway, what ran before stays.", engine_name(d)),
            });
        }
        Ok(Prepared { dialect, connection: conn, table, session, exists: Some(exists), creates, plan, schema, tx, batch, permanent_stage, notices })
    }
}

fn created_notice(p: &Prepared, rows: &LoadRows) -> String {
    let cols: Vec<String> = rows.schema.fields().iter().take(12).map(|f| format!("{} {}", f.name(), sqlgen::column_type(p.dialect, f))).collect();
    let more = rows.schema.fields().len().saturating_sub(12);
    format!(
        "Created table {} in {} from the rows' columns: {}{}",
        p.table,
        p.connection,
        cols.join(", "),
        if more > 0 { format!(", … ({more} more)") } else { String::new() }
    )
}

/// Run the plan; rows written. With `record`, each step is recorded and the
/// run stops at the first error (dry run).
async fn run_plan(p: &Prepared, rows: &LoadRows, cancel: &CancellationToken, mut record: Option<&mut Vec<LoadCheck>>, obs: &dyn LoadObserver) -> Result<u64> {
    let mut inserted = 0u64;
    let mut staged = 0u64;
    let mut affected: Option<u64> = None;
    let push = |record: &mut Option<&mut Vec<LoadCheck>>, label: &str, sql: &str, r: &Result<()>, note: Option<String>| {
        if let Some(rec) = record.as_deref_mut() {
            rec.push(LoadCheck {
                label: label.into(),
                sql: short(sql),
                status: if r.is_ok() { "ok" } else { "error" }.into(),
                message: match r {
                    Ok(()) => note,
                    Err(e) => Some(e.message.clone()),
                },
            });
        }
    };
    for act in &p.plan {
        match act {
            Act::Sql { label, sql, kind } => {
                let r = timed(&p.session, label, sql, cancel, obs).await;
                if let (Ok(Some(n)), ActKind::Write) = (&r, kind) {
                    affected = Some(affected.unwrap_or(0) + n);
                }
                let r = r.map(|_| ());
                push(&mut record, label, sql, &r, None);
                r.map_err(|e| labelled(label, e))?;
            }
            Act::Insert { label, table, target } => {
                let schema = p.schema.clone().unwrap_or_else(|| rows.schema.clone());
                let b = sqlgen::InsertSql::new(p.dialect, table, &schema, p.batch);
                let (mut n, mut statements, mut first) = (0u64, 0usize, String::new());
                let mut r: Result<()> = Ok(());
                let total = rows.count();
                let started = std::time::Instant::now();
                let phase = if *target { "Inserting rows" } else { "Staging rows" };
                obs.event(LoadEvent::Progress { phase: phase.into(), done: 0, total });
                'rows: for batch in &rows.batches {
                    // Batches carry the rows' names; the statement uses the target's.
                    let mut at = 0;
                    loop {
                        let next = match b.next(batch, at, None) {
                            Ok(x) => x,
                            Err(e) => {
                                r = Err(EngineError::new("internal", e.to_string()));
                                break 'rows;
                            }
                        };
                        let Some((sql, k)) = next else { break };
                        if first.is_empty() {
                            first = sql.clone();
                        }
                        if let Err(e) = exec(&p.session, &sql, cancel).await {
                            first = sql;
                            r = Err(e);
                            break 'rows;
                        }
                        at += k;
                        n += k as u64;
                        statements += 1;
                        obs.event(LoadEvent::Progress { phase: phase.into(), done: n, total });
                    }
                }
                obs.event(LoadEvent::Inserted {
                    label: label.clone(),
                    table: table.clone(),
                    rows: n,
                    statements,
                    sql: first.clone(),
                    duration_ms: started.elapsed().as_millis() as u64,
                    error: r.as_ref().err().map(|e| e.message.clone()),
                });
                let note = Some(format!("{n} rows in {statements} statement{}", if statements == 1 { "" } else { "s" }));
                push(&mut record, label, &first, &r, if r.is_ok() { note } else { None });
                r.map_err(|e| labelled(&format!("{label} (after {n} rows)"), e))?;
                if *target {
                    inserted += n;
                } else {
                    staged += n;
                }
            }
            Act::Skip { label, reason } => {
                if let Some(rec) = record.as_deref_mut() {
                    rec.push(LoadCheck { label: label.clone(), sql: String::new(), status: "unchecked".into(), message: Some(reason.clone()) });
                }
            }
        }
    }
    Ok(if staged > 0 || p.plan.iter().any(|a| matches!(a, Act::Insert { target: false, .. })) { affected.unwrap_or(staged) } else { inserted })
}

/// Dry run without a transaction: `EXPLAIN` each statement the engine can
/// check; nothing runs.
async fn explain_plan(p: &Prepared, rows: &LoadRows, checks: &mut Vec<LoadCheck>) {
    let engine = engine_name(p.dialect);
    // Tables created by earlier statements of the plan don't exist yet.
    let mut created_before = false;
    for act in &p.plan {
        let (label, sql, ours_on_new) = match act {
            Act::Skip { label, reason } => {
                checks.push(LoadCheck { label: label.clone(), sql: String::new(), status: "unchecked".into(), message: Some(reason.clone()) });
                continue;
            }
            Act::Sql { label, sql, kind } => (label.clone(), sql.clone(), *kind != ActKind::User && created_before && *kind != ActKind::Ddl),
            Act::Insert { label, table, target } => {
                let schema = p.schema.clone().unwrap_or_else(|| rows.schema.clone());
                let b = sqlgen::InsertSql::new(p.dialect, table, &schema, p.batch);
                let first = rows.batches.iter().find(|b| b.num_rows() > 0).and_then(|batch| b.next(batch, 0, Some(1)).ok().flatten()).map(|(s, _)| s);
                let Some(sql) = first else {
                    checks.push(LoadCheck { label: label.clone(), sql: String::new(), status: "ok".into(), message: Some("No rows to insert".into()) });
                    continue;
                };
                (label.clone(), sql, created_before || (!*target) || p.creates)
            }
        };
        let kind = classify(&sql, p.dialect).kind;
        let is_ddl = kind == StatementKind::Ddl || matches!(act, Act::Sql { kind: ActKind::Ddl | ActKind::Stage, .. });
        let mut c = LoadCheck { label, sql: short(&sql), status: "unchecked".into(), message: None };
        if is_ddl {
            c.message = Some(format!("DDL can't be checked on {engine} without running it"));
            created_before = true;
        } else if ours_on_new {
            c.message = Some("Uses a table an earlier statement creates".into());
        } else if !matches!(kind, StatementKind::Read | StatementKind::Dml) {
            c.message = Some("Not checked (only queries and DML can be checked without running)".into());
        } else {
            match p.session.check_sql(&sql).await {
                Ok(true) => c.status = "ok".into(),
                Ok(false) => c.message = Some(format!("{engine} has no way to check a statement without running it")),
                Err(e) => {
                    c.status = if created_before { "warning" } else { "error" }.into();
                    c.message = Some(if created_before { format!("{} (may be because an earlier statement creates or changes a table)", e.message) } else { e.message });
                }
            }
        }
        checks.push(c);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transactions_per_engine() {
        let ins = Act::Insert { label: String::new(), table: "t".into(), target: true };
        let ddl = Act::Sql { label: String::new(), sql: "CREATE TABLE t (a int)".into(), kind: ActKind::Ddl };
        let user_ddl = Act::Sql { label: String::new(), sql: "alter table t add b int".into(), kind: ActKind::User };
        let stage = Act::Sql { label: String::new(), sql: "CREATE TEMPORARY TABLE s AS SELECT * FROM t WHERE 1 = 0".into(), kind: ActKind::Stage };
        assert!(transaction(ConnectorKind::Postgres, &[ddl.clone(), ins.clone()]).is_some());
        assert!(transaction(ConnectorKind::Mssql, &[ddl.clone()]).is_some());
        assert!(transaction(ConnectorKind::Mysql, &[stage, ins.clone()]).is_some(), "temporary tables don't commit");
        assert!(transaction(ConnectorKind::Mysql, &[ddl, ins.clone()]).is_none());
        assert!(transaction(ConnectorKind::Mysql, &[user_ddl]).is_none());
        assert!(transaction(ConnectorKind::Oracle, &[ins.clone()]).is_none());
        assert!(transaction(ConnectorKind::Snowflake, &[ins]).is_none());
    }

    #[test]
    fn columns_match_ignoring_case() {
        let t = vec!["ID".to_string(), "Name".to_string(), "name".to_string()];
        assert_eq!(match_column("id", &t).unwrap(), "ID");
        assert_eq!(match_column("name", &t).unwrap(), "name", "exact first");
        assert!(match_column("x", &t).is_none());
        assert_eq!(user_statements(Some("delete from t where d = 1;\n\n update s set x = 1;"), ConnectorKind::Postgres), vec!["delete from t where d = 1", "update s set x = 1"]);
        assert!(user_statements(Some("  "), ConnectorKind::Postgres).is_empty());
    }
}
