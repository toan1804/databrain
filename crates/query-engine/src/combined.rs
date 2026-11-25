//! Combined queries: one statement whose parts run on other connections and
//! are joined in DuckDB (docs/design/cross-connection-queries.md, phase 1).
//!
//! The statement is read by [`parse_combined`]; [`QueryEngine::plan_combined`]
//! decides what each server is asked (the one place: phase 2's pushdown
//! changes `PartPlan::sent` there and nothing downstream) and checks
//! everything before anything runs; [`QueryEngine::run_combined`] runs the
//! parts (at most [`PARALLEL_PARTS`] at once, each on a session of its own),
//! keeps each part's rows as an output of the tab, and runs the outer query
//! on the Results (DuckDB) connection over those outputs.

use std::ops::Range;
use std::sync::atomic::AtomicUsize;

use databrain_connector_core::ConnectorKind;
use databrain_connector_core::sql::{Combined, ConnectionRef, looks_combined, parse_combined};

use super::*;
use crate::guard::Intent;
use crate::outputs::NewOutput;

/// Setting: the rows a part may have before the statement fails (0: no
/// limit). A part is read whole; past this it is refused, never cut.
pub const PART_ROWS_SETTING: &str = "combined_part_rows";
/// Default for [`PART_ROWS_SETTING`].
pub const PART_ROWS_DEFAULT: usize = 1_000_000;
/// Parts run at once.
const PARALLEL_PARTS: usize = 4;
/// [`EngineError::code`] of a part with more rows than the limit.
pub const PART_TOO_LARGE: &str = "part_too_large";

/// What one part asks its server.
#[derive(Debug, Clone)]
pub(crate) struct PartPlan {
    pub name: String,
    pub connection_id: String,
    pub connection_name: String,
    pub kind: ConnectorKind,
    /// What the server is asked, classified: in phase 1 the part as written
    /// with its parameters bound.
    pub sent: Checked,
    /// What pushdown added to the part (phase 2; none yet), for the UI and
    /// the history.
    #[allow(dead_code)]
    pub pushed: Vec<String>,
    /// The part's SQL in the statement (byte range): error positions.
    pub span: Range<usize>,
    /// `sent` is the text as written (no parameter changed it): a
    /// server's position in it is one in the statement.
    pub as_written: bool,
}

/// A combined statement, checked: its parts and how the outer query reads them.
#[derive(Debug, Clone)]
pub(crate) struct CombinedPlan {
    pub parts: Vec<PartPlan>,
    parsed: Combined,
    /// The statement as written (spans are into it).
    text: String,
}

impl CombinedPlan {
    /// The outer query, each part read from output `handles[i]`.
    fn outer(&self, handles: &[String]) -> String {
        let mut i = 0;
        self.parsed.outer_sql(&self.text, |_| {
            let h = &handles[i];
            i += 1;
            format!("(SELECT * FROM results.{h})")
        })
    }
}

/// Where a part reports, as the combined statement of the run it belongs to.
#[derive(Debug, Clone)]
pub(crate) struct PartRun {
    pub job_id: String,
    pub index: usize,
    /// Rows read so far by all the statement's parts.
    pub rows: Arc<AtomicUsize>,
}

/// 1-based character position of byte `at` of `text` (an error's position).
fn position_at(text: &str, at: usize) -> u32 {
    text.get(..at).map_or(0, |t| t.chars().count()) as u32 + 1
}

fn refused(text: &str, at: usize, message: impl Into<String>) -> EngineError {
    EngineError { position: Some(position_at(text, at)), ..EngineError::new("invalid", message) }
}

impl QueryEngine {
    /// `text` (one statement, as written) as a combined query, checked, or
    /// `None` when it isn't one. Refused, with the part it is about, unless
    /// every connection exists, every part is one read there (the
    /// connection's own rules: read-only, its dialect), the outer query is a
    /// read in DuckDB, and no part is on `tab_connection` while the tab's
    /// session there has a transaction open (the part would not see it).
    pub(crate) fn plan_combined(
        &self,
        text: &str,
        params: &HashMap<String, databrain_connector_core::sql::ParamValue>,
        origin: Origin,
        tab_connection: &str,
        session_key: &str,
    ) -> Result<Option<CombinedPlan>> {
        if !looks_combined(text) {
            return Ok(None);
        }
        // Who may run one is decided before any connection is looked up: a
        // refusal names none (the assistant sees no connection's name).
        let elsewhere = match origin {
            Origin::User | Origin::Cli => {
                let notebook = matches!(SessionRole::parse(session_key), SessionRole::Notebook { .. });
                (notebook || session_key.starts_with("dash:")).then_some(if notebook { "a notebook" } else { "a dashboard" })
            }
            o => Some(o.as_str()),
        };
        if let Some(from) = elsewhere
            && parse_combined(text, &mut |_| Ok(ConnectorKind::Postgres)).is_ok_and(|c| c.is_some())
        {
            return Err(EngineError::new(
                "unsupported",
                format!("A query with parts on other connections (@connection) runs from a query tab or `dbr` for now, not from {from}"),
            ));
        }
        let all = self.workspace.list_connections()?;
        let mut found: HashMap<ConnectionRef, ConnectionProfile> = HashMap::new();
        let parsed = parse_combined(text, &mut |r| {
            let p = match r {
                ConnectionRef::Id(id) => all.iter().find(|c| c.id == *id).ok_or_else(|| format!("no connection has the id {id:?}"))?,
                ConnectionRef::Name(n) => databrain_workspace::find_connection(&all, n, "")?,
            };
            found.insert(r.clone(), p.clone());
            Ok(p.config.kind)
        })
        .map_err(|e| refused(text, e.position, e.message))?;
        let Some(parsed) = parsed else { return Ok(None) };
        if elsewhere.is_some() {
            return Err(EngineError::new("unsupported", "A query with parts on other connections (@connection) runs from a query tab or `dbr` for now"));
        }
        let mut missing: Vec<String> = Vec::new();
        let mut parts = Vec::with_capacity(parsed.parts.len());
        for p in &parsed.parts {
            let profile = &found[&p.connection];
            let at = p.sql_span.start;
            let on = format!("Part `{}` (on \"{}\")", p.name, profile.name);
            let sent = match Checked::new(p.sql.clone(), p.kind).bind(p.kind, params) {
                Ok(c) => c,
                Err(names) => {
                    missing.extend(names.into_iter().filter(|n| !missing.contains(n)).collect::<Vec<_>>());
                    continue;
                }
            };
            if sent.kind() != StatementKind::Read {
                return Err(refused(
                    text,
                    at,
                    format!("{on} must be a query that only reads: this one is {} ({})", sent.classification().keyword, kind_words(sent.kind())),
                ));
            }
            let one = [PlannedStatement { index: 0, start: 0, end: 0, checked: sent.clone(), combined: false }];
            Self::check_intent(profile, &Intent::Run { plan: &one, confirmed: true })
                .map_err(|e| EngineError { message: format!("{on}: {}", e.message), ..e })?;
            if profile.id == tab_connection && self.in_transaction(tab_connection, session_key) {
                return Err(refused(
                    text,
                    at,
                    format!("{on} runs on a session of its own, which wouldn't see this tab's open transaction: commit or roll it back first"),
                ));
            }
            let as_written = sent.sql() == p.sql;
            parts.push(PartPlan {
                name: p.name.clone(),
                connection_id: profile.id.clone(),
                connection_name: profile.name.clone(),
                kind: p.kind,
                sent,
                pushed: Vec::new(),
                span: p.sql_span.clone(),
                as_written,
            });
        }
        let plan = CombinedPlan { parts, parsed, text: text.to_string() };
        // The outer query, checked as it will run (outputs' handles are
        // names DuckDB reads, so any stands in for them here).
        let outer = Checked::new(plan.outer(&vec!["r0".to_string(); plan.parsed.parts.len()]), ConnectorKind::Duckdb).bind(ConnectorKind::Duckdb, params);
        match outer {
            Err(names) => missing.extend(names.into_iter().filter(|n| !missing.contains(n)).collect::<Vec<_>>()),
            Ok(o) if o.kind() != StatementKind::Read => {
                return Err(EngineError::new(
                    "invalid",
                    format!(
                        "The query that reads the parts must only read (it runs in DuckDB): this one is {} ({})",
                        o.classification().keyword,
                        kind_words(o.kind())
                    ),
                ));
            }
            Ok(_) => {}
        }
        if !missing.is_empty() {
            let list = missing.iter().map(|n| format!(":{n}")).collect::<Vec<_>>().join(", ");
            return Err(EngineError { code: Some("missing_parameters".into()), ..EngineError::new("invalid", format!("Enter a value for {list}")) });
        }
        Ok(Some(plan))
    }

    /// The rows a part may have: the run's own (`part_rows`, 0 = no
    /// limit), else the setting, else [`PART_ROWS_DEFAULT`].
    fn part_rows(&self, req: &RunRequest) -> Option<usize> {
        let n = req
            .part_rows
            .unwrap_or_else(|| self.workspace.get_setting(PART_ROWS_SETTING).ok().flatten().and_then(|v| v.as_u64()).map_or(PART_ROWS_DEFAULT, |v| v as usize));
        (n > 0).then_some(n)
    }

    /// Run statement `stmt` of `req`, a combined query: its parts (each
    /// output of the tab, announced by [`JobEvent::PartFinished`]), then
    /// the outer query on the Results connection, whose outcome is the
    /// statement's. A part that fails stops the others.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn run_combined(
        self: &Arc<Self>,
        job_id: &str,
        req: &RunRequest,
        stmt: &PlannedStatement,
        limit: Option<usize>,
        cancel: CancellationToken,
        waiter: Option<&Waiter>,
    ) -> Result<StatementOutcome> {
        let text = &req.sql[stmt.start..stmt.end];
        let plan = self
            .plan_combined(text, &req.params, req.origin, &req.connection_id, req.session_key())?
            .ok_or_else(|| EngineError::new("internal", "the statement is no longer a combined query"))?;
        let results = self.results_connection()?;
        let cap = self.part_rows(req);
        let stop = cancel.child_token();
        let rows = Arc::new(AtomicUsize::new(0));
        let mut handles: Vec<Option<String>> = vec![None; plan.parts.len()];
        let mut failure: Option<EngineError> = None;
        let mut tasks = tokio::task::JoinSet::new();
        let mut next = 0;
        loop {
            while next < plan.parts.len() && tasks.len() < PARALLEL_PARTS && failure.is_none() && !stop.is_cancelled() {
                let (engine, part, i) = (self.clone(), plan.parts[next].clone(), next);
                let (req, stop) = (req.clone(), stop.clone());
                let run = PartRun { job_id: job_id.to_string(), index: stmt.index, rows: rows.clone() };
                tasks.spawn(async move { (i, engine.run_part(&req, &part, run, cap, stop).await) });
                next += 1;
            }
            let Some(done) = tasks.join_next().await else { break };
            match done {
                Ok((i, Ok(output))) => {
                    let p = &plan.parts[i];
                    handles[i] = Some(output.handle.clone());
                    let ev = JobEvent::PartFinished {
                        job_id: job_id.to_string(),
                        tab_id: req.tab_id.clone(),
                        index: stmt.index,
                        part: p.name.clone(),
                        connection_name: p.connection_name.clone(),
                        output: Box::new(output),
                    };
                    if let Some(w) = waiter {
                        let _ = w.send(ev.clone());
                    }
                    self.events.emit(ev);
                }
                Ok((i, Err(e))) => {
                    // The first real failure says why; a part stopped because
                    // of it only says "cancelled".
                    let p = &plan.parts[i];
                    let e = part_error(text, p, e);
                    if failure.as_ref().is_none_or(|f| f.kind == "cancelled" && e.kind != "cancelled") {
                        failure = Some(e);
                    }
                    stop.cancel();
                }
                Err(e) => {
                    failure.get_or_insert_with(|| EngineError::new("internal", format!("a part stopped unexpectedly: {e}")));
                    stop.cancel();
                }
            }
        }
        if let Some(e) = failure {
            return Err(e);
        }
        if cancel.is_cancelled() {
            return Err(ConnectorError::cancelled().into());
        }
        let handles: Vec<String> = handles.into_iter().map(|h| h.expect("every part finished")).collect();
        let outer = Checked::new(plan.outer(&handles), ConnectorKind::Duckdb)
            .bind(ConnectorKind::Duckdb, &req.params)
            .map_err(|names| EngineError::new("invalid", format!("Enter a value for :{}", names.join(", :"))))?;
        if outer.kind() != StatementKind::Read {
            return Err(EngineError::new("invalid", "The query that reads the parts must only read (it runs in DuckDB)"));
        }
        let mut on_results = req.clone();
        on_results.connection_id = results.id.clone();
        let ran = outer.sql().to_string();
        let outer_stmt = PlannedStatement { index: stmt.index, start: stmt.start, end: stmt.end, checked: outer, combined: false };
        match self.run_statement(job_id, &on_results, &outer_stmt, limit, cancel, waiter, None).await {
            // What ran, reading each part as `results.<handle>`: the output's
            // SQL, so whatever reads the join later (the assistant's policy
            // and masks, an export of every row) traces it to the parts.
            Ok(o) => Ok(StatementOutcome { ran_sql: Some(ran), ..o }),
            Err(e) => {
                if e.kind == "connection" || e.code.as_deref() == Some(databrain_connector_core::SESSION_ENDED) {
                    self.drop_session(&results.id, req.session_key());
                }
                Err(e)
            }
        }
    }

    /// One part: read whole on its connection's session for this tab
    /// ([`SessionRole::Part`]), kept as an output of the tab, and recorded
    /// in that connection's history. More rows than `cap` fail it, as does
    /// anything else that would keep fewer rows than the server has.
    async fn run_part(self: &Arc<Self>, req: &RunRequest, p: &PartPlan, run: PartRun, cap: Option<usize>, cancel: CancellationToken) -> Result<OutputInfo> {
        let key = SessionRole::Part { tab: req.session_key().to_string(), name: p.name.clone() }.key();
        let mut on = req.clone();
        on.connection_id = p.connection_id.clone();
        on.session_key = Some(key.clone());
        on.output_masks = None;
        // Results of its own (`{job}/{n}:0`): never one of the run's statements.
        let part_job = format!("{}/{}", run.job_id, p.name);
        let stmt = PlannedStatement { index: 0, start: 0, end: 0, checked: p.sent.clone(), combined: false };
        let started_at = now_ms();
        let t0 = Instant::now();
        let outcome = self.run_statement(&part_job, &on, &stmt, cap, cancel, None, Some(&run)).await;
        let outcome = outcome.and_then(|o| {
            let r = o.result.ok_or_else(|| EngineError::new("invalid", "it returned no rows (a part must be a query)"))?;
            if r.truncated {
                self.results.remove(&r.id);
                let at_cap = cap.is_some_and(|c| r.total_rows >= c);
                return Err(if at_cap {
                    EngineError {
                        code: Some(PART_TOO_LARGE.into()),
                        ..EngineError::new(
                            "invalid",
                            format!(
                                "it has more than {} rows, and a part is read whole: filter it in the part, or run again without the limit",
                                group_digits(cap.unwrap_or_default())
                            ),
                        )
                    }
                } else {
                    let why = o.notices.last().cloned().unwrap_or_default();
                    EngineError::new("invalid", format!("it couldn't be read whole, so the join would be wrong. {why}"))
                });
            }
            Ok(r)
        });
        if matches!(&outcome, Err(e) if e.kind == "connection" || e.code.as_deref() == Some(databrain_connector_core::SESSION_ENDED)) {
            self.drop_session(&p.connection_id, &key);
        }
        let output = outcome.as_ref().ok().map(|r| {
            self.outputs.add(NewOutput {
                result: r.clone(),
                connection_id: p.connection_id.clone(),
                connection_name: p.connection_name.clone(),
                kind: p.kind,
                sql: p.sent.sql().to_string(),
                tab_id: req.tab_id.clone(),
                statement_index: run.index,
                row_limit: None,
                origin: req.origin,
                // Loading every row later takes the tab's key, which the
                // tab's runs hold too (never the part's session, which a
                // run of the tab may be using).
                session_key: req.session_key().to_string(),
            })
        });
        let (status, rows, error) = match &outcome {
            Ok(r) => (RunStatus::Success, Some(r.total_rows as i64), None),
            Err(e) if e.kind == "cancelled" => (RunStatus::Cancelled, None, None),
            Err(e) => (RunStatus::Error, None, Some(e.message.clone())),
        };
        let _ = crate::blocking(|| {
            self.workspace.add_history(NewHistory {
                origin: req.origin,
                connection_id: Some(p.connection_id.clone()),
                sql: p.sent.sql().to_string(),
                started_at,
                duration_ms: t0.elapsed().as_millis() as i64,
                rows,
                status: Some(status),
                error,
                output_handle: output.as_ref().map(|o| o.handle.clone()),
                result_id: output.as_ref().map(|o| o.result_id.clone()),
            })
        });
        outcome.map(|_| output.expect("an output for a result"))
    }
}

/// A part's error as the statement's: it names the part and its
/// connection, and its position is in the statement.
fn part_error(text: &str, p: &PartPlan, e: EngineError) -> EngineError {
    if e.kind == "cancelled" {
        return e;
    }
    let before = text.get(..p.span.start).map_or(0, |t| t.chars().count()) as u32;
    EngineError {
        message: format!("Part `{}` (on \"{}\"): {}", p.name, p.connection_name, e.message),
        position: Some(e.position.filter(|_| p.as_written).map_or(before + 1, |pos| before + pos)),
        ..e
    }
}

fn kind_words(k: StatementKind) -> &'static str {
    match k {
        StatementKind::Read => "a read",
        StatementKind::Dml => "it changes data",
        StatementKind::Ddl => "it changes the schema",
        StatementKind::Session => "a session setting",
        StatementKind::Transaction => "transaction control",
        StatementKind::Other => "an admin statement",
        StatementKind::Unknown => "DataBrain can't tell that it only reads",
    }
}

/// `1000000` as `1,000,000`.
fn group_digits(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digits_are_grouped() {
        assert_eq!(group_digits(1_000_000), "1,000,000");
        assert_eq!(group_digits(999), "999");
        assert_eq!(group_digits(1234), "1,234");
    }

    #[test]
    fn positions_count_characters() {
        assert_eq!(position_at("ab", 0), 1);
        assert_eq!(position_at("äb", 2), 2);
    }
}
