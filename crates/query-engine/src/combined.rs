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
/// Setting: push the outer query's columns, filters and limit into the
/// parts (docs/design/cross-connection-queries.md, §7); default on.
pub const PUSHDOWN_SETTING: &str = "combined_pushdown";
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
    /// What pushdown added to the part ("columns id, total", "total > 5"),
    /// for the UI; empty when it runs as written.
    pub pushed: Vec<String>,
    /// The part's SQL in the statement (byte range): error positions.
    pub span: Range<usize>,
    /// The part with its parameters bound, before pushdown: where a
    /// server's position maps back to the part as written.
    pub bound: Checked,
    /// The column classes pushdown planned with (the part's as described,
    /// or as kept from an earlier run), checked against what its run gave.
    pub types: Option<Vec<(String, crate::pushdown::TypeClass)>>,
    /// A limit pushed with filters, checked after the run
    /// (`pushdown::Pushed::checked_limit`).
    pub checked_limit: Option<usize>,
    /// Pushdown wrapped it: where `bound`'s text starts in `sent` (bytes).
    /// `None` when it runs as bound (`sent` is `bound`), or wasn't found.
    pub wrapped_at: Option<Option<usize>>,
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
    /// The combined statement's index in the run.
    pub index: usize,
    /// The part's index in the statement: with `index`, its results' ids
    /// (`{job}/{index}/{part}`), unique in the run whatever the parts' names.
    pub part: usize,
    /// Rows read so far by all the statement's parts.
    pub rows: Arc<AtomicUsize>,
    /// Bytes all the statement's parts hold in memory: a part moves to disk
    /// when its rows and the others' pass the statement's cap.
    pub bytes: Arc<AtomicUsize>,
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
        // A stand-in for every name, so the answer never depends on which
        // connections exist: an error comes only once a part was found, so
        // it is a part too. Not one: nothing is looked up.
        if let Some(from) = elsewhere {
            return match parse_combined(text, &mut |_| Ok(ConnectorKind::Postgres)) {
                Ok(None) => Ok(None),
                _ => Err(EngineError::new(
                    "unsupported",
                    format!("A query with parts on other connections (@connection) runs from a query tab or `dbr` for now, not from {from}"),
                )),
            };
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
        // Two parts of one name would share a session (and DuckDB refuses the duplicate).
        for (k, p) in parsed.parts.iter().enumerate() {
            if parsed.parts[..k].iter().any(|q| q.name.to_lowercase() == p.name.to_lowercase()) {
                return Err(refused(text, p.body_span.start, format!("Two parts are named `{}`: give each its own name", p.name)));
            }
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
            Self::check_intent(profile, &Intent::Run { plan: &one, confirmed: true }).map_err(|e| EngineError {
                message: format!("{on}: {}", e.message),
                position: Some(position_at(text, at)),
                ..e
            })?;
            if profile.id == tab_connection && self.in_transaction(tab_connection, session_key) {
                return Err(refused(
                    text,
                    at,
                    format!("{on} runs on a session of its own, which wouldn't see this tab's open transaction: commit or roll it back first"),
                ));
            }
            parts.push(PartPlan {
                name: p.name.clone(),
                connection_id: profile.id.clone(),
                connection_name: profile.name.clone(),
                kind: p.kind,
                bound: sent.clone(),
                sent,
                pushed: Vec::new(),
                span: p.sql_span.clone(),
                types: None,
                checked_limit: None,
                wrapped_at: None,
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

    /// Whether this run pushes into its parts: the run's own choice, else
    /// the setting (on by default).
    fn pushdown_on(&self, req: &RunRequest) -> bool {
        req.pushdown.unwrap_or_else(|| self.workspace.get_setting(PUSHDOWN_SETTING).ok().flatten().and_then(|v| v.as_bool()).unwrap_or(true))
    }

    /// Pushdown (§7): each part's column types from a query that returns
    /// none of its rows, then what the outer query lets each server be
    /// asked ([`crate::pushdown::plan`]). What is sent is checked again
    /// as a read on its connection; a part whose types aren't known, or
    /// whose sent SQL isn't a read there, runs as written.
    #[allow(clippy::too_many_arguments)]
    async fn push_down(
        self: &Arc<Self>,
        job_id: &str,
        index: usize,
        req: &RunRequest,
        plan: &mut CombinedPlan,
        cancel: &CancellationToken,
        fresh: bool,
        limit_with_where: bool,
    ) -> Vec<bool> {
        let mut from_cache = vec![false; plan.parts.len()];
        let outer = match Checked::new(plan.outer(&vec!["r0".to_string(); plan.parts.len()]), ConnectorKind::Duckdb).bind(ConnectorKind::Duckdb, &req.params) {
            Ok(o) => o.sql().to_string(),
            Err(_) => return from_cache,
        };
        // Nothing the outer query could push: no part is asked for its types.
        if !crate::pushdown::may_push(&outer) {
            return from_cache;
        }
        // The parts' types: kept from an earlier run (unless `fresh`), else
        // asked for at once.
        let mut types: Vec<Option<Vec<(String, crate::pushdown::TypeClass)>>> = vec![None; plan.parts.len()];
        let mut asks = tokio::task::JoinSet::new();
        for (i, p) in plan.parts.iter().enumerate() {
            if !fresh && let Some(t) = self.kept_part_types(&p.connection_id, p.bound.sql()) {
                types[i] = Some(t);
                from_cache[i] = true;
                continue;
            }
            let (engine, job, req, p, cancel) = (self.clone(), job_id.to_string(), req.clone(), p.clone(), cancel.clone());
            asks.spawn(async move { (i, engine.part_types(&job, index, i, &req, &p, &cancel).await) });
        }
        while let Some(done) = asks.join_next().await {
            if let Ok((i, t)) = done {
                if let Some(t) = &t {
                    self.keep_part_types(&plan.parts[i].connection_id, plan.parts[i].bound.sql(), t.clone());
                }
                types[i] = t;
            }
        }
        let inputs: Vec<crate::pushdown::PartInput> = plan
            .parts
            .iter()
            .zip(&types)
            .map(|(p, t)| crate::pushdown::PartInput { name: &p.name, kind: p.kind, sql: p.sent.sql(), columns: t.as_deref() })
            .collect();
        let pushed = crate::pushdown::plan_with(&outer, &inputs, limit_with_where);
        for ((p, x), t) in plan.parts.iter_mut().zip(pushed).zip(types) {
            p.types = t;
            let Some(x) = x else { continue };
            let sent = Checked::new(x.sent, p.kind);
            let one = [PlannedStatement { index: 0, start: 0, end: 0, checked: sent.clone(), combined: false }];
            let allowed = self
                .workspace
                .get_connection(&p.connection_id)
                .is_ok_and(|prof| Self::check_intent(&prof, &Intent::Run { plan: &one, confirmed: true }).is_ok());
            if sent.kind() == StatementKind::Read && allowed {
                // Positions now count in the wrapper, around the part's text.
                // `wrap` puts the part in trimmed (and without a last `;`).
                let inner = p.bound.sql().trim().trim_end_matches(';').trim_end();
                let lead = p.bound.sql().len() - p.bound.sql().trim_start().len();
                p.wrapped_at = Some(sent.sql().find(inner).and_then(|at| at.checked_sub(lead)));
                p.sent = sent;
                p.pushed = x.what;
                p.checked_limit = x.checked_limit;
            }
        }
        from_cache
    }

    /// Whether any part whose types came from the cache (`from_cache`) has
    /// other types now (described again): those are kept instead. A run
    /// that used kept types and failed, or whose parts arrived with other
    /// classes, runs again only then; otherwise its answer (or error) is
    /// the one an uncached run gives.
    async fn types_changed(
        self: &Arc<Self>,
        job_id: &str,
        index: usize,
        req: &RunRequest,
        plan: &CombinedPlan,
        from_cache: &[bool],
        cancel: &CancellationToken,
    ) -> bool {
        let mut changed = false;
        for (i, p) in plan.parts.iter().enumerate().filter(|(i, _)| from_cache[*i]) {
            let now = self.part_types(job_id, index, i, req, p, cancel).await;
            if now.as_ref() != p.types.as_ref() {
                match now {
                    Some(t) => self.keep_part_types(&p.connection_id, p.bound.sql(), t),
                    None => self.forget_descriptions(&p.connection_id),
                }
                changed = true;
            }
        }
        changed
    }

    /// The classes a part's output arrived with, as `part_types` reads them.
    fn output_classes(&self, o: &OutputInfo, kind: ConnectorKind) -> Option<Vec<(String, crate::pushdown::TypeClass)>> {
        let schema = self.results.get(&o.result_id).ok().map(|rs| rs.lock().schema())?;
        let db: HashMap<&str, Option<&str>> = o.columns.iter().map(|c| (c.name.as_str(), c.db_type.as_deref())).collect();
        Some(
            schema
                .fields()
                .iter()
                .map(|f| (f.name().clone(), crate::pushdown::TypeClass::of_field(f, db.get(f.name().as_str()).copied().flatten(), kind)))
                .collect(),
        )
    }

    /// Drop what an attempt kept (its parts' outputs and the join's), when
    /// the statement runs again.
    fn drop_attempt(&self, outputs: &[OutputInfo], join_result: Option<&str>) {
        for o in outputs {
            let _ = self.outputs.remove(&o.handle);
            self.results.remove(&o.result_id);
        }
        if let Some(rid) = join_result {
            if let Some(o) = self.outputs.list().into_iter().find(|o| o.result_id == rid) {
                let _ = self.outputs.remove(&o.handle);
            }
            self.results.remove(rid);
        }
    }

    /// A part's output columns and their classes, from its query wrapped to
    /// return no row; `None` when it can't be asked so.
    #[allow(clippy::too_many_arguments)]
    async fn part_types(
        self: &Arc<Self>,
        job_id: &str,
        index: usize,
        part: usize,
        req: &RunRequest,
        p: &PartPlan,
        cancel: &CancellationToken,
    ) -> Option<Vec<(String, crate::pushdown::TypeClass)>> {
        // The part before pushdown (what its types are of).
        let sql = crate::pushdown::describe_sql(p.bound.sql(), p.kind)?;
        let checked = Checked::new(sql, p.kind);
        if checked.kind() != StatementKind::Read {
            return None;
        }
        let mut on = req.clone();
        on.connection_id = p.connection_id.clone();
        on.session_key = Some(SessionRole::Part { tab: req.session_key().to_string(), name: p.name.clone() }.key());
        on.output_masks = None;
        let run = PartRun { job_id: job_id.to_string(), index, part, rows: Arc::new(AtomicUsize::new(0)), bytes: Arc::new(AtomicUsize::new(0)) };
        let stmt = PlannedStatement { index: 0, start: 0, end: 0, checked, combined: false };
        let job = format!("{job_id}/{index}/{part}/types");
        let out = self.run_statement(&job, &on, &stmt, Some(1), cancel.child_token(), None, Some(&run)).await.ok()?;
        let r = out.result?;
        let schema = self.results.get(&r.id).ok().map(|rs| rs.lock().schema());
        self.results.remove(&r.id);
        let schema = schema?;
        // The server's type name (as the grid shows it) tells text from what arrives as text.
        let db: HashMap<&str, Option<&str>> = r.columns.iter().map(|c| (c.name.as_str(), c.db_type.as_deref())).collect();
        Some(
            schema
                .fields()
                .iter()
                .map(|f| (f.name().clone(), crate::pushdown::TypeClass::of_field(f, db.get(f.name().as_str()).copied().flatten(), p.kind)))
                .collect(),
        )
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
        let results = self.results_connection()?;
        let cap = self.part_rows(req);
        // Asked again when kept types turned out old (`fresh`), or a limit
        // pushed with filters was filled by rows DuckDB dropped.
        let (mut fresh, mut limit_with_where) = (false, true);
        let mut notices: Vec<String> = Vec::new();
        for attempt in 0..3 {
            let mut plan = self
                .plan_combined(text, &req.params, req.origin, &req.connection_id, req.session_key())?
                .ok_or_else(|| EngineError::new("internal", "the statement is no longer a combined query"))?;
            let mut from_cache = vec![false; plan.parts.len()];
            if self.pushdown_on(req) {
                from_cache = self.push_down(job_id, stmt.index, req, &mut plan, &cancel, fresh, limit_with_where).await;
            }
            let cached = from_cache.iter().any(|c| *c);
            // A run that may be done again shows its parts only once it is final.
            let hold = attempt < 2 && (cached || plan.parts.iter().any(|p| p.checked_limit.is_some()));
            let (parts, held) = self.run_parts(job_id, req, stmt, &plan, cap, &cancel, waiter, text, hold).await;
            let emit = |held: Vec<JobEvent>| {
                for ev in held {
                    if let Some(w) = waiter {
                        let _ = w.send(ev.clone());
                    }
                    self.events.emit(ev);
                }
            };
            let outs = match parts {
                Ok(outs) => outs,
                Err(e) => {
                    if hold && cached && e.kind != "cancelled" && self.types_changed(job_id, stmt.index, req, &plan, &from_cache, &cancel).await {
                        self.drop_attempt(&outputs_of(&held), None);
                        fresh = true;
                        continue;
                    }
                    emit(held);
                    return Err(e);
                }
            };
            // Kept types that no longer match what the parts gave.
            let stale = plan.parts.iter().zip(&outs).zip(&from_cache).any(|((p, o), c)| {
                *c && self.output_classes(o, p.kind).is_some_and(|got| {
                    let kept = p.types.as_deref().unwrap_or(&[]);
                    got.iter().any(|(n, t)| kept.iter().find(|(k, _)| k == n).is_some_and(|(_, kt)| kt != t))
                })
            });
            if hold && stale && self.types_changed(job_id, stmt.index, req, &plan, &from_cache, &cancel).await {
                self.drop_attempt(&outs, None);
                fresh = true;
                notices.push("A part's column types had changed since they were last read: its rows were read again".into());
                continue;
            }
            let handles: Vec<String> = outs.iter().map(|o| o.handle.clone()).collect();
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
            let o = match self.run_statement(job_id, &on_results, &outer_stmt, limit, cancel.clone(), waiter, None).await {
                Ok(o) => o,
                Err(e) => {
                    if e.kind == "connection" || e.code.as_deref() == Some(databrain_connector_core::SESSION_ENDED) {
                        self.drop_session(&results.id, req.session_key());
                    }
                    if hold && cached && e.kind != "cancelled" && self.types_changed(job_id, stmt.index, req, &plan, &from_cache, &cancel).await {
                        self.drop_attempt(&outs, None);
                        fresh = true;
                        continue;
                    }
                    emit(held);
                    return Err(e);
                }
            };
            // A limit pushed with filters, filled by rows DuckDB then dropped:
            // the server had more; that part is read again without it.
            let joined = o.result.as_ref().map(|r| (r.total_rows, r.truncated));
            let short = plan
                .parts
                .iter()
                .zip(&outs)
                .any(|(p, out)| p.checked_limit.is_some_and(|n| out.rows >= n && joined.is_some_and(|(rows, cut)| rows < n && !cut)));
            if hold && short {
                self.drop_attempt(&outs, o.result.as_ref().map(|r| r.id.as_str()));
                limit_with_where = false;
                notices.push("A part's pushed filter kept rows the query then dropped, so its limit wasn't enough: it was read again without the limit".into());
                continue;
            }
            emit(held);
            let mut o = o;
            o.notices.extend(notices);
            // What ran, reading each part as `results.<handle>`: the output's
            // SQL, so whatever reads the join later (the assistant's policy
            // and masks, an export of every row) traces it to the parts.
            return Ok(StatementOutcome { ran_sql: Some(ran), ..o });
        }
        Err(EngineError::new("internal", "the combined query ran again too often"))
    }

    /// Run the parts of `plan` (four at once), each an output of the tab.
    /// With `hold`, their `PartFinished` events are returned instead of sent
    /// (the statement may run again); else sent as each finishes. A part
    /// that fails stops the others.
    #[allow(clippy::too_many_arguments)]
    async fn run_parts(
        self: &Arc<Self>,
        job_id: &str,
        req: &RunRequest,
        stmt: &PlannedStatement,
        plan: &CombinedPlan,
        cap: Option<usize>,
        cancel: &CancellationToken,
        waiter: Option<&Waiter>,
        text: &str,
        hold: bool,
    ) -> (Result<Vec<OutputInfo>>, Vec<JobEvent>) {
        let stop = cancel.child_token();
        let rows = Arc::new(AtomicUsize::new(0));
        let bytes = Arc::new(AtomicUsize::new(0));
        let mut outs: Vec<Option<OutputInfo>> = vec![None; plan.parts.len()];
        let mut held: Vec<JobEvent> = Vec::new();
        let mut failure: Option<EngineError> = None;
        let mut tasks = tokio::task::JoinSet::new();
        let mut next = 0;
        loop {
            while next < plan.parts.len() && tasks.len() < PARALLEL_PARTS && failure.is_none() && !stop.is_cancelled() {
                let (engine, part, i) = (self.clone(), plan.parts[next].clone(), next);
                let (req, stop) = (req.clone(), stop.clone());
                let run = PartRun { job_id: job_id.to_string(), index: stmt.index, part: i, rows: rows.clone(), bytes: bytes.clone() };
                tasks.spawn(async move { (i, engine.run_part(&req, &part, run, cap, stop).await) });
                next += 1;
            }
            let Some(done) = tasks.join_next().await else { break };
            match done {
                Ok((i, Ok(output))) => {
                    let p = &plan.parts[i];
                    outs[i] = Some(output.clone());
                    let ev = JobEvent::PartFinished {
                        job_id: job_id.to_string(),
                        tab_id: req.tab_id.clone(),
                        index: stmt.index,
                        part: p.name.clone(),
                        connection_name: p.connection_name.clone(),
                        pushed: p.pushed.clone(),
                        output: Box::new(output),
                    };
                    if hold {
                        held.push(ev);
                    } else {
                        if let Some(w) = waiter {
                            let _ = w.send(ev.clone());
                        }
                        self.events.emit(ev);
                    }
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
            return (Err(e), held);
        }
        if cancel.is_cancelled() {
            return (Err(ConnectorError::cancelled().into()), held);
        }
        (Ok(outs.into_iter().map(|o| o.expect("every part finished")).collect()), held)
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
        // Results of its own (`{job}/{statement}/{part}:0`): never one of the
        // run's statements, nor another statement's part of the same name.
        let part_job = format!("{}/{}/{}", run.job_id, run.index, run.part);
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
                part: Some(p.name.clone()),
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

/// The outputs a held attempt's part events carry.
fn outputs_of(held: &[JobEvent]) -> Vec<OutputInfo> {
    held.iter()
        .filter_map(|e| match e {
            JobEvent::PartFinished { output, .. } => Some((**output).clone()),
            _ => None,
        })
        .collect()
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
        position: Some(e.position.and_then(|pos| written_in_part(p, pos)).map_or(before + 1, |pos| before + pos)),
        ..e
    }
}

/// A server's 1-based character position in what a part sent, as one in
/// the part as written: out of pushdown's wrapper, then through its
/// parameters. `None` when it is in the wrapper (the part's start is shown).
fn written_in_part(p: &PartPlan, pos: u32) -> Option<u32> {
    let in_bound = match p.wrapped_at {
        None => pos,
        Some(at) => {
            let at = at?;
            let sent = p.sent.sql();
            let byte = sent.char_indices().nth(pos.saturating_sub(1) as usize).map_or(sent.len(), |(b, _)| b);
            let len = p.bound.sql().len();
            if byte < at || byte >= at + len {
                return None;
            }
            p.bound.sql()[..byte - at].chars().count() as u32 + 1
        }
    };
    Some(p.bound.written_position(in_bound))
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

    /// A server's position in what a part sent maps to the part as
    /// written: through pushdown's wrapper and the part's parameters; in
    /// the wrapper itself there is none (the part's start is shown).
    #[test]
    fn part_positions_through_the_wrapper_and_parameters() {
        let params = HashMap::from([("v".to_string(), databrain_connector_core::sql::ParamValue { value: "a long value".into(), raw: false })]);
        let written = "select :v, nope from t";
        let bound = Checked::new(written, ConnectorKind::Postgres).bind(ConnectorKind::Postgres, &params).unwrap();
        let wrapper = format!("SELECT \"x\" FROM (\n{}\n) databrain_part WHERE \"x\" > 1", bound.sql());
        let mut p = PartPlan {
            name: "a".into(),
            connection_id: "c".into(),
            connection_name: "c".into(),
            kind: ConnectorKind::Postgres,
            sent: bound.clone(),
            bound: bound.clone(),
            pushed: vec![],
            span: 0..written.len(),
            types: None,
            checked_limit: None,
            wrapped_at: None,
        };
        let chars = |s: &str, w: &str| s[..s.find(w).unwrap()].chars().count() as u32 + 1;
        // As bound.
        assert_eq!(written_in_part(&p, chars(bound.sql(), "nope")), Some(chars(written, "nope")));
        // Wrapped.
        p.wrapped_at = Some(wrapper.find(bound.sql()));
        p.sent = Checked::new(wrapper.clone(), ConnectorKind::Postgres);
        assert_eq!(written_in_part(&p, chars(&wrapper, "nope")), Some(chars(written, "nope")));
        assert_eq!(written_in_part(&p, chars(&wrapper, "databrain_part")), None, "in the wrapper");
        assert_eq!(written_in_part(&p, chars(&wrapper, "long value")), Some(chars(written, ":v")), "in a value");
        // Wrapped, but the part's text wasn't found in it.
        p.wrapped_at = Some(None);
        assert_eq!(written_in_part(&p, 3), None);
    }

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
