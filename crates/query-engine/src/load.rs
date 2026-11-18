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
//! - The target path is checked before anything is written
//!   ([`QueryEngine::check_load_target`]): a catalog or schema that doesn't
//!   exist (or is written in another case) stops the load with what to
//!   create first; only the table is created automatically.
//! - Dry run: where the transaction covers everything, the step really runs
//!   on sample rows and is rolled back; elsewhere each statement is checked
//!   with `EXPLAIN` when the engine has it, the rest is listed as unchecked.

use std::sync::Arc;

use databrain_connector_core::arrow::array::RecordBatch;
use databrain_connector_core::arrow::datatypes::{Field, Schema, SchemaRef};
use databrain_connector_core::sql::{Checked, Statement, StatementKind, split_statements};
use databrain_connector_core::traits::NameLevels;
use databrain_connector_core::{
    CancellationToken, ConnectorKind, ErrorKind, ExecOptions, ObjectKind, QueryStream, Session, StreamEvent, quote_ident, split_path,
};
use databrain_export::load as sqlgen;
use databrain_workspace::LoadMode;
use serde::Serialize;

use crate::guard::{Hold, Intent};
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
    /// Whose SQL the before/after statements are (`Ai`: the DuckDB sandbox
    /// applies to the target session).
    pub origin: databrain_workspace::Origin,
    /// The user agreed to write into a production connection (a job's load
    /// step counts as agreed: the job was saved to do it). Unset, a load
    /// into a production connection fails with code `needs_confirmation`.
    pub confirmed: bool,
    /// Rows go in INSERTs only: no bulk path (Postgres `COPY`).
    pub insert_only: bool,
}

/// Outcome of a load.
#[derive(Debug, Clone, Default, Serialize, ts_rs::TS)]
#[ts(export)]
pub struct LoadReport {
    /// Rows inserted (Append/Truncate/Replace), or updated/merged as the
    /// database reports them (the staged rows when it doesn't).
    #[ts(as = "f64")]
    pub rows: u64,
    /// Sync: rows of the table deleted because their key isn't among the rows.
    #[ts(as = "f64")]
    pub deleted: u64,
    pub created_table: bool,
    pub transaction: bool,
    pub notices: Vec<String>,
}

/// Where a load would write, checked against the database: which parts of
/// the path exist. Names are compared exactly (they are quoted as typed).
#[derive(Debug, Clone, Default, Serialize, ts_rs::TS)]
#[ts(export)]
pub struct LoadTarget {
    /// The table with the catalog/schema the session would use filled in.
    pub resolved: String,
    pub catalog: Option<String>,
    pub schema: Option<String>,
    pub table: String,
    /// `None`: not checked (two-level engine, or an earlier part is missing).
    pub catalog_exists: Option<bool>,
    pub schema_exists: Option<bool>,
    pub table_exists: Option<bool>,
    /// Why rows can't go there: a missing catalog or schema, too many name
    /// parts, a view. Loading needs these fixed first.
    pub problems: Vec<String>,
    /// SQL that creates what is missing, to run first.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub create_sql: Option<String>,
    /// The path written with the database's case (`ANALYTICS.ORDERS` for
    /// `analytics.orders`) when that exists and what was typed doesn't.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub suggestion: Option<String>,
    /// Good to know: the default schema used, a name matched in another case.
    pub notes: Vec<String>,
    /// A part was typed in another case than the database's: `resolved`
    /// has the database's spelling, and the load uses it.
    pub respelled: bool,
    /// The schema as the explorer names it (`catalog.schema` on
    /// three-level engines), to describe the table.
    #[serde(skip)]
    #[ts(skip)]
    pub schema_id: Option<String>,
}

/// One statement (or group of INSERTs) of a dry run.
#[derive(Debug, Clone, Serialize, ts_rs::TS)]
#[ts(export)]
pub struct LoadCheck {
    pub label: String,
    pub sql: String,
    /// `ok`, `error`, `warning`, `unchecked`.
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub message: Option<String>,
}

/// Dry run report of a load step.
#[derive(Debug, Clone, Default, Serialize, ts_rs::TS)]
#[ts(export)]
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
    #[ts(as = "Option<f64>")]
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
    /// The load failed (sent once, by every failure of a load). `kept_nothing`:
    /// it failed before it wrote anything, or it ran in one transaction that
    /// was rolled back before any commit was sent and nothing in it commits
    /// on its own ([`kept_nothing`]), so running it again can't write the
    /// rows twice.
    Failed { kept_nothing: bool },
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
    /// DDL this step adds (create/drop the target).
    Ddl,
    /// Staging table create/drop (temporary on most engines).
    Stage,
    /// Update/merge from the staging table: its affected rows are the step's rows.
    Write,
    /// Sync's delete of the rows missing from the step's rows (counted apart).
    Remove,
    /// Other DML (delete all rows).
    Dml,
}

#[derive(Debug, Clone)]
enum Act {
    /// Before/after SQL of the user, classified.
    User { label: String, sql: Checked },
    /// SQL the load writes.
    Sql { label: String, sql: String, kind: ActKind },
    /// Insert the rows into `table`; `target` = the step's table (not staging).
    Insert { label: String, table: String, target: bool },
    /// Can't be built (no rows to plan with).
    Skip { label: String, reason: String },
}

impl Act {
    fn label(&self) -> &str {
        match self {
            Act::User { label, .. } | Act::Sql { label, .. } | Act::Insert { label, .. } | Act::Skip { label, .. } => label,
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
        LoadMode::Sync => "sync by key (merge, then delete rows missing from the step's rows)",
    }
}

/// [`exec`], reported to `obs` as one statement.
async fn timed(session: &Arc<dyn Session>, label: &str, stmt: Statement<'_>, cancel: &CancellationToken, obs: &dyn LoadObserver) -> Result<Option<u64>> {
    let t = std::time::Instant::now();
    let r = exec(session, stmt, cancel).await;
    obs.event(LoadEvent::Statement {
        label: label.into(),
        sql: stmt.sql().into(),
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
    /// Before/after SQL calls something that may commit on its own (a
    /// procedure): a dry run doesn't run the plan, and a failure can't
    /// promise that nothing was kept.
    may_commit: bool,
    /// [`LoadSpec::insert_only`].
    insert_only: bool,
    /// The step set its own rows or KB per INSERT: they cap array calls too.
    batch_from_step: bool,
}

/// The step's own before/after SQL, checked once: transaction control is
/// refused (the load commits or rolls back its own work, and a dry run
/// rolls back everything), and whether a statement may commit by itself.
fn check_user_sql(stmts: &[Checked], dialect: ConnectorKind) -> Result<bool> {
    let mut may_commit = false;
    for checked in stmts {
        let (s, c) = (checked.sql(), checked.classification());
        let lower = s.to_ascii_lowercase();
        let kw = databrain_connector_core::sql::leading_keyword(s);
        let words: Vec<String> = lower.split(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_')).filter(|w| !w.is_empty()).map(str::to_string).collect();
        let word = |w: &str| words.iter().any(|x| x == w);
        // Autocommit settings, also as Snowflake's `ALTER SESSION SET AUTOCOMMIT`.
        let autocommit = (lower.contains("autocommit") || lower.contains("implicit_transactions"))
            && (c.kind == StatementKind::Session || (kw == "ALTER" && word("session")));
        // Transaction control the classifier doesn't call that: `XA …`,
        // `PREPARE TRANSACTION`, MySQL's `LOCK TABLES` (it commits), and a
        // COMMIT or ROLLBACK inside a statement it can't place
        // (`IF @@TRANCOUNT > 0 COMMIT`, a block that commits).
        let hidden = kw == "XA"
            || (kw == "PREPARE" && word("transaction"))
            || (kw == "LOCK" && dialect == ConnectorKind::Mysql)
            || (matches!(c.kind, StatementKind::Other | StatementKind::Unknown) && (word("commit") || word("rollback")));
        if c.kind == StatementKind::Transaction || autocommit || hidden {
            return Err(EngineError::new(
                "invalid",
                format!(
                    "Before/after SQL can't control the transaction ({}): the load commits or rolls back its own work. Remove it.",
                    if autocommit {
                        "autocommit".to_string()
                    } else if hidden {
                        kw.clone()
                    } else {
                        c.keyword.clone()
                    }
                ),
            ));
        }
        may_commit |= matches!(c.kind, StatementKind::Other | StatementKind::Unknown)
            || matches!(c.keyword.as_str(), "CALL" | "EXEC" | "EXECUTE" | "DO" | "BEGIN" | "DECLARE");
    }
    Ok(may_commit)
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
    k.traits().name
}

/// Statements of before/after SQL.
fn user_statements(sql: Option<&str>, dialect: ConnectorKind) -> Vec<Checked> {
    sql.map(|s| {
        split_statements(s, dialect).into_iter().map(|st| st.sql.trim().to_string()).filter(|s| !s.is_empty()).map(|s| Checked::new(s, dialect)).collect()
    })
    .unwrap_or_default()
}

/// Transaction statements when the whole plan can run in one.
fn transaction(dialect: ConnectorKind, plan: &[Act]) -> Option<Tx> {
    use databrain_connector_core::traits::DdlCommits;
    let t = dialect.traits().transactions?;
    if t.ddl_commits != DdlCommits::No {
        // DDL commits implicitly (MySQL: not temporary tables; Snowflake:
        // those too, so a staging table ends the transaction).
        let any = t.ddl_commits == DdlCommits::AnyDdl;
        let ddl = plan.iter().any(|a| match a {
            Act::Sql { kind: ActKind::Ddl, .. } => true,
            Act::Sql { kind: ActKind::Stage, .. } => any,
            Act::User { sql, .. } => sql.kind() == StatementKind::Ddl,
            _ => false,
        });
        if ddl {
            return None;
        }
    }
    // MySQL also commits for statements that aren't DDL or transaction
    // control (`SET PASSWORD`, `RESET …`, `LOAD INDEX INTO CACHE`, account
    // statements): the before/after SQL may only read, change rows, or set
    // harmless session values for the plan to run in one transaction.
    if dialect == ConnectorKind::Mysql
        && plan.iter().any(|a| match a {
            Act::User { sql, .. } => {
                // MySQL's implicit commits: DDL, account statements, admin
                // statements (`ANALYZE TABLE`, `RESET`, `FLUSH`…), and of the
                // ones classified otherwise `SET PASSWORD` (a session
                // statement) and `LOAD INDEX INTO CACHE` (`LOAD` reads as
                // DML for `LOAD DATA`).
                // The words of its code, past comments (`/* x */ SET PASSWORD …`)
                // and strings.
                let code = databrain_connector_core::sql::code_text(sql.sql(), ConnectorKind::Mysql).to_ascii_uppercase();
                let words: Vec<&str> = code.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '@')).filter(|w| !w.is_empty()).collect();
                let two = (words.first().copied().unwrap_or_default(), words.get(1).copied().unwrap_or_default());
                match sql.kind() {
                    StatementKind::Read => false,
                    StatementKind::Dml => two == ("LOAD", "INDEX"),
                    // `SET autocommit = 1` commits what is open, as `SET PASSWORD` does.
                    StatementKind::Session => two == ("SET", "PASSWORD") || words.iter().any(|w| matches!(*w, "AUTOCOMMIT" | "@@AUTOCOMMIT")),
                    _ => true,
                }
            }
            _ => false,
        })
    {
        return None;
    }
    Some(Tx { begin: t.begin, commit: t.commit, rollback: t.rollback })
}

/// A load failed before it wrote anything: say so (a retry can't write twice).
fn before_writing(obs: &dyn LoadObserver, e: EngineError) -> EngineError {
    obs.event(LoadEvent::Failed { kept_nothing: true });
    e
}

/// Whether a load that failed once it started writing kept nothing: it ran
/// in one transaction (`transaction`) that was rolled back before its commit
/// was sent (`committing`: a lost connection may come after the server
/// committed), and none of its before/after SQL may commit on its own
/// (`may_commit`). A database without transactions keeps every statement
/// that ended.
fn kept_nothing(transaction: bool, may_commit: bool, committing: bool) -> bool {
    transaction && !may_commit && !committing
}

/// Run one statement; rows affected when the driver reports them.
async fn exec(session: &Arc<dyn Session>, sql: Statement<'_>, cancel: &CancellationToken) -> Result<Option<u64>> {
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
    /// Rows of an output, for a load: all of them in memory; for an output
    /// on disk a sample (its first 65,536 rows). [`Self::write_output`]
    /// streams every row of either.
    pub fn output_rows(&self, reference: &str) -> Result<LoadRows> {
        let o = self.outputs.ensure_loaded(reference)?;
        self.result_rows(&o.result_id)
    }

    /// Rows of a result in the store (for a result on disk, its first
    /// 65,536 rows: a sample; [`Self::write_output`] streams them all).
    pub fn result_rows(&self, result_id: &str) -> Result<LoadRows> {
        let rs = self.results.get(result_id)?;
        let mut rs = rs.lock();
        let schema = rs.schema();
        if rs.is_on_disk() {
            let (b, _) = rs.view_slice(&databrain_result_store::ViewSpec::default(), 0, 65_536)?;
            return Ok(LoadRows { schema, batches: vec![b] });
        }
        let batches = rs.view_batches(&databrain_result_store::ViewSpec::default(), 65_536)?;
        Ok(LoadRows { schema, batches })
    }

    /// Every row of a result on disk as a stream, read on a blocking thread
    /// (None when the result is in memory).
    fn disk_stream(&self, result_id: &str) -> Result<Option<QueryStream>> {
        let rs = self.results.get(result_id)?;
        if !rs.lock().is_on_disk() {
            return Ok(None);
        }
        let (tx, stream) = QueryStream::channel(4);
        tokio::task::spawn_blocking(move || {
            // The files are read without the result's lock: a load's
            // before-SQL may read results.<this output> meanwhile.
            let schema = rs.lock().schema();
            if !tx.blocking_send(Ok(StreamEvent::Schema(schema))) {
                return;
            }
            let r = databrain_result_store::for_each_batch(&rs, &databrain_result_store::ViewSpec::default(), 2000, &mut |b| {
                // The load stopped reading: stop too.
                if tx.blocking_send(Ok(StreamEvent::Batch(b))) { Ok(()) } else { Err(databrain_result_store::Error::Spill("stopped".into())) }
            });
            let end = match r {
                Ok(()) => Ok(StreamEvent::Done(Default::default())),
                Err(e) => Err(databrain_connector_core::ConnectorError::internal(e.to_string())),
            };
            tx.blocking_send(end);
        });
        Ok(Some(stream))
    }

    /// Copy the rows of an output into a table of another connection (job
    /// load steps), with the step's before/after SQL, on the session `tab`.
    pub async fn write_output(
        &self,
        reference: &str,
        connection_id: &str,
        tab: &str,
        spec: &LoadSpec,
        cancel: &CancellationToken,
        obs: &dyn LoadObserver,
    ) -> Result<LoadReport> {
        let unwritten = |e: EngineError| before_writing(obs, e);
        // A result on disk is streamed: its rows are never all in memory.
        let o = self.outputs.ensure_loaded(reference).map_err(unwritten)?;
        if let Some(stream) = self.disk_stream(&o.result_id).map_err(unwritten)? {
            return self.load_stream(stream, connection_id, tab, spec, cancel, obs).await;
        }
        let rows = self.output_rows(reference).map_err(unwritten)?;
        self.load_rows(&rows, connection_id, tab, spec, cancel, obs).await
    }

    /// Write `rows` as `spec` says.
    pub async fn load_rows(
        &self,
        rows: &LoadRows,
        connection_id: &str,
        tab: &str,
        spec: &LoadSpec,
        cancel: &CancellationToken,
        obs: &dyn LoadObserver,
    ) -> Result<LoadReport> {
        self.load_with(rows, None, connection_id, tab, spec, cancel, obs).await
    }

    /// Run `sql` (one read) on the session `session` of connection `source`
    /// and write every row it returns as `spec` says, a batch at a time as
    /// they arrive: the rows are never all in memory, so neither the row
    /// limit nor the memory a result may keep cuts them short.
    #[allow(clippy::too_many_arguments)]
    pub async fn load_query(
        &self,
        source: &str,
        session: &str,
        sql: &str,
        connection_id: &str,
        tab: &str,
        spec: &LoadSpec,
        cancel: &CancellationToken,
        obs: &dyn LoadObserver,
    ) -> Result<LoadReport> {
        let unwritten = |e: EngineError| before_writing(obs, e);
        let kind = self.workspace.get_connection(source).map_err(|e| unwritten(e.into()))?.config.kind;
        let sql = Checked::new(sql, kind);
        if !sql.kind().is_read() {
            return Err(unwritten(EngineError::new(
                "invalid",
                "Only a query that reads rows can run again for a load; load the rows the result holds instead.",
            )));
        }
        // The query runs on the tab's session (its temp tables, settings):
        // hold it, like a full export, so nothing else runs there meanwhile.
        let hold = Hold::session(session, cancel.clone(), "A query is running in this tab: wait for it, then load every row");
        let (_, _claim) = self.guarded_or_refuse(source, Some(hold), Intent::Inspect).map_err(unwritten)?;
        let src = self.session_for(source, session, databrain_workspace::Origin::User).await.map_err(unwritten)?;
        let opts = ExecOptions { batch_size: 2000, cancel: cancel.clone(), max_rows: None, output_masks: None };
        let stream = src.execute(sql.statement(), opts).await.map_err(|e| unwritten(e.into()))?;
        self.load_stream(stream, connection_id, tab, spec, cancel, obs).await
    }

    /// Write every row of `stream` as `spec` says, as they arrive.
    async fn load_stream(
        &self,
        mut stream: QueryStream,
        connection_id: &str,
        tab: &str,
        spec: &LoadSpec,
        cancel: &CancellationToken,
        obs: &dyn LoadObserver,
    ) -> Result<LoadReport> {
        // The schema, then the first batch (none when there are no rows).
        let (mut schema, mut first) = (None, None);
        while first.is_none() {
            match stream.next().await {
                Some(Ok(StreamEvent::Schema(s))) => schema = Some(s),
                Some(Ok(StreamEvent::Batch(b))) if b.num_rows() > 0 => first = Some(b),
                Some(Ok(StreamEvent::Done(_))) | None => break,
                Some(Ok(_)) => {}
                Some(Err(e)) => return Err(before_writing(obs, e.into())),
            }
        }
        let schema = schema
            .or_else(|| first.as_ref().map(|b| b.schema()))
            .ok_or_else(|| before_writing(obs, EngineError::new("invalid", "The query returns no rows to load")))?;
        let more = first.is_some();
        let rows = LoadRows { schema, batches: first.into_iter().collect() };
        self.load_with(&rows, more.then_some(&mut stream), connection_id, tab, spec, cancel, obs).await
    }

    /// Write `rows`, then the batches still to come from `rest`.
    #[allow(clippy::too_many_arguments)]
    async fn load_with(
        &self,
        rows: &LoadRows,
        mut rest: Option<&mut QueryStream>,
        connection_id: &str,
        tab: &str,
        spec: &LoadSpec,
        cancel: &CancellationToken,
        obs: &dyn LoadObserver,
    ) -> Result<LoadReport> {
        // Preparing only reads (the target, its columns); the before SQL runs in the plan.
        let p = self.prepare_load(connection_id, tab, spec, Some(rows), false).await.map_err(|e| before_writing(obs, e))?;
        obs.event(LoadEvent::Info(format!(
            "Writing {} rows into {} on {} ({}, in batches of up to {} rows / {} KB): {}",
            if rest.is_some() { "every".to_string() } else { rows.count().to_string() },
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
            timed(&p.session, "Start the transaction", Statement::internal(tx.begin), cancel, obs)
                .await
                .map_err(|e| before_writing(obs, labelled("Start the transaction", e)))?;
        }
        // Once the commit is sent, a failure (a lost connection) may come
        // after the server committed.
        let mut committing = false;
        let result = async {
            let n = run_plan(&p, rows, rest.as_deref_mut(), cancel, None, obs).await?;
            if let Some(tx) = &p.tx {
                committing = true;
                timed(&p.session, "Commit", Statement::internal(tx.commit), cancel, obs).await.map_err(|e| labelled("Commit", e))?;
            }
            Ok::<(u64, u64), EngineError>(n)
        }
        .await;
        match result {
            Ok((n, deleted)) => {
                let mut notices = p.notices.clone();
                if p.creates {
                    let c = created_notice(&p, rows);
                    obs.event(LoadEvent::Info(c.clone()));
                    notices.push(c);
                }
                if p.plan.iter().any(|a| matches!(a, Act::Sql { kind: ActKind::Remove, .. })) {
                    let d = format!("Sync: {n} rows written, {deleted} deleted (their key isn't among the step's rows)");
                    obs.event(LoadEvent::Info(d.clone()));
                    notices.push(d);
                }
                Ok(LoadReport { rows: n, deleted, created_table: p.creates, transaction: p.tx.is_some(), notices })
            }
            Err(e) => {
                obs.event(LoadEvent::Failed { kept_nothing: kept_nothing(p.tx.is_some(), p.may_commit, committing) });
                if p.tx.is_some() && e.kind != "cancelled" {
                    obs.event(LoadEvent::Info("Rolling back the transaction: nothing is kept".into()));
                }
                self.abandon(&p, connection_id, tab).await;
                Err(match &p.tx {
                    Some(_) if e.kind != "cancelled" && p.may_commit => EngineError {
                        message: format!("{} (rolled back; a procedure the before/after SQL calls may have committed its own changes)", e.message),
                        ..e
                    },
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
            let _ = tokio::time::timeout(wait, exec(&p.session, Statement::internal(tx.rollback), &fresh)).await;
        }
        if let Some(stage) = &p.permanent_stage {
            let _ = tokio::time::timeout(wait, exec(&p.session, Statement::internal(&sqlgen::drop_stage_sql(p.dialect, stage)), &fresh)).await;
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
        // A procedure in the before/after SQL could commit inside the
        // transaction: then the dry run only checks, it runs nothing.
        match p.tx.as_ref().filter(|_| !p.may_commit) {
            Some(tx) => {
                report.method = "transaction".into();
                match exec(&p.session, Statement::internal(tx.begin), &cancel).await {
                    Ok(_) => {
                        let _ = run_plan(&p, data, None, &cancel, Some(&mut checks), &NoLoadObserver).await;
                        if let Err(e) = exec(&p.session, Statement::internal(tx.rollback), &cancel).await {
                            checks.push(LoadCheck { label: "Roll back".into(), sql: tx.rollback.into(), status: "error".into(), message: Some(e.message) });
                        }
                    }
                    Err(e) => {
                        checks.push(LoadCheck { label: "Start the transaction".into(), sql: tx.begin.into(), status: "error".into(), message: Some(e.message) })
                    }
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

    /// Check the path rows would be loaded into: the catalog (three-level
    /// engines), the schema (the session's default when not given) and the
    /// table, live from the database. A missing table is fine for modes
    /// that create it; a missing catalog or schema is a problem.
    pub async fn check_load_target(&self, connection_id: &str, table: &str) -> Result<LoadTarget> {
        let profile = self.workspace.get_connection(connection_id)?;
        let kind = profile.config.kind;
        let conn = profile.name.clone();
        let parts = split_path(kind, table.trim());
        let mut t = LoadTarget { table: parts.last().cloned().unwrap_or_default(), ..Default::default() };
        if parts.is_empty() || parts.iter().any(String::is_empty) {
            t.problems.push("Enter the table as schema.table (or just the table, for the default schema)".into());
            return Ok(t);
        }
        let levels = kind.traits().levels;
        let catalogs = match levels {
            NameLevels::Three | NameLevels::ThreeWithoutDatabase => self.list_catalogs(connection_id).await?,
            _ => None,
        };
        if parts.len() == 3 && catalogs.is_none() && levels == NameLevels::ThreeWithoutDatabase {
            // `otherdb.dbo.t` from a connection set to one database: valid,
            // but that database's schemas aren't listed here.
            t.catalog = Some(parts[0].clone());
            t.schema = Some(parts[1].clone());
            t.resolved = parts.join(".");
            t.notes.push(format!("{conn} is set to one database: {} in another database can't be checked before the load", t.resolved));
            return Ok(t);
        }
        let max = if catalogs.is_some() { 3 } else { 2 };
        if parts.len() > max {
            t.problems.push(if max == 3 {
                format!("{} has too many parts: write catalog.schema.table, schema.table or table", table.trim())
            } else {
                format!("{} has too many parts: write schema.table or table", table.trim())
            });
            return Ok(t);
        }
        let q = |s: &str| quote_ident(kind, s);
        // The name as the database has it: exactly, else the one name that
        // differs only in case (`account` for `ACCOUNT`). Several: ambiguous.
        fn matching(want: &str, have: &[String]) -> std::result::Result<Option<String>, Vec<String>> {
            if have.iter().any(|h| h == want) {
                return Ok(Some(want.to_string()));
            }
            let same: Vec<String> = have.iter().filter(|h| h.eq_ignore_ascii_case(want)).cloned().collect();
            match same.len() {
                0 => Ok(None),
                1 => Ok(same.into_iter().next()),
                _ => Err(same),
            }
        }
        let ambiguous = |what: &str, want: &str, all: Vec<String>| {
            format!("{what} {want} matches {} in {conn} (names differing only in case): type the exact name", all.join(", "))
        };
        let case_note = |t: &mut LoadTarget, what: &str, typed: &str, actual: &str| {
            t.respelled = true;
            t.notes.push(format!("{what} {typed} is {actual} in {conn}: loading into {actual}"));
        };

        // Catalog.
        let mut catalog: Option<String> = None;
        if let Some(cats) = &catalogs {
            let names: Vec<String> = cats.iter().map(|c| c.name.clone()).collect();
            let want = if parts.len() == 3 { Some(parts[0].clone()) } else { cats.iter().find(|c| c.is_default).map(|c| c.name.clone()) };
            match want {
                Some(c) => match matching(&c, &names) {
                    Ok(Some(found)) => {
                        if found != c {
                            case_note(&mut t, catalog_word(kind), &c, &found);
                        }
                        t.catalog_exists = Some(true);
                        catalog = Some(found);
                    }
                    Ok(None) => {
                        t.catalog_exists = Some(false);
                        t.catalog = Some(c.clone());
                        t.problems.push(format!("{} {c} doesn't exist in {conn}. Create it first: only the table is created for you.", catalog_word(kind)));
                        t.create_sql = create_catalog_sql(kind, &q(&c));
                        t.resolved = parts.join(".");
                        return Ok(t);
                    }
                    Err(all) => {
                        t.problems.push(ambiguous(catalog_word(kind), &c, all));
                        return Ok(t);
                    }
                },
                None if parts.len() < 3 => t.notes.push(format!(
                    "{conn} has no current {}: write {}.schema.table",
                    catalog_word(kind).to_lowercase(),
                    catalog_word(kind).to_lowercase()
                )),
                None => {}
            }
        }
        t.catalog = catalog.clone();

        // Schema.
        let schemas = match &catalog {
            Some(c) => self.list_catalog_schemas(connection_id, c).await?,
            None => self.list_schemas(connection_id).await?,
        };
        let short = |id: &str| -> String {
            match &catalog {
                Some(c) => id.strip_prefix(&format!("{c}.")).unwrap_or(id).to_string(),
                None => id.to_string(),
            }
        };
        let want = if parts.len() >= 2 {
            Some(parts[parts.len() - 2].clone())
        } else {
            let d = schemas.iter().find(|s| s.is_default).map(|s| short(&s.name));
            if let Some(d) = &d {
                t.notes.push(format!("No schema given: {d} is used ({conn}'s default)"));
            }
            d
        };
        let Some(typed_schema) = want else {
            // Can't tell which schema the session uses: not checked.
            t.resolved = parts.join(".");
            t.notes.push(format!("{conn} has no default schema to check against: write schema.table to check it"));
            return Ok(t);
        };
        let schema_names: Vec<String> = schemas.iter().map(|s| short(&s.name)).collect();
        let schema = match matching(&typed_schema, &schema_names) {
            Ok(Some(found)) => {
                if found != typed_schema {
                    case_note(&mut t, schema_word(kind), &typed_schema, &found);
                }
                found
            }
            Ok(None) => {
                t.schema = Some(typed_schema.clone());
                t.schema_exists = Some(false);
                t.resolved = join_path(kind, catalog.iter().map(String::as_str).chain([typed_schema.as_str(), t.table.as_str()]));
                t.problems.push(format!("{} {typed_schema} doesn't exist in {conn}. Create it first: only the table is created for you.", schema_word(kind)));
                let path = match &catalog {
                    Some(c) => format!("{}.{}", q(c), q(&typed_schema)),
                    None => q(&typed_schema),
                };
                t.create_sql = create_schema_sql(kind, &path, &typed_schema);
                return Ok(t);
            }
            Err(all) => {
                t.problems.push(ambiguous(schema_word(kind), &typed_schema, all));
                return Ok(t);
            }
        };
        let schema_id = schemas.iter().find(|s| short(&s.name) == schema).map(|s| s.name.clone()).unwrap_or_else(|| schema.clone());
        t.schema = Some(schema.clone());
        t.schema_exists = Some(true);
        t.schema_id = Some(schema_id.clone());

        // Table.
        let typed = t.table.clone();
        let names = vec![typed.clone(), typed.to_uppercase(), typed.to_lowercase()];
        let objs = self.list_objects_named(connection_id, &schema_id, &names).await?;
        let relations: Vec<_> = objs.iter().filter(|o| o.kind.is_relation()).collect();
        let rel_names: Vec<String> = relations.iter().map(|o| o.name.clone()).collect();
        let found = match matching(&typed, &rel_names) {
            Ok(f) => f,
            Err(all) => {
                t.problems.push(ambiguous("Table", &typed, all));
                return Ok(t);
            }
        };
        if let Some(f) = &found {
            if *f != typed {
                case_note(&mut t, "Table", &typed, f);
            }
            t.table = f.clone();
        }
        t.resolved = join_path(kind, catalog.iter().map(String::as_str).chain([schema.as_str(), t.table.as_str()]));
        match found.and_then(|f| relations.iter().find(|o| o.name == f).copied()) {
            Some(o) if o.kind == ObjectKind::Table || o.kind == ObjectKind::ForeignTable => t.table_exists = Some(true),
            Some(o) => {
                t.table_exists = Some(true);
                t.problems.push(format!(
                    "{} is a {}: rows can only be loaded into a table",
                    t.resolved,
                    if o.kind == ObjectKind::View { "view" } else { "materialized view" }
                ));
            }
            None => t.table_exists = Some(false),
        }
        Ok(t)
    }

    /// The key columns of `table` that may hold NULL: they need a null-safe
    /// match, which keeps Postgres and others from using an index or a hash
    /// join, so NOT NULL keys (every primary key) match with `=`. All keys
    /// when the table can't be described.
    async fn nullable_keys(&self, connection_id: &str, table: &str, keys: &[String]) -> Vec<String> {
        let described = async {
            let t = self.check_load_target(connection_id, table).await.ok()?;
            if t.table_exists != Some(true) {
                return None;
            }
            self.describe(connection_id, t.schema_id.as_deref()?, &t.table).await.ok()
        };
        match described.await {
            Some(d) => keys.iter().filter(|k| d.columns.iter().find(|c| c.name.eq_ignore_ascii_case(k)).is_none_or(|c| c.nullable)).cloned().collect(),
            None => keys.to_vec(),
        }
    }

    /// Validate the spec, probe the table and build the statements.
    async fn prepare_load(&self, connection_id: &str, tab: &str, spec: &LoadSpec, rows: Option<&LoadRows>, dry: bool) -> Result<Prepared> {
        let table = spec.table.trim().to_string();
        if table.is_empty() {
            return Err(EngineError::new("invalid", "Choose the table to write into"));
        }
        // Read-only refuses; production asks (code `needs_confirmation`),
        // except a dry run, which keeps nothing.
        let (profile, _) = self.guarded_or_refuse(connection_id, None, Intent::Load { table: &table, confirmed: spec.confirmed, dry })?;
        let dialect = profile.config.kind;
        if !sqlgen::is_load_target(dialect) {
            return Err(EngineError::new(
                "invalid",
                format!("Rows can't be loaded into {} connections (\"{}\"); save them to a file instead.", engine_name(dialect), profile.name),
            ));
        }
        let keys_in: Vec<String> = spec.key_columns.iter().map(|k| k.trim().to_string()).filter(|k| !k.is_empty()).collect();
        if spec.mode.needs_keys() && !sqlgen::keyed_loads(dialect) {
            return Err(EngineError::new(
                "invalid",
                format!("{} can't update rows by key (\"{}\"): choose Insert, Delete rows then insert, or Replace table.", engine_name(dialect), profile.name),
            ));
        }
        if spec.mode.needs_keys() && keys_in.is_empty() {
            return Err(EngineError::new("invalid", "Choose the key columns that identify a row (Update and Merge match rows by them)"));
        }
        let session = self.session_for(connection_id, tab, spec.origin).await?;
        // Redshift has no COPY from the client (nor `to_regclass`, nor
        // SAVEPOINT, which aborts the transaction there): INSERTs.
        let insert_only = inserts_only(spec.insert_only, &profile.config, session.detected_flavor());

        // Does the table exist, and with which columns?
        let mut table = table;
        let (mut exists, mut target_cols, mut missing_reason) = probe(&session, dialect, &table).await?;
        let before = user_statements(spec.before_sql.as_deref(), dialect);
        let after = user_statements(spec.after_sql.as_deref(), dialect);
        let may_commit = check_user_sql(&before, dialect)? | check_user_sql(&after, dialect)?;
        // Not there as typed: the path as the database spells it
        // (`account` → `ACCOUNT`), or why when the catalog or schema is
        // missing, before any CREATE TABLE fails on it (before SQL may
        // create them).
        if !exists && let Ok(t) = self.check_load_target(connection_id, &table).await {
            let missing = t.catalog_exists == Some(false) || t.schema_exists == Some(false);
            if missing && before.is_empty() {
                let mut m = t.problems.join(" ");
                if let Some(sql) = &t.create_sql {
                    m.push_str(&format!(" ({sql})"));
                }
                return Err(EngineError::new("invalid", m));
            }
            if !missing && t.problems.is_empty() && t.respelled {
                table = t.resolved.clone();
                if t.table_exists == Some(true) {
                    (exists, target_cols, missing_reason) = probe(&session, dialect, &table).await?;
                }
            }
        }
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
                        format!(
                            "{table} in {conn} has no column {} (its columns: {}). Rename them in the step's SQL, or use Replace to recreate the table.",
                            missing.join(", "),
                            cols.join(", ")
                        ),
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
                                return Err(EngineError::new(
                                    "invalid",
                                    format!(
                                        "Key column {k} is not among the {} columns: {}",
                                        if row_cols.is_some() { "rows'" } else { "table's" },
                                        pool.join(", ")
                                    ),
                                ));
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
            plan.push(Act::User { label: format!("Before SQL {}", i + 1), sql: s.clone() });
        }
        let mut creates = false;
        let mut permanent_stage = None;
        let no_rows = |label: &str| Act::Skip { label: label.into(), reason: "Needs the step's rows, which couldn't be selected".into() };
        match spec.mode {
            LoadMode::Append | LoadMode::Truncate => {
                if !exists {
                    if !spec.create_table {
                        if before.is_empty() {
                            return Err(EngineError::new(
                                "invalid",
                                format!("Table {table} doesn't exist in {conn}{}. Turn on \"Create the table if it's missing\", or create it first.", reason()),
                            ));
                        }
                        notices.push(format!("{table} doesn't exist in {conn} yet: the before SQL must create it."));
                    } else {
                        creates = true;
                        notices
                            .push(format!("{table} doesn't exist in {conn}: {} creates it from the rows' columns.", if dry { "the run" } else { "this run" }));
                        plan.push(match &schema {
                            Some(s) => {
                                Act::Sql { label: "Create table".into(), sql: sqlgen::create_table_if_missing_sql(dialect, &table, s), kind: ActKind::Ddl }
                            }
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
            LoadMode::Update | LoadMode::Merge | LoadMode::Sync => {
                let what = match spec.mode {
                    LoadMode::Update => "Update",
                    LoadMode::Merge => "Merge",
                    _ => "Sync",
                };
                // No rows would delete the whole table: almost always an upstream mistake.
                if spec.mode == LoadMode::Sync && rows.is_some_and(|r| r.count() == 0) {
                    return Err(EngineError::new(
                        "invalid",
                        format!(
                            "The step has no rows: Sync would delete every row of {table} in {conn}. Use \"Delete rows, then insert\" to empty it on purpose."
                        ),
                    ));
                }
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
                        plan.push(Act::Sql {
                            label: "Create staging table".into(),
                            sql: sqlgen::create_stage_sql(dialect, &stage, &table, cols),
                            kind: ActKind::Stage,
                        });
                        plan.push(Act::Insert { label: "Stage rows".into(), table: stage.clone(), target: false });
                        let nullable = if exists { self.nullable_keys(connection_id, &table, &keys).await } else { keys.clone() };
                        let keys = sqlgen::Keys { all: &keys, nullable: &nullable };
                        let writes = if spec.mode == LoadMode::Update {
                            sqlgen::update_from_stage_sql(dialect, &table, &stage, cols, keys)
                        } else {
                            sqlgen::merge_from_stage_sql(dialect, &table, &stage, cols, keys)
                        };
                        let n = writes.len();
                        let sync = spec.mode == LoadMode::Sync;
                        for (i, sql) in writes.into_iter().enumerate() {
                            let label = match (spec.mode, n, i) {
                                (LoadMode::Update, _, _) => "Update matching rows".to_string(),
                                (_, 1, _) => "Merge rows".to_string(),
                                (_, _, 0) => "Update matching rows".to_string(),
                                _ => "Insert new rows".to_string(),
                            };
                            plan.push(Act::Sql { label, sql, kind: ActKind::Write });
                        }
                        if sync {
                            plan.push(Act::Sql {
                                label: "Delete rows missing from the step's rows".into(),
                                sql: sqlgen::delete_missing_sql(dialect, &table, &stage, keys),
                                kind: ActKind::Remove,
                            });
                        }
                        plan.push(Act::Sql { label: "Drop staging table".into(), sql: sqlgen::drop_stage_sql(dialect, &stage), kind: ActKind::Stage });
                    }
                    None => plan.push(no_rows(&format!("{what} rows"))),
                }
            }
        }
        for (i, s) in after.iter().enumerate() {
            plan.push(Act::User { label: format!("After SQL {}", i + 1), sql: s.clone() });
        }
        let (mut batch, lowered) = sqlgen::batch_limits(dialect).resolve(spec.batch_rows, spec.batch_kb);
        notices.extend(lowered);
        if dialect == ConnectorKind::Mysql {
            let packet = databrain_connector_core::query_text(&*session, "SELECT @@max_allowed_packet")
                .await
                .ok()
                .and_then(|r| r.first().and_then(|row| row.first().cloned().flatten()).and_then(|v| v.trim().parse::<usize>().ok()));
            notices.extend(fit_packet(&mut batch, packet, spec.batch_kb.is_some_and(|k| k > 0)));
        }
        let tx = transaction(dialect, &plan);
        if tx.is_none() {
            notices.push(match dialect.traits().transactions {
                Some(_) => format!(
                    "{} commits DDL at once, so this step doesn't run in a transaction: if it fails partway, what ran before stays.",
                    engine_name(dialect)
                ),
                None => format!("{} doesn't run this step in a transaction: if it fails partway, what ran before stays.", engine_name(dialect)),
            });
        }
        Ok(Prepared {
            dialect,
            connection: conn,
            table,
            session,
            exists: Some(exists),
            creates,
            plan,
            schema,
            tx,
            batch,
            permanent_stage,
            notices,
            may_commit,
            insert_only,
            batch_from_step: spec.batch_rows.is_some_and(|r| r > 0) || spec.batch_kb.is_some_and(|k| k > 0),
        })
    }
}

/// MySQL refuses a statement past the server's `max_allowed_packet` and
/// closes the connection (in a load's transaction, nothing can go on), so
/// INSERTs stay under it, with room for the packet's own bytes: the
/// default size quietly, a size the step set with a note. When the server
/// doesn't say (`packet` is `None`), the default stays at 1 MB, under any
/// server's setting.
fn fit_packet(batch: &mut sqlgen::BatchSize, packet: Option<usize>, user_set: bool) -> Option<String> {
    const KB: usize = 1024;
    let Some(packet) = packet else {
        if !user_set {
            batch.bytes = batch.bytes.min(1024 * KB);
        }
        return None;
    };
    let room = packet.saturating_sub((packet / 10).max(16 * KB)).max(KB);
    if batch.bytes <= room {
        return None;
    }
    let was = batch.bytes;
    batch.bytes = room;
    user_set.then(|| format!("INSERT size lowered from {} KB to {} KB (the server's max_allowed_packet is {} KB)", was / KB, room / KB, packet / KB))
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
/// Run the plan; returns the rows written and the rows Sync deleted.
/// The next batch with rows of a query being loaded (None at its end).
async fn next_batch(stream: &mut QueryStream) -> Result<Option<RecordBatch>> {
    loop {
        match stream.next().await {
            Some(Ok(StreamEvent::Batch(b))) if b.num_rows() > 0 => return Ok(Some(b)),
            Some(Ok(StreamEvent::Done(_))) | None => return Ok(None),
            Some(Ok(_)) => {}
            Some(Err(e)) => return Err(EngineError::from(e)),
        }
    }
}

/// What a group of INSERTs (or a COPY) wrote.
struct Written {
    rows: u64,
    statements: usize,
    /// An INSERT refused as too large was split: its rows, and the rows
    /// per statement the load went on with.
    split: Option<(usize, usize)>,
    /// Several INSERTs ran at once: the rows written before a failure
    /// aren't necessarily the first ones.
    parallel: bool,
    /// INSERTs running at once were refused for that; the rest went one at a time.
    serialized: bool,
    /// The first statement, or the one that failed.
    sql: String,
    result: Result<()>,
}

/// The rows of a load: those held, then those still streaming in.
struct Feed<'a, 'b> {
    held: std::slice::Iter<'a, RecordBatch>,
    rest: Option<&'b mut QueryStream>,
}

impl Feed<'_, '_> {
    async fn next(&mut self) -> Result<Option<RecordBatch>> {
        if let Some(b) = self.held.next() {
            return Ok(Some(b.clone()));
        }
        match self.rest.as_deref_mut() {
            None => Ok(None),
            Some(stream) => next_batch(stream).await,
        }
    }
}

/// Rows waiting for an INSERT: those an INSERT refused as too large (sent
/// again first), then the feed's. A statement takes rows from as many
/// incoming batches as its size needs (rows arrive 2,000 at a time from a
/// query or a result on disk, which capped every INSERT at that).
struct Pending<'f, 'a, 'b> {
    feed: &'f mut Feed<'a, 'b>,
    retry: std::collections::VecDeque<RecordBatch>,
    rows: Option<RecordBatch>,
    at: usize,
    ended: bool,
}

impl Pending<'_, '_, '_> {
    /// At least `rows` rows (all that are left at the end) in one batch, taken.
    async fn gather(&mut self, rows: usize) -> Result<Option<RecordBatch>> {
        let mut have: Vec<RecordBatch> = self.rows.take().map(|r| r.slice(self.at, r.num_rows() - self.at)).into_iter().filter(|r| r.num_rows() > 0).collect();
        self.at = 0;
        let mut n: usize = have.iter().map(RecordBatch::num_rows).sum();
        while !self.ended && n < rows {
            match self.feed.next().await? {
                Some(batch) => {
                    n += batch.num_rows();
                    have.push(batch);
                }
                None => self.ended = true,
            }
        }
        match have.as_slice() {
            [] => Ok(None),
            [one] => Ok(Some(one.clone())),
            many => databrain_connector_core::arrow::compute::concat_batches(&many[0].schema(), many)
                .map(Some)
                .map_err(|e| EngineError::new("internal", e.to_string())),
        }
    }

    /// The next statement for at most `size` rows: its SQL and its rows.
    async fn next(&mut self, b: &sqlgen::InsertSql, size: usize) -> Result<Option<(String, RecordBatch)>> {
        fn internal(e: impl std::fmt::Display) -> EngineError {
            EngineError::new("internal", e.to_string())
        }
        if let Some(chunk) = self.retry.pop_front() {
            let Some((sql, k)) = b.next(&chunk, 0, Some(size)).map_err(internal)? else { return Ok(None) };
            if k < chunk.num_rows() {
                self.retry.push_front(chunk.slice(k, chunk.num_rows() - k));
            }
            return Ok(Some((sql, chunk.slice(0, k))));
        }
        // Rows enough for a full statement, or all that is left.
        let Some(rows) = self.gather(size).await? else { return Ok(None) };
        let Some((sql, k)) = b.next(&rows, 0, Some(size)).map_err(internal)? else { return Ok(None) };
        self.at = k;
        let taken = rows.slice(0, k);
        self.rows = Some(rows);
        Ok(Some((sql, taken)))
    }
}

/// The rows by array INSERTs (Oracle array DML, Snowflake array binds):
/// one INSERT run with up to `ai.rows_per_call(..)` rows bound at once,
/// fewer past `ai.bytes_per_call()`, each call all or nothing.
/// A session that turns the first call down (a thin helper older than
/// array binding) gets INSERTs instead, the rows it held put back.
#[allow(clippy::too_many_arguments)]
async fn array_rows(
    p: &Prepared,
    table: &str,
    schema: &SchemaRef,
    ai: &sqlgen::ArrayInsert,
    mut feed: Feed<'_, '_>,
    cancel: &CancellationToken,
    progress: &(dyn Fn(u64) + Sync),
    note: &(dyn Fn(String) + Sync),
) -> Written {
    let mut w = Written { rows: 0, statements: 0, sql: ai.sql.clone(), result: Ok(()), split: None, parallel: false, serialized: false };
    let mut rows = Pending { feed: &mut feed, retry: Default::default(), rows: None, at: 0, ended: false };
    // A step's own rows and KB per INSERT cap the calls too.
    let (rows_cap, bytes_cap) = (ai.rows_per_call(p.session.insert_array_values()), ai.bytes_per_call());
    let types = ai.bind_types();
    let (mut per_call, max_bytes) =
        if p.batch_from_step { (rows_cap.min(p.batch.rows.max(1)), bytes_cap.min(p.batch.bytes.max(1))) } else { (rows_cap, bytes_cap) };
    // Numbers are written with the session's decimal separator, so they
    // convert as the INSERT's literals do; unknown, INSERTs.
    let decimal = match decimal_separator(p).await {
        Some(d) => d,
        None => {
            note("INSERT ALLs: the session's decimal separator couldn't be read for array INSERTs".into());
            return insert_rows(p, table, schema, feed, cancel, progress).await;
        }
    };
    loop {
        let batch = match rows.gather(per_call).await {
            Ok(Some(b)) => b,
            Ok(None) => return w,
            Err(e) => {
                w.result = Err(e);
                return w;
            }
        };
        let mut at = 0;
        while at < batch.num_rows() {
            let (columns, k) = match ai.next(&batch, at, per_call, max_bytes, decimal) {
                Ok(Some(x)) => x,
                Ok(None) => break,
                Err(e) => {
                    w.result = Err(EngineError::new("internal", e.to_string()));
                    return w;
                }
            };
            match p.session.insert_array(&ai.sql, columns, &types, cancel.clone()).await {
                Ok(Some(n)) => {
                    if w.statements == 0 {
                        note(format!("array INSERTs, up to {} rows bound to one statement a call ({})", grouped(per_call), ai.sql));
                    }
                    // The rows the server says it inserted (said when they differ from those sent).
                    if n != k as u64 {
                        note(format!("{} said it inserted {} rows of the {} rows of a call", engine_name(p.dialect), grouped(n as usize), grouped(k)));
                    }
                    at += k;
                    w.rows += n;
                    w.statements += 1;
                    progress(w.rows);
                }
                // Refused for its size: the call wrote nothing; half its rows go
                // again, and the rest of the load uses that size.
                Err(e) if k > 1 && rejected_as_too_large(&EngineError::from(e.clone())) => {
                    per_call = k / 2;
                    w.split.get_or_insert((k, per_call)).1 = per_call;
                }
                Ok(None) if w.statements == 0 => {
                    note(match p.dialect {
                        ConnectorKind::Oracle => "INSERT ALLs: this Oracle driver doesn't take array INSERTs (a helper older than 0.1.5)".into(),
                        _ => format!("INSERTs: {} doesn't take array binds for this statement", engine_name(p.dialect)),
                    });
                    rows.retry.push_front(batch.slice(at, batch.num_rows() - at));
                    return insert_pending(p, table, schema, rows, cancel, progress).await;
                }
                Ok(None) => {
                    w.result = Err(EngineError::new("internal", "the session stopped taking array INSERTs part way"));
                    return w;
                }
                Err(e) => {
                    let mut e = EngineError::from(e);
                    e.message = name_failing_row(&e.message, w.rows, k);
                    w.result = Err(e);
                    return w;
                }
            }
        }
    }
}

/// Rows per appender call ([`appended_rows`]).
const APPEND_ROWS: usize = 100_000;

/// Whether the rows' columns go through DuckDB's appender as they are:
/// the types it takes without converting them (a `Date64` would lose its
/// time, a nanosecond time zone timestamp its last digits, a decimal over
/// 38 digits its exactness); `Err` says which column goes by INSERTs.
fn appender_fits(schema: &SchemaRef) -> std::result::Result<(), String> {
    use databrain_connector_core::arrow::datatypes::{DataType as D, TimeUnit};
    for f in schema.fields() {
        let ok = match f.data_type() {
            D::Boolean | D::Int8 | D::Int16 | D::Int32 | D::Int64 | D::UInt8 | D::UInt16 | D::UInt32 | D::UInt64 => true,
            D::Float32 | D::Float64 | D::Utf8 | D::LargeUtf8 | D::Utf8View | D::Binary | D::LargeBinary | D::Date32 => true,
            D::Timestamp(_, None) => true,
            D::Timestamp(u, Some(_)) => *u != TimeUnit::Nanosecond,
            D::Decimal128(p, _) => *p <= 38,
            _ => false,
        };
        if !ok {
            return Err(format!("{} ({}) goes by INSERTs", f.name(), f.data_type()));
        }
    }
    Ok(())
}

/// The rows by the database's appender (DuckDB): up to [`APPEND_ROWS`] a
/// call, each through a temporary table and one `INSERT … SELECT` that
/// converts the values as INSERTs do (see `Session::append_rows`). A session
/// that turns the first call down gets INSERTs, the rows put back.
async fn appended_rows(
    p: &Prepared,
    table: &str,
    schema: &SchemaRef,
    mut feed: Feed<'_, '_>,
    cancel: &CancellationToken,
    progress: &(dyn Fn(u64) + Sync),
    note: &(dyn Fn(String) + Sync),
) -> Written {
    let mut w = Written {
        rows: 0,
        statements: 0,
        sql: format!("INSERT INTO {table} SELECT … (appender)"),
        result: Ok(()),
        split: None,
        parallel: false,
        serialized: false,
    };
    let columns: Vec<String> = schema.fields().iter().map(|f| f.name().clone()).collect();
    let mut rows = Pending { feed: &mut feed, retry: Default::default(), rows: None, at: 0, ended: false };
    loop {
        let batch = match rows.gather(APPEND_ROWS).await {
            Ok(Some(b)) => b,
            Ok(None) => return w,
            Err(e) => {
                w.result = Err(e);
                return w;
            }
        };
        let k = batch.num_rows();
        match p.session.append_rows(table, &columns, batch.clone(), cancel.clone()).await {
            Ok(Some(n)) => {
                if w.statements == 0 {
                    note(format!("the appender, up to {} rows a call through a temporary table", grouped(APPEND_ROWS)));
                }
                w.rows += n;
                w.statements += 1;
                progress(w.rows);
            }
            Ok(None) if w.statements == 0 => {
                rows.retry.push_front(batch);
                return insert_pending(p, table, schema, rows, cancel, progress).await;
            }
            Ok(None) => {
                w.result = Err(EngineError::new("internal", "the session stopped taking appended rows part way"));
                return w;
            }
            Err(e) => {
                let mut e = EngineError::from(e);
                e.message = name_failing_row(&e.message, w.rows, k);
                w.result = Err(e);
                return w;
            }
        }
    }
}

/// The session's decimal separator (Oracle's `NLS_NUMERIC_CHARACTERS`);
/// `.` where numbers don't depend on it. `None`: it couldn't be read.
async fn decimal_separator(p: &Prepared) -> Option<char> {
    if p.dialect != ConnectorKind::Oracle {
        return Some('.');
    }
    let r =
        databrain_connector_core::query_text(&*p.session, "SELECT value FROM nls_session_parameters WHERE parameter = 'NLS_NUMERIC_CHARACTERS'").await.ok()?;
    r.first().and_then(|row| row.first().cloned().flatten()).and_then(|v| v.chars().next())
}

/// An array call's error with the rows it held among the step's rows
/// (neither driver says which row failed without batch-error mode, whose
/// call fails anyway past too many bad rows).
fn name_failing_row(message: &str, written: u64, k: usize) -> String {
    let at = |n: u64| grouped(written as usize + n as usize);
    format!("{message} (in rows {} to {} of the step's rows)", at(1), at(k as u64))
}

/// `12345` as `12,345`.
fn grouped(n: usize) -> String {
    let d = n.to_string();
    let mut out = String::new();
    for (i, c) in d.chars().enumerate() {
        if i > 0 && (d.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// One INSERT that ended: its SQL, rows and result, whether another was
/// running when it started, and how many had started before it.
type Ran = (String, RecordBatch, Result<Option<u64>>, bool, usize);

/// Batched INSERTs, built while the rows are sent: one after the other, or
/// outside a transaction up to the database's `parallel_inserts` at once
/// (Databricks: each waits seconds for its Delta commit, and appends don't
/// conflict). An INSERT the server refuses as too large
/// ([`rejected_as_too_large`]) is sent again as INSERTs of half its rows,
/// and the rest of the load uses that size; only outside a transaction,
/// where a refused statement wrote nothing and the session goes on. After a
/// failure, the INSERTs already running end before it is reported (their
/// rows count as written).
async fn insert_rows(
    p: &Prepared,
    table: &str,
    schema: &SchemaRef,
    mut feed: Feed<'_, '_>,
    cancel: &CancellationToken,
    progress: &(dyn Fn(u64) + Sync),
) -> Written {
    let rows = Pending { feed: &mut feed, retry: Default::default(), rows: None, at: 0, ended: false };
    insert_pending(p, table, schema, rows, cancel, progress).await
}

/// [`insert_rows`] for rows partly read already (an array INSERT the
/// session turned down put its rows back).
async fn insert_pending(
    p: &Prepared,
    table: &str,
    schema: &SchemaRef,
    mut rows: Pending<'_, '_, '_>,
    cancel: &CancellationToken,
    progress: &(dyn Fn(u64) + Sync),
) -> Written {
    let b = sqlgen::InsertSql::new(p.dialect, table, schema, p.batch);
    let mut at_once = if p.tx.is_none() { p.dialect.traits().load.map_or(1, |l| l.parallel_inserts).max(1) } else { 1 };
    let mut w = Written { rows: 0, statements: 0, sql: String::new(), result: Ok(()), split: None, parallel: false, serialized: false };
    let mut size = p.batch.rows.max(1);
    let mut running: tokio::task::JoinSet<Ran> = tokio::task::JoinSet::new();
    let mut started = 0usize;
    let mut failed: Option<(String, EngineError)> = None;
    let mut drained = false;
    loop {
        while failed.is_none() && !drained && running.len() < at_once {
            match rows.next(&b, size).await {
                Ok(Some((sql, chunk))) => {
                    if w.sql.is_empty() {
                        w.sql = sql.clone();
                    }
                    let (session, cancel, beside, seq) = (p.session.clone(), cancel.clone(), !running.is_empty(), started);
                    // Rows written before a failure may now not be the first ones.
                    w.parallel |= beside;
                    started += 1;
                    running.spawn(async move {
                        let r = exec(&session, Statement::internal(&sql), &cancel).await;
                        (sql, chunk, r, beside, seq)
                    });
                }
                Ok(None) => drained = true,
                Err(e) => failed = Some((String::new(), e)),
            }
        }
        let Some(done) = running.join_next().await else { break };
        let (sql, chunk, r, beside, seq) = match done {
            Ok(x) => x,
            Err(e) => {
                failed.get_or_insert((String::new(), EngineError::new("internal", e.to_string())));
                continue;
            }
        };
        let k = chunk.num_rows();
        // It ran beside another: one was running when it started, or one
        // started before its result was taken.
        let together = beside || started > seq + 1;
        match r {
            Ok(_) => {
                w.rows += k as u64;
                w.statements += 1;
                progress(w.rows);
            }
            // Refused for running beside the others (a table with an identity
            // column takes one writer at a time): the refused commit wrote
            // nothing; its rows go again, and the rest one at a time.
            Err(e) if together && failed.is_none() && concurrent_write_conflict(&e) => {
                at_once = 1;
                w.serialized = true;
                rows.retry.push_back(chunk);
                drained = false;
            }
            Err(e) if k > 1 && p.tx.is_none() && failed.is_none() && rejected_as_too_large(&e) => {
                size = size.min(k / 2);
                w.split.get_or_insert((k, size)).1 = size;
                rows.retry.push_back(chunk);
                drained = false;
            }
            Err(e) => {
                failed.get_or_insert((sql, e));
            }
        }
    }
    if let Some((sql, e)) = failed {
        if !sql.is_empty() {
            w.sql = sql;
        }
        w.result = Err(e);
    }
    w
}

/// Delta refused a commit because another one changed the table at the
/// same time (`ConcurrentAppendException`, `MetadataChangedException`, …:
/// a table with an identity column takes one writer at a time). The
/// refused statement wrote nothing.
fn concurrent_write_conflict(e: &EngineError) -> bool {
    let m = format!("{} {}", e.code.as_deref().unwrap_or_default(), e.message).to_ascii_lowercase();
    [
        "concurrentappendexception",
        "metadatachangedexception",
        "concurrenttransactionexception",
        "delta_concurrent_append",
        "delta_metadata_changed",
        "delta_concurrent_transaction",
        "concurrentwriteexception",
        "delta_concurrent_write",
    ]
    .iter()
    .any(|w| m.contains(w))
}

/// The server refused a statement for its size before running it: the
/// request or statement is too large (Databricks' message size, MySQL's
/// `max_allowed_packet`, an HTTP 413), not an error in its rows.
fn rejected_as_too_large(e: &EngineError) -> bool {
    if e.kind == "cancelled" {
        return false;
    }
    let m = e.message.to_ascii_lowercase();
    m.contains("(413")
        || m.contains("request entity too large")
        || m.contains("payload too large")
        || m.contains("max_allowed_packet")
        || m.contains("packet too large")
        || m.contains("statement is too large")
        || m.contains("statement too large")
        || m.contains("query is too large")
        || m.contains("message size")
        || m.contains("message too large")
        || ((m.contains("exceed") || m.contains("too long") || m.contains("too large")) && (m.contains("size") || m.contains("length") || m.contains("bytes")))
}

/// Rows go in INSERTs only: the step says so, or the connection is Amazon
/// Redshift, however it was recognised (the flavor setting in any case,
/// the host name, or found at connect time).
fn inserts_only(step: bool, config: &databrain_connector_core::ConnectionConfig, session_flavor: Option<&str>) -> bool {
    step || config.is_redshift() || session_flavor.is_some_and(|f| f.eq_ignore_ascii_case("redshift"))
}

/// Why a load doesn't use COPY: the rows go in INSERTs instead
/// ([`CopyCheck::Inserts`]), or the check left the transaction unusable
/// and the load must stop ([`CopyCheck::Fatal`]).
enum CopyCheck {
    Inserts(String),
    Fatal(EngineError),
}

/// A bulk load instead of INSERTs: Postgres `COPY`, or Databricks files
/// staged in a volume and read by one `INSERT … SELECT`.
enum Bulk {
    Copy(sqlgen::PgCopy),
    Staged(sqlgen::DbxStage),
    /// ClickHouse: `INSERT … FORMAT TabSeparated`, the rows as the body.
    Tsv(sqlgen::ChTsv),
}

impl Bulk {
    fn sql(&self) -> &str {
        match self {
            Bulk::Copy(c) => &c.sql,
            Bulk::Staged(s) => &s.sql,
            Bulk::Tsv(t) => &t.sql,
        }
    }
}

/// The bulk load these rows may use (`Ok(None)`: INSERTs), as [`bulk_copy`]
/// and [`bulk_staged`] decide.
async fn bulk_plan(p: &Prepared, table: &str, schema: &SchemaRef, cancel: &CancellationToken) -> std::result::Result<Option<Bulk>, CopyCheck> {
    if p.insert_only {
        return Ok(None);
    }
    if !p.session.bulk_load_possible() {
        return Err(CopyCheck::Inserts(match p.dialect {
            ConnectorKind::Databricks => "no staging volume is set in the connection's settings".into(),
            _ => "this connection has no bulk load".into(),
        }));
    }
    match p.dialect {
        ConnectorKind::Postgres => Ok(bulk_copy(p, table, schema, cancel).await?.map(Bulk::Copy)),
        ConnectorKind::Databricks => Ok(bulk_staged(p, table, schema, cancel).await?.map(Bulk::Staged)),
        ConnectorKind::Clickhouse => Ok(bulk_tsv(p, table, schema, cancel).await?.map(Bulk::Tsv)),
        _ => Ok(None),
    }
}

/// A Databricks bulk load for these rows into `table`, when it writes what
/// the INSERTs would: every column's rows go into its column's type with
/// the same result ([`sqlgen::dbx_fits`]), read with `DESCRIBE TABLE`.
/// Whether the connection has a staging volume is the session's to say.
async fn bulk_staged(p: &Prepared, table: &str, schema: &SchemaRef, cancel: &CancellationToken) -> std::result::Result<Option<sqlgen::DbxStage>, CopyCheck> {
    let columns = described_columns(p, table, cancel).await?;
    let names: Vec<String> = columns.iter().map(|(n, _)| n.clone()).collect();
    let mut types = Vec::with_capacity(schema.fields().len());
    for f in schema.fields() {
        let Some(name) = match_column(f.name(), &names) else { return Err(CopyCheck::Inserts(format!("no column {}", f.name()))) };
        let ty = columns.iter().find(|(n, _)| n == name).map(|(_, t)| t.clone()).unwrap_or_default();
        sqlgen::dbx_fits(f, &ty).map_err(CopyCheck::Inserts)?;
        types.push(ty);
    }
    Ok(Some(sqlgen::DbxStage::new(table, schema, &types)))
}

/// A table's columns and types as `DESCRIBE TABLE` lists them (Databricks,
/// ClickHouse): rows of `name, type, …`, up to an empty or `#` row.
async fn described_columns(p: &Prepared, table: &str, cancel: &CancellationToken) -> std::result::Result<Vec<(String, String)>, CopyCheck> {
    let sql = format!("DESCRIBE TABLE {}", databrain_connector_core::quote_path(p.dialect, table));
    let opts = ExecOptions { cancel: cancel.clone(), ..Default::default() };
    let got = match p.session.execute(Statement::internal(&sql), opts).await {
        Ok(s) => s.collect().await,
        Err(e) => Err(e),
    };
    let c = match got {
        Ok(c) => c,
        Err(e) if e.is_cancelled() => return Err(CopyCheck::Fatal(EngineError::new("cancelled", "Cancelled"))),
        Err(e) => return Err(CopyCheck::Inserts(format!("the table's column types couldn't be read: {}", e.message))),
    };
    let mut columns: Vec<(String, String)> = Vec::new();
    'rows: for b in &c.batches {
        let cell = |col: usize, row: usize| databrain_connector_core::arrow::util::display::array_value_to_string(b.column(col), row).unwrap_or_default();
        for r in 0..b.num_rows() {
            let name = cell(0, r);
            if name.trim().is_empty() || name.starts_with('#') {
                break 'rows;
            }
            columns.push((name, cell(1, r)));
        }
    }
    Ok(columns)
}

/// ClickHouse rows as the body of `INSERT … FORMAT TabSeparated`, when it
/// writes what the VALUES INSERTs would: every column's rows parse as its
/// literal into the table's type ([`sqlgen::ch_fits`]).
async fn bulk_tsv(p: &Prepared, table: &str, schema: &SchemaRef, cancel: &CancellationToken) -> std::result::Result<Option<sqlgen::ChTsv>, CopyCheck> {
    let columns = described_columns(p, table, cancel).await?;
    let names: Vec<String> = columns.iter().map(|(n, _)| n.clone()).collect();
    for f in schema.fields() {
        let Some(name) = match_column(f.name(), &names) else { return Err(CopyCheck::Inserts(format!("no column {}", f.name()))) };
        let ty = columns.iter().find(|(n, _)| n == name).map(|(_, t)| t.clone()).unwrap_or_default();
        sqlgen::ch_fits(f, &ty).map_err(CopyCheck::Inserts)?;
    }
    Ok(Some(sqlgen::ChTsv::new(table, schema)))
}

/// The rows as TabSeparated INSERTs (ClickHouse): chunks of the step's
/// rows and bytes per INSERT, each one INSERT (a part), taken from as many
/// incoming batches as a chunk needs. Like INSERTs, the chunks sent before
/// a failure stay.
async fn tsv_rows(
    p: &Prepared,
    t: &sqlgen::ChTsv,
    mut writer: Box<dyn databrain_connector_core::BulkWriter>,
    mut feed: Feed<'_, '_>,
    progress: &(dyn Fn(u64) + Sync),
) -> Written {
    let mut w = Written { rows: 0, statements: 0, sql: t.sql.clone(), result: Ok(()), split: None, parallel: false, serialized: false };
    let mut rows = Pending { feed: &mut feed, retry: Default::default(), rows: None, at: 0, ended: false };
    loop {
        let batch = match rows.gather(p.batch.rows.max(1)).await {
            Ok(Some(b)) => b,
            Ok(None) => break,
            Err(e) => {
                w.result = Err(e);
                return w;
            }
        };
        let mut at = 0;
        while let Some(chunk) = t.next(&batch, at, p.batch.rows.max(1), p.batch.bytes).transpose() {
            let (data, k) = match chunk {
                Ok(x) => x,
                Err(e) => {
                    w.result = Err(EngineError::new("internal", e.to_string()));
                    return w;
                }
            };
            if let Err(e) = writer.write(data).await {
                w.result = Err(EngineError::from(e));
                return w;
            }
            at += k;
            w.rows += k as u64;
            w.statements += 1;
            progress(w.rows);
        }
    }
    if let Err(e) = writer.finish().await {
        w.result = Err(EngineError::from(e));
    }
    w
}

/// Postgres `COPY … FROM STDIN` for these rows into `table`, when it writes
/// what the INSERTs would (`Ok(None)` when the session has no COPY, `Err`
/// says why not): every column's rows fit the table's column type
/// ([`sqlgen::pg_copy_fits`]), and the table is a plain or partitioned table
/// without rules (COPY doesn't apply rules) or row-level security (COPY
/// refuses a table whose policies apply to the user).
async fn bulk_copy(p: &Prepared, table: &str, schema: &SchemaRef, cancel: &CancellationToken) -> std::result::Result<Option<sqlgen::PgCopy>, CopyCheck> {
    if p.dialect != ConnectorKind::Postgres {
        return Ok(None);
    }
    let path = databrain_connector_core::quote_path(p.dialect, table);
    let sql = format!(
        "SELECT c.relkind::text, c.relhasrules::text, a.attname::text, format_type(a.atttypid, NULL), c.relrowsecurity::text, \
         a.attidentity::text, a.attgenerated::text \
         FROM pg_catalog.pg_class c JOIN pg_catalog.pg_attribute a ON a.attrelid = c.oid AND a.attnum > 0 AND NOT a.attisdropped \
         WHERE c.oid = to_regclass({})",
        databrain_connector_core::quote_literal(&path)
    );
    // A failure of the savepoint itself (or of going back to it) leaves the
    // transaction aborted: the load stops with that error, not with the
    // INSERT's "current transaction is aborted" that would follow.
    let fatal =
        |what: &str, e: EngineError| CopyCheck::Fatal(if e.kind == "cancelled" { e } else { EngineError { message: format!("{what}: {}", e.message), ..e } });
    // In the load's transaction, a failed lookup would abort it (and the
    // INSERTs after it): it runs behind a savepoint, given up on failure.
    let savepoint = p.tx.is_some();
    if savepoint && let Err(e) = exec(&p.session, Statement::internal("SAVEPOINT databrain_copy_check"), cancel).await {
        return Err(fatal("Checking whether COPY can be used (SAVEPOINT)", e));
    }
    let opts = ExecOptions { cancel: cancel.clone(), ..Default::default() };
    let got = match p.session.execute(Statement::internal(&sql), opts).await {
        Ok(s) => s.collect().await,
        Err(e) => Err(e),
    };
    if savepoint {
        let end = if got.is_ok() { "RELEASE SAVEPOINT databrain_copy_check" } else { "ROLLBACK TO SAVEPOINT databrain_copy_check" };
        if let Err(e) = exec(&p.session, Statement::internal(end), cancel).await {
            return Err(fatal(&format!("Checking whether COPY can be used ({})", end.split(" SAVEPOINT").next().unwrap_or(end)), e));
        }
    }
    let c = match got {
        Ok(c) => c,
        Err(e) if e.is_cancelled() => return Err(CopyCheck::Fatal(EngineError::new("cancelled", "Cancelled"))),
        Err(e) => return Err(CopyCheck::Inserts(format!("the table's column types couldn't be read: {}", e.message))),
    };
    let columns: Vec<CopyColumn> = c
        .batches
        .iter()
        .flat_map(|b| {
            let cell =
                move |col: usize, row: usize| databrain_connector_core::arrow::util::display::array_value_to_string(b.column(col), row).unwrap_or_default();
            (0..b.num_rows()).map(move |r| CopyColumn {
                relkind: cell(0, r),
                rules: cell(1, r) == "true",
                name: cell(2, r),
                ty: cell(3, r),
                rls: cell(4, r) == "true",
                identity: cell(5, r),
                generated: cell(6, r),
            })
        })
        .collect();
    copy_fits(&columns, schema).map_err(CopyCheck::Inserts)?;
    Ok(Some(sqlgen::PgCopy::new(table, schema)))
}

/// One column of the target, as the COPY check reads it.
struct CopyColumn {
    relkind: String,
    rules: bool,
    name: String,
    ty: String,
    rls: bool,
    /// `attidentity`: `a` GENERATED ALWAYS, `d` BY DEFAULT, empty otherwise.
    identity: String,
    /// `attgenerated`: `s` a stored generated column, empty otherwise.
    generated: String,
}

/// Whether COPY writes into the table what the INSERTs would; `Err` says
/// why not. COPY skips what an INSERT is refused or rewritten by: rules,
/// row-level security (COPY refuses it), and it writes a value into a
/// GENERATED ALWAYS identity column, which an INSERT refuses.
fn copy_fits(columns: &[CopyColumn], schema: &SchemaRef) -> std::result::Result<(), String> {
    let Some(first) = columns.first() else { return Err("the table's column types couldn't be read".into()) };
    if !matches!(first.relkind.as_str(), "r" | "p") {
        return Err("not a plain table".into());
    }
    if first.rules {
        return Err("the table has rules, which COPY doesn't apply".into());
    }
    if first.rls {
        return Err("the table has row-level security, which COPY refuses".into());
    }
    for f in schema.fields() {
        let Some(c) = columns.iter().find(|c| &c.name == f.name()) else { return Err(format!("no column {}", f.name())) };
        if c.identity == "a" {
            return Err(format!("{} is GENERATED ALWAYS AS IDENTITY: COPY would write it, an INSERT refuses it", c.name));
        }
        if !c.generated.is_empty() {
            return Err(format!("{} is a generated column", c.name));
        }
        sqlgen::pg_copy_fits(f, &c.ty)?;
    }
    Ok(())
}

/// The rows by COPY: one COPY for all of them, sent in chunks of about the
/// batch's byte size. Stopped or failed, the COPY is abandoned (nothing of
/// it is kept).
async fn copy_rows(
    p: &Prepared,
    c: &sqlgen::PgCopy,
    mut writer: Box<dyn databrain_connector_core::BulkWriter>,
    mut feed: Feed<'_, '_>,
    progress: &(dyn Fn(u64) + Sync),
) -> Written {
    let mut w = Written { rows: 0, statements: 1, sql: c.sql.clone(), result: Ok(()), split: None, parallel: false, serialized: false };
    'rows: loop {
        let batch = match feed.next().await {
            Ok(Some(b)) => b,
            Ok(None) => break,
            Err(e) => {
                w.result = Err(e);
                break;
            }
        };
        let mut at = 0;
        loop {
            let (data, k) = match c.next(&batch, at, p.batch.bytes) {
                Ok(Some(x)) => x,
                Ok(None) => break,
                Err(e) => {
                    w.result = Err(EngineError::new("internal", e.to_string()));
                    break 'rows;
                }
            };
            // The writer watches `cancel` itself and asks the server to stop
            // the COPY: not raced here, which would drop it before it asked.
            let sent = writer.write(data).await.map_err(EngineError::from);
            if let Err(e) = sent {
                w.result = Err(e);
                break 'rows;
            }
            at += k;
            w.rows += k as u64;
            progress(w.rows);
        }
    }
    if w.result.is_err() {
        // Dropped unfinished: the server aborts the COPY, nothing of it is kept.
        drop(writer);
        w.rows = 0;
        return w;
    }
    let done = writer.finish().await.map_err(EngineError::from);
    match done {
        Ok(Some(n)) => w.rows = n,
        Ok(None) => {}
        // A bad row is reported here, at the end: none of the COPY is kept.
        Err(e) => {
            w.rows = 0;
            w.result = Err(e);
        }
    }
    w
}

/// Rows per staged file (fewer when they pass [`STAGED_FILE_BYTES`]).
const STAGED_FILE_ROWS: usize = 250_000;
/// Text per staged file, before compression.
const STAGED_FILE_BYTES: usize = 64 * 1024 * 1024;

/// The rows as staged files for one statement ([`Bulk::Staged`]): files of
/// up to [`STAGED_FILE_ROWS`] rows, then the statement that reads them all
/// (one commit). Stopped or failed, nothing is inserted, and the files are
/// deleted.
async fn staged_rows(
    stage: &sqlgen::DbxStage,
    mut writer: Box<dyn databrain_connector_core::BulkWriter>,
    mut feed: Feed<'_, '_>,
    progress: &(dyn Fn(u64) + Sync),
) -> Written {
    let mut w = Written { rows: 0, statements: 1, sql: stage.sql.clone(), result: Ok(()), split: None, parallel: false, serialized: false };
    let mut rows = Pending { feed: &mut feed, retry: Default::default(), rows: None, at: 0, ended: false };
    let failed = loop {
        let batch = match rows.gather(STAGED_FILE_ROWS).await {
            Ok(Some(b)) => b,
            Ok(None) => break None,
            Err(e) => break Some(e),
        };
        let mut at = 0;
        let mut err = None;
        while let Some(file) = stage.next(&batch, at, STAGED_FILE_BYTES).transpose() {
            let (data, k) = match file {
                Ok(x) => x,
                Err(e) => {
                    err = Some(EngineError::new("internal", e.to_string()));
                    break;
                }
            };
            if let Err(e) = writer.write(data).await {
                err = Some(EngineError::from(e));
                break;
            }
            at += k;
            w.rows += k as u64;
            progress(w.rows);
        }
        if err.is_some() {
            break err;
        }
    };
    if let Some(e) = failed {
        // Dropped unfinished: its files are deleted, nothing is inserted.
        drop(writer);
        w.rows = 0;
        w.result = Err(e);
        return w;
    }
    match writer.finish().await.map_err(EngineError::from) {
        Ok(Some(n)) => w.rows = n,
        Ok(None) => {}
        Err(e) => {
            w.rows = 0;
            w.result = Err(e);
        }
    }
    w
}

async fn run_plan(
    p: &Prepared,
    rows: &LoadRows,
    mut rest: Option<&mut QueryStream>,
    cancel: &CancellationToken,
    mut record: Option<&mut Vec<LoadCheck>>,
    obs: &dyn LoadObserver,
) -> Result<(u64, u64)> {
    let mut inserted = 0u64;
    let mut removed = 0u64;
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
            Act::User { label, sql } => {
                let r = timed(&p.session, label, sql.statement(), cancel, obs).await.map(|_| ());
                push(&mut record, label, sql.sql(), &r, None);
                r.map_err(|e| labelled(label, e))?;
            }
            Act::Sql { label, sql, kind } => {
                let r = timed(&p.session, label, Statement::internal(sql), cancel, obs).await;
                match (&r, kind) {
                    (Ok(Some(n)), ActKind::Write) => affected = Some(affected.unwrap_or(0) + n),
                    (Ok(Some(n)), ActKind::Remove) => removed += n,
                    _ => {}
                }
                let r = r.map(|_| ());
                push(&mut record, label, sql, &r, None);
                r.map_err(|e| labelled(label, e))?;
            }
            Act::Insert { label, table, target } => {
                let schema = p.schema.clone().unwrap_or_else(|| rows.schema.clone());
                // Rows still streaming in: the total isn't known (0).
                let total = if rest.is_some() { 0 } else { rows.count() };
                let started = std::time::Instant::now();
                let phase = if *target { "Inserting rows" } else { "Staging rows" };
                let mut why_not = None;
                let copy = match bulk_plan(p, table, &schema, cancel).await {
                    Ok(c) => c,
                    Err(CopyCheck::Inserts(why)) => {
                        why_not = Some(why);
                        None
                    }
                    Err(CopyCheck::Fatal(e)) => {
                        push(&mut record, label, "", &Err(e.clone()), None);
                        return Err(labelled(label, e));
                    }
                };
                // The COPY starts here (it waits for the table's lock); a
                // session without one sends INSERTs.
                let writer = match &copy {
                    Some(c) => match p.session.bulk_load(c.sql(), cancel.clone()).await {
                        Ok(Some(w)) => Some((c, w)),
                        Ok(None) => {
                            why_not = Some(match c {
                                Bulk::Copy(_) => "this connection has no COPY".into(),
                                Bulk::Staged(_) => "no staging volume is set in the connection's settings".into(),
                                Bulk::Tsv(_) => "this connection is read-only".into(),
                            });
                            None
                        }
                        Err(e) => {
                            let e = EngineError::from(e);
                            push(&mut record, label, c.sql(), &Err(e.clone()), None);
                            return Err(labelled(label, e));
                        }
                    },
                    None => None,
                };
                // No bulk load: array INSERTs where the session binds arrays (Oracle).
                let array_insert = (writer.is_none() && !p.insert_only && p.session.insert_array_possible())
                    .then(|| sqlgen::ArrayInsert::new(p.dialect, table, &schema))
                    .flatten();
                // No bulk load or array INSERTs: the appender where the session has one (DuckDB).
                // The columns' types, then the table's (`append_rows_refused`).
                let appender = if writer.is_none() && array_insert.is_none() && !p.insert_only && p.session.append_rows_possible() {
                    let columns: Vec<String> = schema.fields().iter().map(|f| f.name().clone()).collect();
                    let fits = match appender_fits(&schema) {
                        Ok(()) => match p.session.append_rows_refused(table, &columns, &schema).await {
                            Ok(None) => Ok(()),
                            Ok(Some(why)) => Err(why),
                            Err(e) if e.is_cancelled() => return Err(labelled(label, EngineError::from(e))),
                            Err(e) => Err(format!("the table's columns couldn't be read: {}", e.message)),
                        },
                        e => e,
                    };
                    match fits {
                        Ok(()) => Some(()),
                        Err(why) => {
                            obs.event(LoadEvent::Info(format!("{label}: INSERTs, not the appender ({why})")));
                            None
                        }
                    }
                } else {
                    None
                };
                let bulk_word = match p.dialect {
                    ConnectorKind::Databricks => "a bulk load",
                    ConnectorKind::Clickhouse => "TabSeparated",
                    _ => "COPY",
                };
                let how = match (&writer, &why_not) {
                    // Array INSERTs say so once a call went in (`array_rows`).
                    (None, _) if array_insert.is_some() => String::new(),
                    (Some((Bulk::Copy(c), _)), _) => format!("bulk load ({})", c.sql),
                    (Some((Bulk::Staged(s), _)), _) => format!("bulk load: Parquet files in the staging volume, then {}", s.sql),
                    (Some((Bulk::Tsv(t), _)), _) => format!("rows as TabSeparated, up to {} rows an INSERT ({})", grouped(p.batch.rows), t.sql),
                    (None, Some(why)) => format!("INSERTs, not {bulk_word} ({why})"),
                    (None, None) => String::new(),
                };
                if !how.is_empty() {
                    obs.event(LoadEvent::Info(format!("{label}: {how}")));
                    // The dry run says which one the load would use.
                    push(&mut record, &format!("{label}: how"), "", &Ok(()), Some(how));
                }
                obs.event(LoadEvent::Progress { phase: phase.into(), done: 0, total });
                let feed = Feed { held: rows.batches.iter(), rest: rest.as_deref_mut() };
                let progress = |done: u64| obs.event(LoadEvent::Progress { phase: phase.into(), done, total });
                let w = match writer {
                    Some((Bulk::Copy(c), writer)) => copy_rows(p, c, writer, feed, &progress).await,
                    Some((Bulk::Staged(s), writer)) => staged_rows(s, writer, feed, &progress).await,
                    Some((Bulk::Tsv(t), writer)) => tsv_rows(p, t, writer, feed, &progress).await,
                    None => match array_insert.as_ref() {
                        Some(ai) => {
                            // Said once a call went in (or why not), not before.
                            let note = |m: String| obs.event(LoadEvent::Info(format!("{label}: {m}")));
                            array_rows(p, table, &schema, ai, feed, cancel, &progress, &note).await
                        }
                        None if appender.is_some() => {
                            let note = |m: String| obs.event(LoadEvent::Info(format!("{label}: {m}")));
                            appended_rows(p, table, &schema, feed, cancel, &progress, &note).await
                        }
                        None => insert_rows(p, table, &schema, feed, cancel, &progress).await,
                    },
                };
                let Written { rows: n, statements, sql: first, result: r, split, parallel, serialized } = w;
                if serialized {
                    obs.event(LoadEvent::Info(format!(
                        "{label}: {} refused INSERTs running at the same time into {table} (a table with an identity column takes one writer at a time); they were sent again, and the rest went one at a time",
                        engine_name(p.dialect)
                    )));
                }
                if let Some((refused, rows)) = split {
                    obs.event(LoadEvent::Info(format!(
                        "{label}: {} refused an INSERT of {refused} rows as too large; it was split, and the rest went in INSERTs of up to {rows} rows (lower the step's rows or KB per INSERT to start there)",
                        engine_name(p.dialect)
                    )));
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
                let after = if parallel && n > 0 {
                    format!("{label} ({n} rows written by INSERTs that ran at the same time: not necessarily the first {n} rows)")
                } else {
                    format!("{label} (after {n} rows)")
                };
                r.map_err(|e| labelled(&after, e))?;
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
    let written = if staged > 0 || p.plan.iter().any(|a| matches!(a, Act::Insert { target: false, .. })) { affected.unwrap_or(staged) } else { inserted };
    Ok((written, removed))
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
            Act::User { label, sql } => (label.clone(), sql.sql().to_string(), false),
            Act::Sql { label, sql, kind } => (label.clone(), sql.clone(), created_before && *kind != ActKind::Ddl),
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
        // The user's SQL as it was classified; the load's own, classified here
        // (`check_sql` takes what a session may be asked to check).
        let checked = match act {
            Act::User { sql, .. } => sql.clone(),
            _ => Checked::new(sql.clone(), p.dialect),
        };
        let kind = checked.kind();
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
            match p.session.check_sql(&checked).await {
                Ok(true) => c.status = "ok".into(),
                Ok(false) => c.message = Some(format!("{engine} has no way to check a statement without running it")),
                Err(e) => {
                    c.status = if created_before { "warning" } else { "error" }.into();
                    c.message = Some(if created_before {
                        format!("{} (may be because an earlier statement creates or changes a table)", e.message)
                    } else {
                        e.message
                    });
                }
            }
        }
        checks.push(c);
    }
}

/// `SELECT … WHERE 1 = 0` on `table`: (exists, its columns, why not).
async fn probe(session: &Arc<dyn Session>, dialect: ConnectorKind, table: &str) -> Result<(bool, Option<Vec<String>>, Option<String>)> {
    Ok(match session.execute(Statement::internal(&sqlgen::probe_sql(dialect, table)), ExecOptions::default()).await {
        Ok(s) => match s.collect().await {
            Ok(c) => (true, c.schema.map(|s| s.fields().iter().map(|f| f.name().clone()).collect::<Vec<_>>()), None),
            Err(e) if matches!(e.kind, ErrorKind::Connection | ErrorKind::Auth | ErrorKind::Cancelled) => return Err(e.into()),
            Err(e) => (false, None, Some(e.message)),
        },
        Err(e) if matches!(e.kind, ErrorKind::Connection | ErrorKind::Auth | ErrorKind::Cancelled) => return Err(e.into()),
        Err(e) => (false, None, Some(e.message)),
    })
}

/// Parts back into a path [`split_path`] reads the same (parts with a dot
/// or a quote are quoted).
fn join_path<'a>(kind: ConnectorKind, parts: impl IntoIterator<Item = &'a str>) -> String {
    parts
        .into_iter()
        .map(|p| if p.contains(['.', '"', '`', '[', ']']) || p != p.trim() { quote_ident(kind, p) } else { p.to_string() })
        .collect::<Vec<_>>()
        .join(".")
}

/// What the top level of names is called on `kind`.
fn catalog_word(kind: ConnectorKind) -> &'static str {
    match kind {
        ConnectorKind::Snowflake | ConnectorKind::Duckdb | ConnectorKind::Mssql => "Database",
        ConnectorKind::Bigquery => "Project",
        _ => "Catalog",
    }
}

/// What a schema is called on `kind`.
fn schema_word(kind: ConnectorKind) -> &'static str {
    match kind {
        ConnectorKind::Mysql | ConnectorKind::Clickhouse => "Database",
        ConnectorKind::Bigquery => "Dataset",
        ConnectorKind::Sqlite => "Attached database",
        ConnectorKind::Oracle => "Schema (user)",
        _ => "Schema",
    }
}

/// SQL that creates a catalog (`quoted`), where SQL can.
fn create_catalog_sql(kind: ConnectorKind, quoted: &str) -> Option<String> {
    match kind {
        ConnectorKind::Snowflake | ConnectorKind::Mssql => Some(format!("CREATE DATABASE {quoted}")),
        ConnectorKind::Databricks => Some(format!("CREATE CATALOG {quoted}")),
        ConnectorKind::Duckdb => Some(format!("ATTACH 'file.duckdb' AS {quoted}")),
        _ => None,
    }
}

/// SQL that creates a schema (`path`: quoted, with its catalog).
fn create_schema_sql(kind: ConnectorKind, path: &str, name: &str) -> Option<String> {
    match kind {
        ConnectorKind::Mysql | ConnectorKind::Clickhouse => Some(format!("CREATE DATABASE {path}")),
        ConnectorKind::Sqlite => Some(format!("ATTACH DATABASE 'file.db' AS {path}")),
        ConnectorKind::Oracle => Some(format!("CREATE USER {path} IDENTIFIED BY \"password\" -- {name}'s tables need a user of that name")),
        _ => Some(format!("CREATE SCHEMA {path}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A warehouse that refuses an INSERT of more than `limit` rows as too
    /// large, and records the rows of each INSERT it takes.
    #[derive(Default)]
    struct Warehouse {
        limit: usize,
        taken: parking_lot::Mutex<Vec<usize>>,
        /// INSERTs running now, and the most at once.
        now: std::sync::atomic::AtomicUsize,
        peak: std::sync::atomic::AtomicUsize,
        /// Refuse an INSERT that runs beside another (a table with an identity column).
        one_writer: bool,
    }

    impl Warehouse {
        fn refusing_over(limit: usize) -> Arc<Self> {
            Arc::new(Warehouse { limit, ..Default::default() })
        }
        fn taken(&self) -> Vec<usize> {
            let mut t = self.taken.lock().clone();
            t.sort_unstable();
            t
        }
    }

    #[async_trait::async_trait]
    impl Session for Warehouse {
        fn kind(&self) -> ConnectorKind {
            ConnectorKind::Databricks
        }
        async fn server_version(&self) -> databrain_connector_core::Result<String> {
            Ok("test".into())
        }
        async fn ping(&self) -> databrain_connector_core::Result<()> {
            Ok(())
        }
        async fn execute(&self, stmt: Statement<'_>, _: ExecOptions) -> databrain_connector_core::Result<QueryStream> {
            use std::sync::atomic::Ordering::SeqCst;
            let rows = stmt.sql().matches("), (").count() + 1;
            let n = self.now.fetch_add(1, SeqCst) + 1;
            self.peak.fetch_max(n, SeqCst);
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            let beside = self.now.load(SeqCst) > 1;
            self.now.fetch_sub(1, SeqCst);
            if self.one_writer && (n > 1 || beside) {
                return Err(databrain_connector_core::ConnectorError::query(
                    "[DELTA_METADATA_CHANGED] MetadataChangedException: The metadata of the Delta table has been changed by a concurrent update.",
                ));
            }
            if rows > self.limit {
                return Err(databrain_connector_core::ConnectorError::query("Databricks (413 Payload Too Large): request too large"));
            }
            self.taken.lock().push(rows);
            let (tx, stream) = QueryStream::channel(1);
            tx.send(StreamEvent::Done(Default::default())).await;
            Ok(stream)
        }
        async fn list_schemas(&self) -> databrain_connector_core::Result<Vec<databrain_connector_core::SchemaInfo>> {
            Ok(vec![])
        }
        async fn list_objects(&self, _: &str) -> databrain_connector_core::Result<Vec<databrain_connector_core::DbObject>> {
            Ok(vec![])
        }
        async fn describe(&self, _: &str, _: &str) -> databrain_connector_core::Result<databrain_connector_core::ObjectDetail> {
            Err(databrain_connector_core::ConnectorError::query("no"))
        }
    }

    /// The `at`th INSERT fails (a row the table refuses); the others go to `inner`.
    struct Failing {
        inner: Arc<Warehouse>,
        at: usize,
        n: std::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl Session for Failing {
        fn kind(&self) -> ConnectorKind {
            ConnectorKind::Databricks
        }
        async fn server_version(&self) -> databrain_connector_core::Result<String> {
            Ok("test".into())
        }
        async fn ping(&self) -> databrain_connector_core::Result<()> {
            Ok(())
        }
        async fn execute(&self, stmt: Statement<'_>, opts: ExecOptions) -> databrain_connector_core::Result<QueryStream> {
            if self.n.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1 == self.at {
                return Err(databrain_connector_core::ConnectorError::query("bad row: cannot cast 'x' to INT"));
            }
            self.inner.execute(stmt, opts).await
        }
        async fn list_schemas(&self) -> databrain_connector_core::Result<Vec<databrain_connector_core::SchemaInfo>> {
            Ok(vec![])
        }
        async fn list_objects(&self, _: &str) -> databrain_connector_core::Result<Vec<databrain_connector_core::DbObject>> {
            Ok(vec![])
        }
        async fn describe(&self, _: &str, _: &str) -> databrain_connector_core::Result<databrain_connector_core::ObjectDetail> {
            Err(databrain_connector_core::ConnectorError::query("no"))
        }
    }

    fn prepared(w: Arc<dyn Session>, rows: usize, tx: Option<Tx>) -> Prepared {
        Prepared {
            dialect: ConnectorKind::Databricks,
            connection: "dbx".into(),
            table: "t".into(),
            session: w,
            exists: Some(true),
            creates: false,
            plan: vec![],
            schema: None,
            tx,
            batch: sqlgen::BatchSize { rows, bytes: 16 * 1024 * 1024 },
            permanent_stage: None,
            notices: vec![],
            may_commit: false,
            insert_only: false,
            batch_from_step: false,
        }
    }

    /// `n` rows of one column, in batches of `per` (as a query or a result
    /// on disk sends them).
    fn batches(n: i64, per: i64) -> (SchemaRef, Vec<RecordBatch>) {
        use databrain_connector_core::arrow::array::Int64Array;
        use databrain_connector_core::arrow::datatypes::{DataType, Field, Schema};
        let schema = Arc::new(Schema::new(vec![Field::new("id", DataType::Int64, true)]));
        let out = (0..n)
            .step_by(per as usize)
            .map(|from| RecordBatch::try_new(schema.clone(), vec![Arc::new(Int64Array::from((from..(from + per).min(n)).collect::<Vec<_>>()))]).unwrap())
            .collect();
        (schema, out)
    }

    /// An INSERT takes as many rows as the step says, from as many incoming
    /// batches as that needs (they come 2,000 at a time, which capped every
    /// INSERT at 2,000 rows); the last one takes what is left.
    #[tokio::test]
    async fn inserts_fill_up_across_incoming_batches() {
        let w = Warehouse::refusing_over(usize::MAX);
        let p = prepared(w.clone(), 10, None);
        let (schema, held) = batches(33, 3);
        let written = insert_rows(&p, "t", &schema, Feed { held: held.iter(), rest: None }, &CancellationToken::new(), &|_| {}).await;
        assert!(written.result.is_ok());
        assert_eq!((written.rows, written.statements, written.split), (33, 4, None));
        assert_eq!(w.taken(), vec![3, 10, 10, 10]);
    }

    /// Outside a transaction, Databricks gets up to `parallel_inserts` (4)
    /// INSERTs at once; in a transaction, one after the other.
    #[tokio::test]
    async fn databricks_inserts_run_four_at_once_outside_a_transaction() {
        use std::sync::atomic::Ordering::SeqCst;
        let (schema, held) = batches(100, 2000);
        let w = Warehouse::refusing_over(usize::MAX);
        let written =
            insert_rows(&prepared(w.clone(), 5, None), "t", &schema, Feed { held: held.iter(), rest: None }, &CancellationToken::new(), &|_| {}).await;
        assert!(written.result.is_ok());
        assert_eq!((written.rows, written.statements), (100, 20));
        assert_eq!(w.peak.load(SeqCst), 4);
        assert_eq!(w.taken(), vec![5; 20], "every row once");

        let one = Warehouse::refusing_over(usize::MAX);
        let tx = Tx { begin: "BEGIN", commit: "COMMIT", rollback: "ROLLBACK" };
        let written =
            insert_rows(&prepared(one.clone(), 5, Some(tx)), "t", &schema, Feed { held: held.iter(), rest: None }, &CancellationToken::new(), &|_| {}).await;
        assert_eq!(written.rows, 100);
        assert_eq!(one.peak.load(SeqCst), 1);
    }

    /// A failed INSERT stops the load: no new INSERT starts, those already
    /// running end, and their rows count as written.
    #[tokio::test]
    async fn a_failed_insert_lets_the_running_ones_end() {
        let w = Warehouse::refusing_over(usize::MAX);
        let (schema, held) = batches(1000, 2000);
        let p = prepared(w.clone(), 10, None);
        // The fourth INSERT fails: a row the table refuses.
        let failing = Arc::new(Failing { inner: w.clone(), at: 4, n: Default::default() });
        let p = Prepared { session: failing, ..p };
        let written = insert_rows(&p, "t", &schema, Feed { held: held.iter(), rest: None }, &CancellationToken::new(), &|_| {}).await;
        assert!(written.result.as_ref().is_err_and(|e| e.message.contains("bad row")));
        let taken = w.taken();
        assert_eq!(written.rows as usize, taken.iter().sum::<usize>(), "what was written is counted");
        // The 3 started with it, and at most a few that ended before it was seen: none started after.
        assert!(taken.len() <= 3 + 4, "no new INSERT after the failure: {}", taken.len());
        assert!(written.parallel, "the message says the rows written may not be the first ones");
    }

    /// A table that takes one writer at a time (an identity column) refuses
    /// INSERTs running at once: theirs go again, the rest one at a time;
    /// every row once, and the run log says so.
    #[tokio::test]
    async fn concurrent_insert_conflicts_go_one_at_a_time() {
        use std::sync::atomic::Ordering::SeqCst;
        let w = Arc::new(Warehouse { limit: usize::MAX, one_writer: true, ..Default::default() });
        let (schema, held) = batches(100, 2000);
        let written =
            insert_rows(&prepared(w.clone(), 10, None), "t", &schema, Feed { held: held.iter(), rest: None }, &CancellationToken::new(), &|_| {}).await;
        assert!(written.result.is_ok(), "{:?}", written.result.err());
        assert!(written.serialized);
        assert_eq!(written.rows, 100);
        assert_eq!(w.taken().iter().sum::<usize>(), 100, "every row once");
        // A conflict on an INSERT that ran alone (another writer), outside a
        // transaction: it fails as before, and the rows aren't said to be out of order.
        let alone = Arc::new(Warehouse { limit: usize::MAX, one_writer: true, ..Default::default() });
        alone.now.store(1, SeqCst);
        let (schema, one) = batches(5, 2000);
        let written = insert_rows(&prepared(alone, 10, None), "t", &schema, Feed { held: one.iter(), rest: None }, &CancellationToken::new(), &|_| {}).await;
        assert!(written.result.is_err_and(|e| e.message.contains("MetadataChangedException")));
        assert!(!written.parallel && !written.serialized);
    }

    /// A load whose only INSERT fails, or whose first one does, ran nothing
    /// beside it: its error keeps "after 0 rows", not the parallel wording.
    #[tokio::test]
    async fn a_load_that_ran_one_insert_isnt_called_parallel() {
        let w = Warehouse::refusing_over(usize::MAX);
        let (schema, one) = batches(5, 2000);
        let p = Prepared { session: Arc::new(Failing { inner: w, at: 1, n: Default::default() }), ..prepared(Warehouse::refusing_over(1), 10, None) };
        let written = insert_rows(&p, "t", &schema, Feed { held: one.iter(), rest: None }, &CancellationToken::new(), &|_| {}).await;
        assert!(written.result.is_err());
        assert!(!written.parallel);
    }

    #[test]
    fn delta_conflicts_are_recognised() {
        let e = |m: &str| EngineError::new("query", m);
        assert!(concurrent_write_conflict(&e("[DELTA_CONCURRENT_APPEND] ConcurrentAppendException: Files were added")));
        assert!(concurrent_write_conflict(&e("MetadataChangedException: The metadata of the Delta table has been changed")));
        assert!(concurrent_write_conflict(&e("[DELTA_CONCURRENT_WRITE] ConcurrentWriteException: A concurrent transaction has written new data")));
        assert!(!concurrent_write_conflict(&e("[TABLE_OR_VIEW_NOT_FOUND] t")));
        assert!(!concurrent_write_conflict(&e("[CAST_INVALID_INPUT] 'x' cannot be cast to INT")));
    }

    /// An INSERT refused as too large is sent again as two of half its rows,
    /// and the rest of the load goes in INSERTs of that size; every row once.
    /// Not in a transaction, where the refused statement may have spoilt it,
    /// and not when a single row is refused.
    #[tokio::test]
    async fn an_insert_refused_as_too_large_is_split() {
        let w = Warehouse::refusing_over(4);
        let p = prepared(w.clone(), 10, None);
        let (schema, held) = batches(25, 2000);
        let written = insert_rows(&p, "t", &schema, Feed { held: held.iter(), rest: None }, &CancellationToken::new(), &|_| {}).await;
        assert!(written.result.is_ok(), "{:?}", written.result.err());
        assert_eq!(written.rows, 25);
        assert_eq!(written.split.map(|s| s.1), Some(2), "10 refused, then 5 refused: the load went on with 2");
        assert_eq!(w.taken().iter().sum::<usize>(), 25, "every row once");
        assert!(w.taken().iter().all(|n| *n <= 4));

        let one = Warehouse::refusing_over(0);
        let written = insert_rows(&prepared(one, 10, None), "t", &schema, Feed { held: held.iter(), rest: None }, &CancellationToken::new(), &|_| {}).await;
        assert!(written.result.is_err_and(|e| e.message.contains("413")));

        let in_tx = Warehouse::refusing_over(4);
        let tx = Tx { begin: "BEGIN", commit: "COMMIT", rollback: "ROLLBACK" };
        let written =
            insert_rows(&prepared(in_tx.clone(), 10, Some(tx)), "t", &schema, Feed { held: held.iter(), rest: None }, &CancellationToken::new(), &|_| {}).await;
        assert!(written.result.is_err() && written.split.is_none());
        assert!(in_tx.taken.lock().is_empty());
    }

    /// A Databricks table as `DESCRIBE TABLE` shows it, for the bulk check.
    struct Described(Vec<(&'static str, &'static str)>);

    #[async_trait::async_trait]
    impl Session for Described {
        fn kind(&self) -> ConnectorKind {
            ConnectorKind::Databricks
        }
        async fn server_version(&self) -> databrain_connector_core::Result<String> {
            Ok("test".into())
        }
        async fn ping(&self) -> databrain_connector_core::Result<()> {
            Ok(())
        }
        async fn execute(&self, stmt: Statement<'_>, _: ExecOptions) -> databrain_connector_core::Result<QueryStream> {
            use databrain_connector_core::arrow::array::StringArray;
            use databrain_connector_core::arrow::datatypes::{DataType, Field, Schema};
            assert!(stmt.sql().starts_with("DESCRIBE TABLE `main`.`s`.`t`"), "{}", stmt.sql());
            let schema = Arc::new(Schema::new(vec![Field::new("col_name", DataType::Utf8, true), Field::new("data_type", DataType::Utf8, true)]));
            let rows = self.0.iter().copied().chain([("", ""), ("# Partition Information", ""), ("id", "string")]);
            let (n, t): (Vec<&str>, Vec<&str>) = rows.unzip();
            let b = RecordBatch::try_new(schema.clone(), vec![Arc::new(StringArray::from(n)), Arc::new(StringArray::from(t))]).unwrap();
            let (tx, stream) = QueryStream::channel(4);
            tx.send(StreamEvent::Schema(schema)).await;
            tx.send(StreamEvent::Batch(b)).await;
            tx.send(StreamEvent::Done(Default::default())).await;
            Ok(stream)
        }
        async fn list_schemas(&self) -> databrain_connector_core::Result<Vec<databrain_connector_core::SchemaInfo>> {
            Ok(vec![])
        }
        async fn list_objects(&self, _: &str) -> databrain_connector_core::Result<Vec<databrain_connector_core::DbObject>> {
            Ok(vec![])
        }
        async fn describe(&self, _: &str, _: &str) -> databrain_connector_core::Result<databrain_connector_core::ObjectDetail> {
            Err(databrain_connector_core::ConnectorError::query("no"))
        }
    }

    /// Databricks takes the rows in bulk when every column goes into its
    /// table's type as its INSERT literal would (names matched as INSERTs
    /// match them; the partition section isn't a column); otherwise INSERTs, saying why.
    #[tokio::test]
    async fn databricks_bulk_needs_columns_that_cast_as_inserts_do() {
        let (schema, _) = batches(1, 1);
        let p = |cols| Prepared { session: Arc::new(Described(cols)), table: "main.s.t".into(), ..prepared(Warehouse::refusing_over(1), 10, None) };
        let ok = bulk_plan(&p(vec![("ID", "bigint"), ("other", "string")]), "main.s.t", &schema, &CancellationToken::new()).await;
        match ok {
            Ok(Some(Bulk::Staged(s))) => assert!(s.sql.contains("CAST(CAST(c0 AS DECIMAL(38, 10)) AS bigint)"), "{}", s.sql),
            _ => panic!("a bulk load"),
        }
        let no = bulk_plan(&p(vec![("id", "string")]), "main.s.t", &schema, &CancellationToken::new()).await;
        assert!(matches!(no, Err(CopyCheck::Inserts(why)) if why.contains("into a string column")));
        let missing = bulk_plan(&p(vec![("other", "bigint")]), "main.s.t", &schema, &CancellationToken::new()).await;
        assert!(matches!(missing, Err(CopyCheck::Inserts(why)) if why.contains("no column id")));
        // A step that asks for INSERTs gets them.
        let only = Prepared { insert_only: true, ..p(vec![("id", "bigint")]) };
        assert!(matches!(bulk_plan(&only, "main.s.t", &schema, &CancellationToken::new()).await, Ok(None)));
    }

    /// The files a bulk load stages, and what it finishes with.
    #[derive(Default)]
    struct Staging {
        files: parking_lot::Mutex<Vec<usize>>,
        finished: std::sync::atomic::AtomicBool,
        fail_at: Option<usize>,
    }

    struct StagingWriter(Arc<Staging>);

    #[async_trait::async_trait]
    impl databrain_connector_core::BulkWriter for StagingWriter {
        async fn write(&mut self, data: Vec<u8>) -> databrain_connector_core::Result<()> {
            let mut f = self.0.files.lock();
            if self.0.fail_at == Some(f.len()) {
                return Err(databrain_connector_core::ConnectorError::query("Databricks Files API (403): no WRITE VOLUME"));
            }
            assert!(data.starts_with(b"PAR1"), "a Parquet file");
            f.push(data.len());
            Ok(())
        }
        async fn finish(self: Box<Self>) -> databrain_connector_core::Result<Option<u64>> {
            self.0.finished.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(None)
        }
    }

    /// Rows arriving 2,000 at a time go into files of 250,000 rows, then one
    /// statement; a failed upload inserts nothing (the writer is dropped,
    /// which deletes its files).
    #[tokio::test]
    async fn staged_rows_go_in_large_files_then_one_statement() {
        let (schema, held) = batches(600_000, 2000);
        let stage = sqlgen::DbxStage::new("t", &schema, &["bigint".to_string()]);
        let st = Arc::new(Staging::default());
        let w = staged_rows(&stage, Box::new(StagingWriter(st.clone())), Feed { held: held.iter(), rest: None }, &|_| {}).await;
        assert!(w.result.is_ok());
        assert_eq!((w.rows, w.statements), (600_000, 1), "the rows sent, when the server doesn't say");
        assert_eq!(st.files.lock().len(), 3, "250,000 + 250,000 + 100,000 rows");
        assert!(st.finished.load(std::sync::atomic::Ordering::SeqCst));

        let st = Arc::new(Staging { fail_at: Some(1), ..Default::default() });
        let w = staged_rows(&stage, Box::new(StagingWriter(st.clone())), Feed { held: held.iter(), rest: None }, &|_| {}).await;
        assert!(w.result.is_err_and(|e| e.message.contains("WRITE VOLUME")));
        assert_eq!(w.rows, 0, "nothing inserted");
        assert!(!st.finished.load(std::sync::atomic::Ordering::SeqCst));
    }

    /// MySQL INSERTs stay under the server's max_allowed_packet: the
    /// default quietly, a size the step set with a note; unknown, 1 MB.
    #[test]
    fn mysql_inserts_fit_the_servers_packet() {
        const MB: usize = 1024 * 1024;
        let size = |bytes| sqlgen::BatchSize { rows: 5000, bytes };
        // 64 MB (MySQL 8.0): the 3 MB default fits.
        let mut b = size(3 * MB);
        assert_eq!(fit_packet(&mut b, Some(64 * MB), false), None);
        assert_eq!(b.bytes, 3 * MB);
        // A hosted server at 1 MB: under it, quietly.
        let mut b = size(3 * MB);
        assert_eq!(fit_packet(&mut b, Some(MB), false), None);
        assert!(b.bytes < MB && b.bytes > MB / 2, "{}", b.bytes);
        // 4 MB (5.7) and the step's own 16 MB: lowered, with a note.
        let mut b = size(16 * MB);
        let note = fit_packet(&mut b, Some(4 * MB), true).unwrap();
        assert!(b.bytes < 4 * MB && note.contains("max_allowed_packet is 4096 KB"), "{note}");
        // The server didn't say: 1 MB, or the step's own size.
        let mut b = size(3 * MB);
        assert_eq!(fit_packet(&mut b, None, false), None);
        assert_eq!(b.bytes, MB);
        let mut b = size(8 * MB);
        fit_packet(&mut b, None, true);
        assert_eq!(b.bytes, 8 * MB);
    }

    /// An Oracle session: array INSERTs (unless `arrays` is off, as an old
    /// thin helper), and INSERT ALLs; records the rows of each, and fails
    /// the array call `fail_at` (1-based).
    #[derive(Default)]
    struct OracleLike {
        /// `NLS_NUMERIC_CHARACTERS` (None: the query fails).
        nls: Option<&'static str>,
        /// Each array call's columns.
        calls: parking_lot::Mutex<Vec<Vec<Vec<Option<String>>>>>,
        no_arrays: bool,
        fail_at: Option<usize>,
        /// Refuse a call of more rows as too large (as an HTTP 413).
        too_large_over: Option<usize>,
        /// The rows the server says each call inserted (else the rows sent).
        says: Option<u64>,
        arrays: parking_lot::Mutex<Vec<usize>>,
        inserts: parking_lot::Mutex<Vec<usize>>,
    }

    #[async_trait::async_trait]
    impl Session for OracleLike {
        fn kind(&self) -> ConnectorKind {
            ConnectorKind::Oracle
        }
        async fn server_version(&self) -> databrain_connector_core::Result<String> {
            Ok("test".into())
        }
        async fn ping(&self) -> databrain_connector_core::Result<()> {
            Ok(())
        }
        async fn execute(&self, stmt: Statement<'_>, _: ExecOptions) -> databrain_connector_core::Result<QueryStream> {
            if stmt.sql().contains("nls_session_parameters") {
                use databrain_connector_core::arrow::array::StringArray;
                use databrain_connector_core::arrow::datatypes::{DataType, Field, Schema};
                let Some(v) = self.nls else { return Err(databrain_connector_core::ConnectorError::query("ORA-00942")) };
                let schema = Arc::new(Schema::new(vec![Field::new("VALUE", DataType::Utf8, true)]));
                let b = RecordBatch::try_new(schema.clone(), vec![Arc::new(StringArray::from(vec![v]))]).unwrap();
                let (tx, stream) = QueryStream::channel(4);
                tx.send(StreamEvent::Schema(schema)).await;
                tx.send(StreamEvent::Batch(b)).await;
                tx.send(StreamEvent::Done(Default::default())).await;
                return Ok(stream);
            }
            assert!(stmt.sql().starts_with("INSERT ALL"), "{}", stmt.sql());
            self.inserts.lock().push(stmt.sql().matches(" INTO ").count());
            let (tx, stream) = QueryStream::channel(1);
            tx.send(StreamEvent::Done(Default::default())).await;
            Ok(stream)
        }
        async fn insert_array(
            &self,
            sql: &str,
            columns: Vec<Vec<Option<String>>>,
            _: &[&str],
            _: CancellationToken,
        ) -> databrain_connector_core::Result<Option<u64>> {
            if self.no_arrays {
                return Ok(None);
            }
            assert!(sql.starts_with("INSERT INTO") && sql.contains("VALUES (:1)"), "{sql}");
            if self.too_large_over.is_some_and(|m| columns[0].len() > m) {
                return Err(databrain_connector_core::ConnectorError::query("Snowflake (413 Payload Too Large): request too large"));
            }
            let mut a = self.arrays.lock();
            if self.fail_at == Some(a.len() + 1) {
                return Err(databrain_connector_core::ConnectorError::query("ORA-01722: invalid number"));
            }
            a.push(columns[0].len());
            let n = columns[0].len();
            self.calls.lock().push(columns);
            Ok(Some(self.says.unwrap_or(n as u64)))
        }
        fn insert_array_possible(&self) -> bool {
            true
        }
        async fn list_schemas(&self) -> databrain_connector_core::Result<Vec<databrain_connector_core::SchemaInfo>> {
            Ok(vec![])
        }
        async fn list_objects(&self, _: &str) -> databrain_connector_core::Result<Vec<databrain_connector_core::DbObject>> {
            Ok(vec![])
        }
        async fn describe(&self, _: &str, _: &str) -> databrain_connector_core::Result<databrain_connector_core::ObjectDetail> {
            Err(databrain_connector_core::ConnectorError::query("no"))
        }
    }

    fn oracle(s: Arc<OracleLike>) -> Prepared {
        Prepared { dialect: ConnectorKind::Oracle, session: s, ..prepared(Warehouse::refusing_over(1), 100, None) }
    }

    /// Oracle loads go in array INSERTs of up to 10,000 rows, taken from as
    /// many incoming batches as that needs; every row once.
    #[tokio::test]
    async fn oracle_loads_bind_arrays_of_rows() {
        let s = Arc::new(OracleLike { nls: Some(".,"), ..Default::default() });
        let (schema, held) = batches(25_000, 2000);
        let ai = sqlgen::ArrayInsert::new(ConnectorKind::Oracle, "t", &schema).unwrap();
        let w = array_rows(&oracle(s.clone()), "t", &schema, &ai, Feed { held: held.iter(), rest: None }, &CancellationToken::new(), &|_| {}, &|_| {}).await;
        assert!(w.result.is_ok(), "{:?}", w.result.err());
        assert_eq!((w.rows, w.statements), (25_000, 3));
        assert_eq!(*s.arrays.lock(), vec![10_000, 10_000, 5_000]);
        assert!(s.inserts.lock().is_empty());
    }

    /// A session that turns array INSERTs down (a thin helper older than
    /// them) gets INSERT ALLs instead, every row once.
    #[tokio::test]
    async fn oracle_without_arrays_gets_inserts() {
        let s = Arc::new(OracleLike { nls: Some(".,"), no_arrays: true, ..Default::default() });
        let (schema, held) = batches(2_500, 2000);
        let ai = sqlgen::ArrayInsert::new(ConnectorKind::Oracle, "t", &schema).unwrap();
        let w = array_rows(&oracle(s.clone()), "t", &schema, &ai, Feed { held: held.iter(), rest: None }, &CancellationToken::new(), &|_| {}, &|_| {}).await;
        assert!(w.result.is_ok(), "{:?}", w.result.err());
        assert_eq!(w.rows, 2_500);
        assert_eq!(s.inserts.lock().iter().sum::<usize>(), 2_500);
        assert!(s.arrays.lock().is_empty());
    }

    /// A failed array INSERT ends the load with its error; the calls before
    /// it count as written.
    #[tokio::test]
    async fn a_failed_array_insert_stops_the_load() {
        let s = Arc::new(OracleLike { nls: Some(".,"), fail_at: Some(2), ..Default::default() });
        let (schema, held) = batches(25_000, 2000);
        let ai = sqlgen::ArrayInsert::new(ConnectorKind::Oracle, "t", &schema).unwrap();
        let w = array_rows(&oracle(s.clone()), "t", &schema, &ai, Feed { held: held.iter(), rest: None }, &CancellationToken::new(), &|_| {}, &|_| {}).await;
        assert!(w.result.is_err_and(|e| e.message.contains("ORA-01722") && e.message.contains("(in rows 10,001 to 20,000 of the step's rows)")));
        assert_eq!(w.rows, 10_000);
        assert_eq!(*s.arrays.lock(), vec![10_000]);
    }

    /// Numbers go with the session's decimal separator; a step's own size
    /// caps the calls; the run log says "array INSERTs" once one went in, or
    /// why not; a separator that can't be read gets INSERT ALLs.
    #[tokio::test]
    async fn oracle_array_calls_follow_the_session_and_the_step() {
        use databrain_connector_core::arrow::array::Float64Array;
        use databrain_connector_core::arrow::datatypes::{DataType, Field, Schema};
        let schema = Arc::new(Schema::new(vec![Field::new("f", DataType::Float64, true)]));
        let batch = RecordBatch::try_new(schema.clone(), vec![Arc::new(Float64Array::from(vec![2.5, 1.0, 0.125]))]).unwrap();
        let ai = sqlgen::ArrayInsert::new(ConnectorKind::Oracle, "t", &schema).unwrap();
        let notes = parking_lot::Mutex::new(Vec::<String>::new());
        let note = |m: String| notes.lock().push(m);
        // A German session.
        let s = Arc::new(OracleLike { nls: Some(",."), ..Default::default() });
        let w = array_rows(&oracle(s.clone()), "t", &schema, &ai, Feed { held: [batch.clone()].iter(), rest: None }, &CancellationToken::new(), &|_| {}, &note)
            .await;
        assert!(w.result.is_ok());
        assert_eq!(s.calls.lock()[0][0], vec![Some("2,5".into()), Some("1,0".into()), Some("0,125".into())]);
        assert!(notes.lock()[0].starts_with("array INSERTs, up to 10,000 rows"), "{:?}", notes.lock());
        // The step's own 2 rows per INSERT: calls of 2 rows.
        let s = Arc::new(OracleLike { nls: Some(".,"), ..Default::default() });
        let p = Prepared { batch_from_step: true, batch: sqlgen::BatchSize { rows: 2, bytes: 1 << 20 }, ..oracle(s.clone()) };
        array_rows(&p, "t", &schema, &ai, Feed { held: [batch.clone()].iter(), rest: None }, &CancellationToken::new(), &|_| {}, &|_| {}).await;
        assert_eq!(*s.arrays.lock(), vec![2, 1]);
        // The separator can't be read: INSERT ALLs, and the log says why.
        notes.lock().clear();
        let s = Arc::new(OracleLike::default());
        let w = array_rows(&oracle(s.clone()), "t", &schema, &ai, Feed { held: [batch].iter(), rest: None }, &CancellationToken::new(), &|_| {}, &note).await;
        assert_eq!(w.rows, 3);
        assert!(s.arrays.lock().is_empty() && s.inserts.lock().iter().sum::<usize>() == 3);
        assert!(notes.lock()[0].contains("decimal separator couldn't be read"), "{:?}", notes.lock());
    }

    /// A call refused as too large wrote nothing: half its rows go again
    /// and the rest of the load uses that size; every row once. The rows
    /// the server says it inserted count, and the run log says when they
    /// differ from those sent.
    #[tokio::test]
    async fn array_calls_refused_as_too_large_are_halved() {
        let s = Arc::new(OracleLike { nls: Some(".,"), too_large_over: Some(3_000), ..Default::default() });
        let (schema, held) = batches(25_000, 2000);
        let ai = sqlgen::ArrayInsert::new(ConnectorKind::Oracle, "t", &schema).unwrap();
        let w = array_rows(&oracle(s.clone()), "t", &schema, &ai, Feed { held: held.iter(), rest: None }, &CancellationToken::new(), &|_| {}, &|_| {}).await;
        assert!(w.result.is_ok(), "{:?}", w.result.err());
        assert_eq!(w.rows, 25_000);
        assert_eq!(s.arrays.lock().iter().sum::<usize>(), 25_000, "every row once");
        assert!(s.arrays.lock().iter().all(|n| *n <= 3_000));
        assert_eq!(w.split.map(|x| x.1), Some(2_500));

        let notes = parking_lot::Mutex::new(Vec::<String>::new());
        let s = Arc::new(OracleLike { nls: Some(".,"), says: Some(7), ..Default::default() });
        let (schema, held) = batches(10, 2000);
        let w =
            array_rows(&oracle(s), "t", &schema, &ai, Feed { held: held.iter(), rest: None }, &CancellationToken::new(), &|_| {}, &|m| notes.lock().push(m))
                .await;
        assert_eq!(w.rows, 7, "the server's count");
        assert!(notes.lock().iter().any(|n| n.contains("said it inserted 7 rows of the 10")), "{:?}", notes.lock());
    }

    /// The appender takes columns whose types it keeps as they are; others
    /// (a time it would drop, digits it would cut) send the rows by INSERTs.
    #[test]
    fn the_appender_takes_types_it_keeps() {
        use databrain_connector_core::arrow::datatypes::{DataType as D, Field, Schema, TimeUnit};
        let schema = |t: D| Arc::new(Schema::new(vec![Field::new("c", t, true)]));
        for t in [
            D::Int64,
            D::UInt8,
            D::Float64,
            D::Utf8,
            D::LargeUtf8,
            D::Binary,
            D::Boolean,
            D::Date32,
            D::Timestamp(TimeUnit::Nanosecond, None),
            D::Timestamp(TimeUnit::Microsecond, Some("UTC".into())),
            D::Decimal128(38, 10),
        ] {
            assert!(appender_fits(&schema(t.clone())).is_ok(), "{t:?}");
        }
        for t in [D::Date64, D::Timestamp(TimeUnit::Nanosecond, Some("UTC".into())), D::Decimal256(50, 2), D::Time32(TimeUnit::Second), D::Float16] {
            assert!(appender_fits(&schema(t.clone())).is_err_and(|w| w.contains("goes by INSERTs")), "{t:?}");
        }
    }

    #[test]
    fn failing_rows_are_named_among_the_steps_rows() {
        assert_eq!(name_failing_row("ORA-01722: invalid number", 20_000, 10_000), "ORA-01722: invalid number (in rows 20,001 to 30,000 of the step's rows)");
        assert_eq!(grouped(1_234_567), "1,234,567");
        assert_eq!(grouped(999), "999");
    }

    /// What counts as refused for its size (sent again smaller), and what doesn't.
    #[test]
    fn too_large_is_a_refusal_for_size_only() {
        let e = |m: &str| EngineError::new("query", m);
        for m in [
            "Databricks (413 Payload Too Large): request too large",
            "Got a packet bigger than 'max_allowed_packet' bytes",
            "The statement exceeds the maximum size of 16 MiB",
            "[REQUEST_LIMIT_EXCEEDED] message size 17000000 exceeds the limit",
            "Query is too large",
        ] {
            assert!(rejected_as_too_large(&e(m)), "{m}");
        }
        for m in [
            "[TABLE_OR_VIEW_NOT_FOUND] The table t cannot be found",
            "value too long for type character varying(10)",
            "Numeric value out of range",
            "duplicate key value violates unique constraint",
            "connection reset",
        ] {
            assert!(!rejected_as_too_large(&e(m)), "{m}");
        }
        assert!(!rejected_as_too_large(&EngineError::new("cancelled", "request too large")));
    }

    /// Redshift loads go in INSERTs (no COPY, and no SAVEPOINT for its
    /// check, which aborts a Redshift transaction), however the connection
    /// is recognised as Redshift; plain Postgres may use COPY.
    #[test]
    fn redshift_loads_never_use_copy() {
        use databrain_auth::AuthMethod;
        use databrain_connector_core::ConnectionConfig;
        let cfg = |host: &str, flavor: Option<&str>| {
            let mut c = ConnectionConfig::new(ConnectorKind::Postgres, AuthMethod::Password { user: "u".into() });
            c.host = Some(host.into());
            if let Some(f) = flavor {
                c.options.insert("flavor".into(), f.into());
            }
            c
        };
        // Refused: the host alone, the flavor in any case, found at connect time, or the step's choice.
        assert!(inserts_only(false, &cfg("x.abc.us-east-1.redshift.amazonaws.com", None), None));
        assert!(inserts_only(false, &cfg("wg.123.eu-west-1.redshift-serverless.amazonaws.com", None), None));
        assert!(inserts_only(false, &cfg("db.internal", Some("Redshift")), None));
        assert!(inserts_only(false, &cfg("db.internal", Some("redshift")), None));
        assert!(inserts_only(false, &cfg("tunnel.local", None), Some("redshift")));
        assert!(inserts_only(true, &cfg("db.internal", None), None));
        // Allowed: plain Postgres.
        assert!(!inserts_only(false, &cfg("db.internal", None), None));
        assert!(!inserts_only(false, &cfg("db.internal", Some("postgres")), None));
    }

    /// COPY only where it writes what the INSERTs would: not into a
    /// GENERATED ALWAYS identity column (an INSERT refuses it, COPY would
    /// write it) or a generated column; a BY DEFAULT identity takes the
    /// value either way.
    #[test]
    fn copy_skips_identity_always_and_generated_columns() {
        use databrain_connector_core::arrow::datatypes::DataType;
        let col = |name: &str, identity: &str, generated: &str| CopyColumn {
            relkind: "r".into(),
            rules: false,
            name: name.into(),
            ty: "bigint".into(),
            rls: false,
            identity: identity.into(),
            generated: generated.into(),
        };
        let schema: SchemaRef = Arc::new(Schema::new(vec![Field::new("id", DataType::Int64, true), Field::new("v", DataType::Int64, true)]));
        assert!(copy_fits(&[col("id", "", ""), col("v", "", "")], &schema).is_ok());
        assert!(copy_fits(&[col("id", "d", ""), col("v", "", "")], &schema).is_ok());
        assert!(copy_fits(&[col("id", "a", ""), col("v", "", "")], &schema).unwrap_err().contains("GENERATED ALWAYS AS IDENTITY"));
        assert!(copy_fits(&[col("id", "", ""), col("v", "", "s")], &schema).unwrap_err().contains("generated column"));
        // A table column the rows don't have isn't checked.
        let only_v: SchemaRef = Arc::new(Schema::new(vec![Field::new("v", DataType::Int64, true)]));
        assert!(copy_fits(&[col("id", "a", ""), col("v", "", "")], &only_v).is_ok());
        // No columns read, rules, RLS: INSERTs.
        assert!(copy_fits(&[], &schema).is_err());
        let rls = CopyColumn { rls: true, ..col("id", "", "") };
        assert!(copy_fits(&[rls, col("v", "", "")], &schema).unwrap_err().contains("row-level security"));
    }

    #[test]
    fn load_sql_is_checked_for_transaction_control() {
        use ConnectorKind::*;
        let check = |sql: &str, k: ConnectorKind| check_user_sql(&[Checked::new(sql, k)], k);
        for (sql, k) in [
            ("COMMIT", Postgres),
            ("rollback", Mysql),
            ("BEGIN TRANSACTION", Mssql),
            ("start transaction", Mysql),
            ("SET autocommit = 1", Mysql),
            ("set implicit_transactions on", Mssql),
            ("savepoint a", Postgres),
            ("ALTER SESSION SET AUTOCOMMIT = FALSE", Snowflake),
            ("XA COMMIT 'x'", Mysql),
            ("COMMIT\nINSERT INTO t VALUES (1)", Mssql),
            ("IF @@TRANCOUNT > 0 COMMIT", Mssql),
            ("LOCK TABLES t WRITE", Mysql),
            ("PREPARE TRANSACTION 'x'", Postgres),
            ("BEGIN pkg.run; COMMIT; END;", Oracle),
        ] {
            assert!(check(sql, k).is_err(), "{k:?}: {sql}");
        }
        for (sql, k) in [
            ("update t set a = 1 where id = 2", Postgres),
            ("create index i on t (a)", Postgres),
            ("select 1", Mssql),
            ("update settings set autocommit_mode = 1 where id = 2", Postgres),
            ("ALTER SESSION SET TIMEZONE = 'UTC'", Snowflake),
            ("insert into log (what) values ('commit')", Postgres),
        ] {
            assert!(!check(sql, k).unwrap(), "{k:?}: {sql}");
        }
        // A procedure may commit by itself.
        for (sql, k) in [("call refresh_totals()", Postgres), ("exec dbo.rebuild", Mssql), ("BEGIN pkg.run; END;", Oracle)] {
            assert!(check(sql, k).unwrap(), "{k:?}: {sql}");
        }
    }

    /// Only a load that ran in one transaction, rolled back before its
    /// commit was sent, with no before/after SQL that may commit on its own,
    /// kept nothing.
    #[test]
    fn kept_nothing_only_when_rolled_back_as_a_whole() {
        assert!(kept_nothing(true, false, false), "rolled back");
        assert!(!kept_nothing(false, false, false), "no transaction: each statement that ended was kept");
        assert!(!kept_nothing(true, true, false), "before/after SQL that may commit");
        assert!(!kept_nothing(true, false, true), "the commit was sent: the server may have committed");
    }

    #[test]
    fn transactions_per_engine() {
        let ins = Act::Insert { label: String::new(), table: "t".into(), target: true };
        let ddl = Act::Sql { label: String::new(), sql: "CREATE TABLE t (a int)".into(), kind: ActKind::Ddl };
        let user_ddl = Act::User { label: String::new(), sql: Checked::new("alter table t add b int", ConnectorKind::Mysql) };
        let stage = Act::Sql { label: String::new(), sql: "CREATE TEMPORARY TABLE s AS SELECT * FROM t WHERE 1 = 0".into(), kind: ActKind::Stage };
        assert!(transaction(ConnectorKind::Postgres, &[ddl.clone(), ins.clone()]).is_some());
        assert!(transaction(ConnectorKind::Mssql, std::slice::from_ref(&ddl)).is_some());
        assert!(transaction(ConnectorKind::Mysql, &[stage.clone(), ins.clone()]).is_some(), "temporary tables don't commit");
        assert!(transaction(ConnectorKind::Mysql, &[ddl, ins.clone()]).is_none());
        assert!(transaction(ConnectorKind::Mysql, &[user_ddl]).is_none());
        // MySQL statements that commit without being DDL: no transaction
        // (a dry run checks the plan instead of running it); reads, row
        // changes and harmless session values keep it.
        let user = |sql: &str| Act::User { label: String::new(), sql: Checked::new(sql, ConnectorKind::Mysql) };
        for sql in [
            "SET PASSWORD = 'x'",
            "/* rotate */ SET PASSWORD = 'x'",
            "SET -- now\n PASSWORD FOR u = 'x'",
            "SET autocommit = 1",
            "SET @@autocommit = 1",
            "SET SESSION autocommit = 1",
            "RESET QUERY CACHE",
            "LOAD INDEX INTO CACHE t",
            "/* warm */ LOAD INDEX INTO CACHE t",
            "CREATE USER u",
            "GRANT SELECT ON t TO u",
            "ANALYZE TABLE t",
        ] {
            assert!(transaction(ConnectorKind::Mysql, &[user(sql), ins.clone()]).is_none(), "{sql}");
        }
        for sql in [
            "update t set a = 1 where id = 2",
            "select 1",
            "SET @batch = 1",
            "SET NAMES utf8mb4",
            "SET @note = 'autocommit'",
            "/* x */ SET @batch = 2",
            "LOAD DATA LOCAL INFILE 'f' INTO TABLE t",
        ] {
            assert!(transaction(ConnectorKind::Mysql, &[user(sql), ins.clone()]).is_some(), "{sql}");
        }
        // Elsewhere those don't commit: the transaction stays.
        assert!(transaction(ConnectorKind::Postgres, &[user("RESET search_path"), ins.clone()]).is_some());
        assert!(transaction(ConnectorKind::Oracle, std::slice::from_ref(&ins)).is_none());
        // Snowflake: an append runs in one; any DDL, a staging table too, commits.
        assert!(transaction(ConnectorKind::Snowflake, std::slice::from_ref(&ins)).is_some());
        assert!(transaction(ConnectorKind::Snowflake, &[stage, ins]).is_none());
    }

    #[test]
    fn columns_match_ignoring_case() {
        let t = vec!["ID".to_string(), "Name".to_string(), "name".to_string()];
        assert_eq!(match_column("id", &t).unwrap(), "ID");
        assert_eq!(match_column("name", &t).unwrap(), "name", "exact first");
        assert!(match_column("x", &t).is_none());
        assert_eq!(
            user_statements(Some("delete from t where d = 1;\n\n update s set x = 1;"), ConnectorKind::Postgres).iter().map(Checked::sql).collect::<Vec<_>>(),
            vec!["delete from t where d = 1", "update s set x = 1"]
        );
        assert!(user_statements(Some("  "), ConnectorKind::Postgres).is_empty());
    }
}
