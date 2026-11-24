//! Running SQL: planning statements, safety prompts, the run loop and
//! waiting for a run (AI, MCP, job steps).

use super::*;

/// A statement that asks before it runs on a production connection: every
/// statement except reads, transaction control (`BEGIN`, `COMMIT`) and the
/// harmless `USE`/`SET` a read-only connection allows. So admin statements
/// classified `Other` (`KILL`, `SHUTDOWN`, `RESTORE`, `VACUUM FULL`,
/// `REFRESH MATERIALIZED VIEW`, `FLUSH TABLES WITH READ LOCK`) and session
/// statements with effects beyond the session (`SET GLOBAL …`) ask too, as
/// they do for the AI.
pub(crate) fn prod_needs_confirmation(p: &PlannedStatement) -> bool {
    match p.classification().kind {
        StatementKind::Read | StatementKind::Transaction => false,
        StatementKind::Session => !databrain_connector_core::sql::read_only_allows(p.sql(), p.classification()),
        StatementKind::Dml | StatementKind::Ddl | StatementKind::Other | StatementKind::Unknown => true,
    }
}

impl QueryEngine {
    // -------------------------------------------------------------- jobs

    pub fn plan(&self, profile: &ConnectionProfile, sql: &str) -> Vec<PlannedStatement> {
        let kind = profile.config.kind;
        split_statements(sql, kind)
            .into_iter()
            .enumerate()
            .map(|(index, s)| PlannedStatement { index, checked: Checked::new(s.sql, kind), start: s.start, end: s.end })
            .collect()
    }

    /// Put the values of `:name` parameters into the statements (spans stay
    /// those of the editor text; classification sees the real SQL).
    pub(crate) fn bind_parameters(
        plan: &mut [PlannedStatement],
        kind: databrain_connector_core::ConnectorKind,
        params: &HashMap<String, databrain_connector_core::sql::ParamValue>,
    ) -> Result<()> {
        let mut missing: Vec<String> = Vec::new();
        for p in plan.iter_mut() {
            match p.checked.clone().bind(kind, params) {
                Ok(bound) => p.checked = bound,
                Err(names) => missing.extend(names.into_iter().filter(|n| !missing.contains(n)).collect::<Vec<_>>()),
            }
        }
        if missing.is_empty() {
            return Ok(());
        }
        let list = missing.iter().map(|n| format!(":{n}")).collect::<Vec<_>>().join(", ");
        Err(EngineError { code: Some("missing_parameters".into()), ..EngineError::new("invalid", format!("Enter a value for {list}")) })
    }

    /// Reasons the user must confirm before running, if any.
    pub(crate) fn safety_reasons(profile: &ConnectionProfile, plan: &[PlannedStatement]) -> Vec<String> {
        let mut reasons = Vec::new();
        for p in plan {
            let c = p.classification();
            if c.missing_where {
                reasons.push(format!("Statement {} is {} without a WHERE clause and will affect every row.", p.index + 1, c.keyword));
            }
        }
        if profile.env == EnvTag::Prod {
            let writes = plan.iter().filter(|p| prod_needs_confirmation(p)).count();
            if writes > 0 {
                reasons.push(format!("{writes} statement(s) may modify data, schema or server state on the production connection \"{}\".", profile.name));
            }
        }
        reasons
    }

    pub fn run(self: &Arc<Self>, req: RunRequest) -> Result<RunResponse> {
        self.start_run(req, None)
    }

    /// Run the statement that made output `handle` again, as that result
    /// only (the grid shows saved edits): the tab's other results stay
    /// active and shown, and the new result takes the old one's statement
    /// index, so events and the output say where it belongs. `req` holds
    /// that one statement, on the output's tab.
    pub fn rerun_output(self: &Arc<Self>, req: RunRequest, handle: &str) -> Result<RunResponse> {
        self.start(req, None, Some(handle))
    }

    /// [`Self::run`]; `waiter` also gets this run's outcome events
    /// (statement finished/failed, job finished), so a caller waiting for it
    /// can't miss them the way a lagging broadcast subscriber can.
    pub(crate) fn start_run(self: &Arc<Self>, req: RunRequest, waiter: Option<Waiter>) -> Result<RunResponse> {
        self.start(req, waiter, None)
    }

    fn start(self: &Arc<Self>, req: RunRequest, waiter: Option<Waiter>, replaces: Option<&str>) -> Result<RunResponse> {
        let profile = self.workspace.get_connection(&req.connection_id)?;
        let mut plan = self.plan(&profile, &req.sql);
        if plan.is_empty() {
            return Err(EngineError::new("invalid", "Nothing to run"));
        }
        Self::bind_parameters(&mut plan, profile.config.kind, &req.params)?;
        // The table explorer's sample: one read, whatever its `where` box
        // held (a `;` in it would start another statement). Its Delta
        // history and detail are reads there (see `Checked::explorer_read`).
        if req.origin == Origin::Explorer {
            for p in &mut plan {
                p.checked = p.checked.clone().explorer_read(profile.config.kind);
            }
        }
        if req.origin == Origin::Explorer && !(plan.len() == 1 && plan[0].classification().kind.is_read()) {
            return Err(EngineError::new("invalid", "The table explorer runs one read only: the condition can't end the statement (`;`) or change data"));
        }
        if let Some(h) = replaces {
            let o = self.outputs.get(h).ok_or_else(|| EngineError::new("not_found", format!("output {h} not found")))?;
            if o.tab_id != req.tab_id {
                return Err(EngineError::new("invalid", format!("{h} is a result of another tab")));
            }
            if plan.len() != 1 {
                return Err(EngineError::new("invalid", "A result is refreshed by one statement"));
            }
            plan[0].index = o.statement_index;
        }
        if let Some(n) = req.output_name.as_deref().filter(|n| !n.is_empty()) {
            outputs::validate_name(n)?;
            self.outputs.check_owner(n, &req.tab_id)?;
        }
        let job_id = uuid::Uuid::new_v4().to_string();
        let cancel = CancellationToken::new();
        // Read-only and production checks, then the tab and its session
        // claimed under one lock: two runs started at once on the same tab
        // or session can't both pass.
        let hold = Hold {
            session_key: req.session_key(),
            cancel: cancel.clone(),
            busy: "A query is already running in this tab",
            run: Some((&req.tab_id, job_id.clone())),
        };
        match self.guarded(&req.connection_id, Some(hold), Intent::Run { plan: &plan, confirmed: req.confirmed })? {
            Guarded::Ask { reasons } => return Ok(RunResponse::NeedsConfirmation { reasons, statements: plan }),
            // The job's task ends the entry (`JobGuard`, `run_job`).
            Guarded::Go { claim, .. } => claim.expect("held").keep(),
        }
        // Previous outputs of this tab become "recent" (evictable); a
        // refreshed result only replaces its own.
        match replaces {
            Some(h) => self.outputs.retire(&req.tab_id, h),
            None => self.outputs.begin_run(&req.tab_id),
        }

        let engine = self.clone();
        let jid = job_id.clone();
        let statements = plan.clone();
        tokio::spawn(async move {
            // Ends the job however the task ends: a panic in it must not
            // leave the tab "already running" for good.
            let guard = JobGuard { engine: engine.clone(), job_id: jid.clone(), tab_id: req.tab_id.clone(), waiter: waiter.clone(), done: false };
            engine.run_job(jid, req, statements, cancel, waiter).await;
            guard.finish();
        });
        Ok(RunResponse::Started { job_id, statements: plan })
    }

    /// Hand the rows of the statements run for tab `tab_id` to the returned
    /// receiver as they are read, until the [`RowTap`] is dropped: what the
    /// result keeps after the row limit, batch by batch. The run waits for
    /// the receiver when it falls behind (at most a few batches queue).
    pub fn tap_rows(self: &Arc<Self>, tab_id: &str) -> (RowTap, tokio::sync::mpsc::Receiver<TappedBatch>) {
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        self.taps.lock().insert(tab_id.to_string(), tx);
        (RowTap { engine: self.clone(), tab_id: tab_id.to_string() }, rx)
    }

    /// Run and wait for completion (for the AI agent / MCP and job steps);
    /// `confirmed` skips the interactive safety prompt because the caller
    /// has already enforced its own policy.
    ///
    /// The outcome comes on a channel of its own, not from the [`EventHub`]
    /// broadcast: a subscriber that lags behind there loses events, and a
    /// lost `JobFinished` left the caller (a job step, with every step
    /// downstream of it) waiting forever after the query had ended.
    pub async fn run_and_wait(self: &Arc<Self>, hub: &EventHub, req: RunRequest, cancel: Option<CancellationToken>) -> Result<Vec<StatementOutcomeView>> {
        self.run_and_watch(hub, req, cancel, &|_, _| {}).await
    }

    /// [`Self::run_and_wait`], telling `rows` how many rows the running
    /// statement has read so far and in how many ms (when the tab's
    /// progress goes out), so a job step reading a large table isn't just
    /// "running".
    pub async fn run_and_watch(
        self: &Arc<Self>,
        _hub: &EventHub,
        req: RunRequest,
        cancel: Option<CancellationToken>,
        rows: &(dyn Fn(u64, u64) + Sync),
    ) -> Result<Vec<StatementOutcomeView>> {
        let (waiter, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let statements = match self.start_run(req, Some(waiter))? {
            RunResponse::Started { job_id, statements } => (job_id, statements),
            // Code `needs_confirmation`: a caller that can ask (the terminal's
            // `dbr`) asks and runs it again confirmed; the others report it.
            RunResponse::NeedsConfirmation { reasons, .. } => {
                return Err(EngineError { code: Some("needs_confirmation".into()), ..EngineError::new("invalid", reasons.join(" ")) });
            }
        };
        let (job_id, plan) = statements;
        let mut out: Vec<StatementOutcomeView> = plan
            .iter()
            .map(|p| StatementOutcomeView {
                index: p.index,
                sql: p.sql().to_string(),
                result: None,
                rows_affected: None,
                error: None,
                duration_ms: 0,
                notices: vec![],
            })
            .collect();
        let cancel = cancel.unwrap_or_default();
        // Forwarded once: `cancelled()` stays ready, so selecting on it again
        // would spin and starve the task that interrupts the statement.
        let mut forwarded = false;
        loop {
            let ev = tokio::select! {
                ev = rx.recv() => ev,
                _ = cancel.cancelled(), if !forwarded => {
                    forwarded = true;
                    self.cancel(&job_id);
                    continue;
                }
            };
            let Some(ev) = ev else {
                // The run's task ended without saying so (it panicked).
                return Err(EngineError::new("internal", "the query stopped unexpectedly"));
            };
            if ev.job_id() != job_id {
                continue;
            }
            match ev {
                JobEvent::StatementFinished { index, result, rows_affected, duration_ms, notices, .. } => {
                    if let Some(o) = out.get_mut(index) {
                        o.result = result;
                        o.rows_affected = rows_affected;
                        o.duration_ms = duration_ms;
                        o.notices = notices;
                    }
                }
                JobEvent::StatementFailed { index, error, duration_ms, .. } => {
                    if let Some(o) = out.get_mut(index) {
                        o.error = Some(error);
                        o.duration_ms = duration_ms;
                    }
                }
                JobEvent::Progress { rows: n, elapsed_ms, .. } => rows(n as u64, elapsed_ms),
                JobEvent::JobFinished { status, .. } => return finished(status, out),
                _ => {}
            }
        }
    }

    pub fn cancel(&self, job_id: &str) -> bool {
        match self.jobs.lock().get(job_id) {
            Some(j) => {
                j.cancel.cancel();
                true
            }
            None => false,
        }
    }

    pub fn is_running(&self, job_id: &str) -> bool {
        self.jobs.lock().contains_key(job_id)
    }

    pub(crate) async fn run_job(
        self: Arc<Self>,
        job_id: String,
        req: RunRequest,
        plan: Vec<PlannedStatement>,
        cancel: CancellationToken,
        waiter: Option<Waiter>,
    ) {
        // Outcome events go to the waiter too (see `start_run`).
        let emit = |ev: JobEvent| {
            if let Some(w) = &waiter {
                let _ = w.send(ev.clone());
            }
            self.events.emit(ev);
        };
        let job_start = Instant::now();
        let mut status = RunStatus::Success;
        let limit = req.row_limit.filter(|l| *l > 0);
        let (conn_name, conn_kind) = self
            .workspace
            .get_connection(&req.connection_id)
            .map(|p| (p.name, p.config.kind))
            .unwrap_or_else(|_| (req.connection_id.clone(), databrain_connector_core::ConnectorKind::Sqlite));
        let mut last_output: Option<String> = None;

        for stmt in &plan {
            if cancel.is_cancelled() {
                status = RunStatus::Cancelled;
                break;
            }
            self.events.emit(JobEvent::StatementStarted { job_id: job_id.clone(), tab_id: req.tab_id.clone(), index: stmt.index, sql: stmt.sql().to_string() });
            if changes_tables(stmt.classification().kind) {
                self.forget_descriptions(&req.connection_id);
            }
            let started_at = now_ms();
            let t0 = Instant::now();
            let outcome = match self.begin_if_manual(&req, conn_kind, stmt, &cancel).await {
                Ok(()) => self.run_statement(&job_id, &req, stmt, limit, cancel.child_token(), waiter.as_ref()).await,
                Err(e) => Err(e),
            };
            let duration_ms = t0.elapsed().as_millis() as u64;
            // What may have changed a table (DDL; a call or block we can't
            // tell; a COMMIT of DDL run in a transaction): its kept
            // descriptions go, even after an error (part of a script may
            // have run). Also before it (below), so one made while it ran
            // isn't kept.
            if changes_tables(stmt.classification().kind) {
                self.forget_descriptions(&req.connection_id);
            }

            let output = match &outcome {
                Ok(StatementOutcome { result: Some(r), .. }) => Some(Box::new(self.outputs.add(NewOutput {
                    result: r.clone(),
                    connection_id: req.connection_id.clone(),
                    connection_name: conn_name.clone(),
                    kind: conn_kind,
                    sql: stmt.sql().to_string(),
                    tab_id: req.tab_id.clone(),
                    statement_index: stmt.index,
                    row_limit: limit,
                    origin: req.origin,
                    session_key: req.session_key().to_string(),
                }))),
                _ => None,
            };
            if let Some(o) = &output {
                last_output = Some(o.handle.clone());
            }
            let (hist_status, rows, error) = match &outcome {
                Ok(o) => (RunStatus::Success, o.result.as_ref().map(|r| r.total_rows as i64).or(o.rows_affected.map(|n| n as i64)), None),
                Err(e) if e.kind == "cancelled" => (RunStatus::Cancelled, None, None),
                Err(e) => (RunStatus::Error, None, Some(e.message.clone())),
            };
            let _ = crate::blocking(|| {
                self.workspace.add_history(NewHistory {
                    origin: req.origin,
                    connection_id: Some(req.connection_id.clone()),
                    sql: stmt.sql().to_string(),
                    started_at,
                    duration_ms: duration_ms as i64,
                    rows,
                    status: Some(hist_status),
                    error,
                    output_handle: output.as_ref().map(|o| o.handle.clone()),
                    result_id: output.as_ref().map(|o| o.result_id.clone()),
                })
            });

            match outcome {
                Ok(o) => {
                    emit(JobEvent::StatementFinished {
                        job_id: job_id.clone(),
                        tab_id: req.tab_id.clone(),
                        index: stmt.index,
                        result: o.result,
                        output,
                        rows_affected: o.rows_affected,
                        duration_ms,
                        notices: o.notices,
                        server_ms: o.server_ms,
                    });
                }
                Err(e) => {
                    status = if e.kind == "cancelled" { RunStatus::Cancelled } else { RunStatus::Error };
                    // Lost, or ended to stop a statement its driver couldn't:
                    // the next statement opens a new session.
                    if matches!(e.kind.as_str(), "connection") || e.code.as_deref() == Some(databrain_connector_core::SESSION_ENDED) {
                        self.drop_session(&req.connection_id, req.session_key());
                    }
                    emit(JobEvent::StatementFailed {
                        job_id: job_id.clone(),
                        tab_id: req.tab_id.clone(),
                        index: stmt.index,
                        error: e,
                        duration_ms,
                        statement_start: req.base_offset + stmt.start,
                    });
                    break;
                }
            }
        }

        if let (Some(name), Some(h)) = (req.output_name.as_deref().filter(|n| !n.is_empty()), &last_output) {
            let _ = self.outputs.set_name(h, Some(name));
        }
        // Removed under the jobs lock `close_tab` marks it under: a tab
        // closed before this point is seen here, one closed after it has
        // every output of this run to free.
        let tab_closed = self.jobs.lock().remove(&job_id).is_some_and(|j| j.tab_closed);
        if tab_closed {
            // Outputs added after the tab closed would stay active (never
            // evicted) for a tab that no longer exists.
            self.outputs.close_tab(&req.tab_id);
        }
        self.outputs_changed();
        let in_transaction = self.in_transaction(&req.connection_id, req.session_key());
        // What ran and how it ended; never the SQL itself.
        tracing::debug!(connection = %req.connection_id, tab = %req.tab_id, origin = req.origin.as_str(), statements = plan.len(), ?status, ms = job_start.elapsed().as_millis() as u64, "run finished");
        emit(JobEvent::JobFinished { job_id, tab_id: req.tab_id, status, duration_ms: job_start.elapsed().as_millis() as u64, in_transaction });
    }

    /// Move result `rs` to disk (its own folder), or why it wasn't.
    fn spill(
        &self,
        rs: &Arc<parking_lot::Mutex<databrain_result_store::ResultSet>>,
        id: &str,
        sample: &databrain_connector_core::arrow::array::RecordBatch,
        jobs: bool,
    ) -> Spilled {
        let Some(dir) = self.results.spill_dir(id) else { return Spilled::NotHere };
        if let Some(free) = self.disk_still_low(jobs) {
            return Spilled::LowDisk(free);
        }
        // Writes up to half the budget: not on an async worker's time, and
        // not under the result's lock (the outputs list, the grid and
        // `result_info` read it meanwhile). Only this run adds rows, so
        // none come in between.
        blocking_io(|| {
            let start = match rs.lock().spill_start() {
                Ok(Some(s)) => s,
                Ok(None) => return Spilled::Yes,
                Err(e) => return Spilled::Failed(e.to_string()),
            };
            match start.write(dir, sample).and_then(|ready| rs.lock().spill_adopt(ready)) {
                Ok(()) => Spilled::Yes,
                Err(e) => Spilled::Failed(e.to_string()),
            }
        })
    }

    /// The free disk space when the disk where results spill is low even
    /// after freeing older results kept there that no tab shows
    /// ([`OutputRegistry::free_disk`]); None when there is room.
    /// `jobs`: the result needing room is a job step's (results of its own
    /// kind are freed first).
    fn disk_still_low(&self, jobs: bool) -> Option<u64> {
        self.outputs.low_disk()?;
        let (low, freed) = blocking_io(|| self.outputs.free_disk(jobs));
        if freed > 0 {
            self.outputs_changed();
        }
        low
    }

    pub(crate) async fn run_statement(
        &self,
        job_id: &str,
        req: &RunRequest,
        stmt: &PlannedStatement,
        limit: Option<usize>,
        cancel: CancellationToken,
        waiter: Option<&Waiter>,
    ) -> Result<StatementOutcome> {
        let started = Instant::now();
        let session = tokio::select! {
            s = self.session_for(&req.connection_id, req.session_key(), req.origin) => s?,
            _ = cancel.cancelled() => return Err(ConnectorError::cancelled().into()),
        };
        let batch_size = limit.map(|l| l.clamp(1, 2000)).unwrap_or(2000);
        let stream = session
            .execute(
                stmt.checked.statement(),
                ExecOptions {
                    batch_size,
                    cancel: cancel.clone(),
                    // One row past the cap tells us the result was truncated.
                    max_rows: limit.map(|l| l.saturating_add(1)),
                    output_masks: req.output_masks.clone(),
                },
            )
            .await?;
        self.collect_stream(job_id, req, stmt, limit, cancel, stream, started, waiter).await
    }

    /// Reads a statement's result stream into the result store. Every way
    /// out except a finished result (`Done`, or stopped at the row cap)
    /// removes what was stored: an error, a stream that ends without `Done`,
    /// and this future being dropped.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn collect_stream(
        &self,
        job_id: &str,
        req: &RunRequest,
        stmt: &PlannedStatement,
        limit: Option<usize>,
        cancel: CancellationToken,
        mut stream: databrain_connector_core::QueryStream,
        started: Instant,
        // Gets the progress too (a job step waiting on the run).
        waiter: Option<&Waiter>,
    ) -> Result<StatementOutcome> {
        let mut server_ms = None;
        let result_id = format!("{job_id}:{}", stmt.index);
        let mut unfinished = Unfinished { results: &self.results, id: &result_id, armed: true };
        let mut result = None;
        let mut notices = Vec::new();
        let mut truncated = false;
        let mut rows = 0usize;
        // Bytes kept: a row limit alone doesn't bound wide rows.
        let mut bytes = 0usize;
        // Half the outputs budget, less what this run's earlier results hold
        // in memory: a script's results are all active (never freed), so
        // together they stay within it too (see `result_memory_cap`).
        let earlier: usize = (0..stmt.index).map(|i| self.results.bytes_of(&format!("{job_id}:{i}"))).sum();
        // A job step's result counts against the job budget, less what the
        // other steps hold or reserved, so a job never takes the memory the
        // tabs' results have; past it the result goes to disk. Where results
        // can't spill, or the disk refuses, it keeps the tabs' cap, so it
        // isn't cut sooner than before.
        let job_tab = crate::outputs::is_job_tab(&req.tab_id);
        let tabs_cap = result_memory_cap(self.outputs.tabs_budget(), earlier);
        let job_step = job_tab && self.results.spill_root().is_some();
        // Steps loading at the same time share the job budget: this one's
        // share is worked out and reserved at once, and counts for the
        // others until its output is registered.
        let (max_bytes, _reserved) = if job_step {
            let (b, r) = self.outputs.reserve_job_share(&result_id, &req.tab_id, |free| result_memory_cap(free, earlier));
            (b, Some(r))
        } else {
            (tabs_cap, None)
        };
        // Where it stays in memory (the disk refused a move), a job step's
        // result is cut where a tab's would be.
        let cut_bytes = if job_step { max_bytes.max(tabs_cap) } else { max_bytes };
        let mut too_big = false;
        // Free memory of the computer when it ran low (and the result stopped).
        let mut low_free: Option<u64> = None;
        // Past its memory the result moves to disk and keeps loading; it stops
        // only when the disk runs low (`low_disk`) or writing fails.
        let mut on_disk = false;
        let mut low_disk: Option<u64> = None;
        let mut spill_failed: Option<String> = None;
        // Stopped at the grid's 32-bit row ids.
        let mut at_id_cap = false;
        let t0 = Instant::now();
        let mut last_progress = Instant::now();

        loop {
            let Some(ev) = stream.next().await else {
                return Err(EngineError::new("internal", "result stream ended unexpectedly"));
            };
            match ev {
                Ok(StreamEvent::Schema(schema)) => {
                    result = Some(self.results.create(result_id.clone(), schema));
                }
                Ok(StreamEvent::Batch(mut batch)) => {
                    let Some(rs) = &result else { continue };
                    if let Some(l) = limit {
                        let room = l.saturating_sub(rows);
                        if batch.num_rows() > room {
                            // A row beyond the cap exists: keep `room` rows
                            // and stop the query.
                            truncated = true;
                            batch = batch.slice(0, room);
                        }
                    }
                    let size = databrain_connector_core::memory_size([&batch]);
                    // Row ids in the grid are 32-bit: a result stops there.
                    let room = (u32::MAX as usize).saturating_sub(rows);
                    if batch.num_rows() > room {
                        batch = batch.slice(0, room);
                        truncated = true;
                        at_id_cap = true;
                    }
                    if on_disk {
                        if let Some(free) = self.disk_still_low(job_tab) {
                            low_disk = Some(free);
                            cancel.cancel();
                            break;
                        }
                        // On disk the result holds little memory (a file's rows and a
                        // few pages): it stops only when memory is critically low.
                        if let Some(free) = self.outputs.critically_low_memory() {
                            low_free = Some(free);
                            cancel.cancel();
                            break;
                        }
                    } else if batch.num_rows() > 0 {
                        // Past the memory a result may keep, or the computer low on
                        // free memory: move the result to disk and keep loading.
                        let low = if !truncated && rows > 0 { self.outputs.low_memory() } else { None };
                        // Once the disk refused (low, or a write failed), not tried
                        // again for each batch (a job step's cut comes later than
                        // its share: each try would rewrite the rows so far).
                        let refused = low_disk.is_some() || spill_failed.is_some();
                        let spilled = if !refused && (bytes + size > max_bytes || low.is_some()) {
                            self.spill(rs, &result_id, &batch, job_tab)
                        } else {
                            Spilled::NotHere
                        };
                        // Why it stays in memory, for the stop message: the
                        // disk, not the memory, when that's what refused.
                        let moved = matches!(spilled, Spilled::Yes);
                        match spilled {
                            Spilled::LowDisk(free) => low_disk = Some(free),
                            Spilled::Failed(e) => spill_failed = Some(e),
                            Spilled::Yes | Spilled::NotHere => {}
                        }
                        if moved {
                            on_disk = true;
                        } else if let Some(free) = low {
                            // Can't spill: stop before taking more, keeping the rows so far.
                            low_free = Some(free);
                            cancel.cancel();
                            break;
                        } else if bytes + size > cut_bytes {
                            // Can't spill: the rows that fit, copied so the rest of
                            // the batch's buffers are freed.
                            let keep = ((cut_bytes.saturating_sub(bytes) as u128 * batch.num_rows() as u128) / size as u128) as usize;
                            // At least one row, so the result shows what it holds.
                            let keep = if rows == 0 { keep.max(1) } else { keep };
                            let idx = databrain_connector_core::arrow::array::UInt32Array::from_iter_values(0..keep.min(batch.num_rows()) as u32);
                            batch = databrain_connector_core::arrow::compute::take_record_batch(&batch, &idx).unwrap_or_else(|_| batch.slice(0, idx.len()));
                            truncated = true;
                            too_big = true;
                        }
                    }
                    rows += batch.num_rows();
                    if !on_disk {
                        bytes += databrain_connector_core::memory_size([&batch]);
                    }
                    // Handed on as read, when the tab is tapped (`dbr --csv`):
                    // the rows kept after the row limit and the cuts above (a
                    // later failed write to disk can still drop some from the
                    // result). A slow reader slows the query.
                    let tap = self.taps.lock().get(&req.tab_id).cloned();
                    if let Some(tap) = tap
                        && batch.num_rows() > 0
                    {
                        let _ = tap.send(TappedBatch { index: stmt.index, batch: batch.clone() }).await;
                    }
                    // On disk a file due is written outside the lock (the
                    // grid and the outputs list read the result meanwhile).
                    let job = rs.lock().push_deferred(batch);
                    if job.is_some() {
                        blocking_io(|| write_files(rs, job, false));
                    }
                    let failed = if on_disk { rs.lock().spill_error().map(str::to_string) } else { None };
                    if let Some(e) = failed {
                        spill_failed = Some(e);
                        cancel.cancel();
                        break;
                    }
                    if truncated {
                        cancel.cancel();
                        break;
                    }
                    if last_progress.elapsed() >= PROGRESS_INTERVAL {
                        last_progress = Instant::now();
                        let ev = JobEvent::Progress {
                            job_id: job_id.to_string(),
                            tab_id: req.tab_id.clone(),
                            index: stmt.index,
                            rows,
                            elapsed_ms: t0.elapsed().as_millis() as u64,
                        };
                        if let Some(w) = waiter {
                            let _ = w.send(ev.clone());
                        }
                        self.events.emit(ev);
                    }
                }
                Ok(StreamEvent::Notice(n)) => notices.push(n),
                Ok(StreamEvent::Downloading(notice)) => {
                    let ms = started.elapsed().as_millis() as u64;
                    server_ms = Some(ms); // last one: a fallback re-runs the statement
                    if let Some(n) = &notice {
                        notices.push(n.clone());
                    }
                    self.events.emit(JobEvent::Downloading {
                        job_id: job_id.to_string(),
                        tab_id: req.tab_id.clone(),
                        index: stmt.index,
                        notice,
                        elapsed_ms: ms,
                    });
                }
                Ok(StreamEvent::Done(summary)) => {
                    let (info, failed) = result.as_ref().map_or((None, None), |rs| {
                        blocking_io(|| {
                            // The last file, outside the lock.
                            let last = rs.lock().flush_deferred();
                            write_files(rs, last, true);
                            let mut g = rs.lock();
                            g.finish(false);
                            let failed = g.spill_error().map(str::to_string);
                            // The last file couldn't be written: the rows in files stay.
                            if failed.is_some() {
                                g.finish(true);
                            }
                            (Some(g.info()), failed)
                        })
                    });
                    if let Some(i) = info.as_ref().filter(|i| i.on_disk) {
                        notices.push(on_disk_notice(i.disk_bytes));
                    }
                    if let (Some(e), Some(i)) = (&failed, &info) {
                        notices.push(format!("Showing the first {} rows: writing the last of the result to disk failed ({e}).", i.total_rows));
                    }
                    unfinished.armed = false;
                    return Ok(StatementOutcome { result: info, rows_affected: summary.rows_affected, notices, server_ms });
                }
                Err(e) => return Err(e.into()),
            }
        }

        // Only reached when stopped at the row cap.
        let info = result.as_ref().map(|rs| {
            blocking_io(|| {
                let last = rs.lock().flush_deferred();
                write_files(rs, last, true);
                let mut g = rs.lock();
                g.finish(true);
                g.info()
            })
        });
        unfinished.armed = false;
        if let Some(i) = info.as_ref().filter(|i| i.on_disk) {
            notices.push(on_disk_notice(i.disk_bytes));
        }
        // What the result holds (a failed write drops the rows not yet in a file).
        let rows = info.as_ref().map_or(rows, |i| i.total_rows);
        notices.push(if let Some(free) = low_disk {
            format!(
                "Showing the first {rows} rows: the disk has only {} free. Free some disk space, or export the query to get every row.",
                crate::outputs::size_text(free as usize)
            )
        } else if let Some(e) = &spill_failed {
            format!("Showing the first {rows} rows: writing the result to disk failed ({e}).")
        } else if let Some(free) = low_free {
            format!(
                "Showing the first {rows} rows: the computer is low on free memory ({} free). Close other programs or results, or export the query to get every row.",
                crate::outputs::size_text(free as usize)
            )
        } else if at_id_cap {
            format!("Showing the first {rows} rows: a result holds at most 4,294,967,295 rows. Export the query to get every row.")
        } else if too_big {
            format!(
                "Showing the first {rows} rows: they fill the {} a result may keep in memory. Select fewer columns, or export the query to get every row.",
                crate::outputs::size_text(max_bytes)
            )
        } else {
            format!("Showing the first {rows} rows. Increase the row limit to fetch more.")
        });
        // Wait for the connector to finish cancelling before the session is
        // reused; otherwise a server-side cancel could hit the next statement.
        let _ = tokio::time::timeout(Duration::from_secs(15), async { while stream.next().await.is_some() {} }).await;
        Ok(StatementOutcome { result: info, rows_affected: None, notices, server_ms })
    }
}

/// What [`QueryEngine::run_and_wait`] returns when the run ends. Failed
/// with no statement saying why: the run's task died (its guard reports
/// it), which callers must not take for success.
pub(crate) fn finished(status: RunStatus, out: Vec<StatementOutcomeView>) -> Result<Vec<StatementOutcomeView>> {
    if status == RunStatus::Error && out.iter().all(|o| o.error.is_none()) {
        return Err(EngineError::new("internal", "the query stopped unexpectedly"));
    }
    Ok(out)
}

/// The `jobs` entry of a run, released on any exit of its task: normally
/// `run_job` removes it and emits `JobFinished`; after a panic the guard
/// does both (status Error), so the tab can run again and the UI and any
/// waiter stop waiting.
pub(crate) struct JobGuard {
    pub(crate) engine: Arc<QueryEngine>,
    pub(crate) job_id: String,
    pub(crate) tab_id: String,
    pub(crate) waiter: Option<Waiter>,
    pub(crate) done: bool,
}

impl JobGuard {
    pub(crate) fn finish(mut self) {
        self.done = true;
    }
}

impl Drop for JobGuard {
    fn drop(&mut self) {
        if self.done || self.engine.jobs.lock().remove(&self.job_id).is_none() {
            return;
        }
        tracing::error!(job = %self.job_id, "a run ended without finishing (panic); its tab is free again");
        let ev =
            JobEvent::JobFinished { job_id: self.job_id.clone(), tab_id: self.tab_id.clone(), status: RunStatus::Error, duration_ms: 0, in_transaction: false };
        if let Some(w) = &self.waiter {
            let _ = w.send(ev.clone());
        }
        self.engine.events.emit(ev);
        self.engine.outputs_changed();
    }
}

/// Removes a statement's stored result unless it finished (see
/// [`QueryEngine::collect_stream`]).
struct Unfinished<'a> {
    results: &'a ResultStore,
    id: &'a str,
    armed: bool,
}

impl Drop for Unfinished<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.results.remove(self.id);
        }
    }
}

/// What a result kept on disk tells the user.
/// The memory statement N of a script may keep, `earlier` being what the
/// statements before it keep: half the budget less that, and at least 4 MB
/// (so a small result still shows) while the script is within the budget.
/// Past it, 64 KB (and at least one row; the rest goes to disk where results
/// spill, or is cut): each statement got 4 MB more before, never freed, so
/// N statements kept N × 4 MB.
pub(crate) fn result_memory_cap(budget: usize, earlier: usize) -> usize {
    let floor = if earlier.saturating_add(4 << 20) <= budget { 4 << 20 } else { 64 << 10 };
    (budget / 2).saturating_sub(earlier).max(floor)
}

/// Why a result stayed in memory ([`QueryEngine::spill`]).
enum Spilled {
    Yes,
    /// Results don't move to disk here (no spill folder).
    NotHere,
    /// The disk is low: its free bytes.
    LowDisk(u64),
    /// Its rows couldn't be written.
    Failed(String),
}

/// Write `job` and the files due after it, each outside `rs`'s lock
/// (`all`: the rows still waiting too). A failed write is kept as the
/// result's `spill_error`.
fn write_files(rs: &parking_lot::Mutex<databrain_result_store::ResultSet>, mut job: Option<databrain_result_store::spill::FileJob>, all: bool) {
    while let Some(j) = job {
        let done = j.write();
        job = rs.lock().adopt_file(done, all);
    }
}

fn on_disk_notice(disk_bytes: u64) -> String {
    format!(
        "This result is larger than the memory a result may keep: its rows are on disk ({}) and read as you scroll. Sorting, filtering, column statistics and charts apply to the rows loaded in the grid (scroll further to include more); find in the whole result, export, \"Load into…\" and SQL on results.<name> read every row.",
        crate::outputs::size_text(disk_bytes as usize)
    )
}

/// Run file I/O from async code without stalling the runtime's worker
/// (when the runtime has others to take over; a single-threaded one just runs it).
fn blocking_io<T>(f: impl FnOnce() -> T) -> T {
    match tokio::runtime::Handle::try_current() {
        Ok(h) if h.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => tokio::task::block_in_place(f),
        _ => f(),
    }
}

/// Rows of statement `index` as they were read ([`QueryEngine::tap_rows`]).
#[derive(Debug, Clone)]
pub struct TappedBatch {
    pub index: usize,
    pub batch: databrain_connector_core::arrow::record_batch::RecordBatch,
}

/// While held, the tab's rows are handed on ([`QueryEngine::tap_rows`]).
pub struct RowTap {
    engine: Arc<QueryEngine>,
    tab_id: String,
}

impl Drop for RowTap {
    fn drop(&mut self) {
        self.engine.taps.lock().remove(&self.tab_id);
    }
}

/// Statements after which a table's kept description may be wrong.
fn changes_tables(kind: StatementKind) -> bool {
    matches!(kind, StatementKind::Ddl | StatementKind::Other | StatementKind::Unknown | StatementKind::Transaction)
}
