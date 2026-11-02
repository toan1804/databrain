//! Running SQL: planning statements, safety prompts, the run loop and
//! waiting for a run (AI, MCP, job steps).

use super::*;

impl QueryEngine {
    // -------------------------------------------------------------- jobs

    pub fn plan(&self, profile: &ConnectionProfile, sql: &str) -> Vec<PlannedStatement> {
        let kind = profile.config.kind;
        split_statements(sql, kind)
            .into_iter()
            .enumerate()
            .map(|(index, s)| PlannedStatement { index, classification: classify(&s.sql, kind), sql: s.sql, start: s.start, end: s.end })
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
            match databrain_connector_core::sql::substitute_parameters(&p.sql, kind, params) {
                Ok(sql) if sql != p.sql => {
                    p.classification = classify(&sql, kind);
                    p.sql = sql;
                }
                Ok(_) => {}
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
            let c = &p.classification;
            if c.missing_where {
                reasons.push(format!("Statement {} is {} without a WHERE clause and will affect every row.", p.index + 1, c.keyword));
            }
        }
        if profile.env == EnvTag::Prod {
            let writes = plan.iter().filter(|p| p.classification.kind.is_write() || p.classification.kind == StatementKind::Unknown).count();
            if writes > 0 {
                reasons.push(format!("{writes} statement(s) may modify data or schema on the production connection \"{}\".", profile.name));
            }
        }
        reasons
    }

    pub fn run(self: &Arc<Self>, req: RunRequest) -> Result<RunResponse> {
        self.start_run(req, None)
    }

    /// [`Self::run`]; `waiter` also gets this run's outcome events
    /// (statement finished/failed, job finished), so a caller waiting for it
    /// can't miss them the way a lagging broadcast subscriber can.
    pub(crate) fn start_run(self: &Arc<Self>, req: RunRequest, waiter: Option<Waiter>) -> Result<RunResponse> {
        let profile = self.workspace.get_connection(&req.connection_id)?;
        let mut plan = self.plan(&profile, &req.sql);
        if plan.is_empty() {
            return Err(EngineError::new("invalid", "Nothing to run"));
        }
        Self::bind_parameters(&mut plan, profile.config.kind, &req.params)?;
        if profile.config.read_only {
            // Only reads (and harmless USE/SET): a transaction or session
            // statement could turn read-only off for what follows.
            if let Some(p) = plan.iter().find(|p| !databrain_connector_core::sql::read_only_allows(&p.sql, &p.classification)) {
                let why = if p.classification.kind.is_write() {
                    ""
                } else {
                    ": on a read-only connection only queries run (transactions and session settings could make it writable)"
                };
                return Err(EngineError::new(
                    "invalid",
                    format!("Connection \"{}\" is read-only; statement {} ({}) was not run{why}.", profile.name, p.index + 1, p.classification.keyword),
                ));
            }
        }
        if !req.confirmed {
            let reasons = Self::safety_reasons(&profile, &plan);
            if !reasons.is_empty() {
                return Ok(RunResponse::NeedsConfirmation { reasons, statements: plan });
            }
        }
        if let Some(n) = req.output_name.as_deref().filter(|n| !n.is_empty()) {
            outputs::validate_name(n)?;
            self.outputs.check_owner(n, &req.tab_id)?;
        }
        let job_id = uuid::Uuid::new_v4().to_string();
        let cancel = CancellationToken::new();
        {
            // Checked and claimed under one lock: two runs started at once
            // on the same tab or session can't both pass the check.
            let mut jobs = self.jobs.lock();
            if jobs.values().any(|j| j.tab_id == req.tab_id || j.session_key == req.session_key()) {
                return Err(EngineError::new("busy", "A query is already running in this tab"));
            }
            jobs.insert(job_id.clone(), JobHandle { tab_id: req.tab_id.clone(), session_key: req.session_key().to_string(), cancel: cancel.clone() });
        }
        // Previous outputs of this tab become "recent" (evictable).
        self.outputs.begin_run(&req.tab_id);

        let engine = self.clone();
        let jid = job_id.clone();
        let statements = plan.clone();
        tokio::spawn(async move {
            engine.run_job(jid, req, statements, cancel, waiter).await;
        });
        Ok(RunResponse::Started { job_id, statements: plan })
    }

    /// Run and wait for completion (for the AI agent / MCP and job steps);
    /// `confirmed` skips the interactive safety prompt because the caller
    /// has already enforced its own policy.
    ///
    /// The outcome comes on a channel of its own, not from the [`EventHub`]
    /// broadcast: a subscriber that lags behind there loses events, and a
    /// lost `JobFinished` left the caller (a job step, with every step
    /// downstream of it) waiting forever after the query had ended.
    pub async fn run_and_wait(self: &Arc<Self>, _hub: &EventHub, req: RunRequest, cancel: Option<CancellationToken>) -> Result<Vec<StatementOutcomeView>> {
        let (waiter, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let statements = match self.start_run(req, Some(waiter))? {
            RunResponse::Started { job_id, statements } => (job_id, statements),
            RunResponse::NeedsConfirmation { reasons, .. } => {
                return Err(EngineError::new("invalid", reasons.join(" ")));
            }
        };
        let (job_id, plan) = statements;
        let mut out: Vec<StatementOutcomeView> = plan
            .iter()
            .map(|p| StatementOutcomeView {
                index: p.index,
                sql: p.sql.clone(),
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
                JobEvent::JobFinished { .. } => return Ok(out),
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
            self.events.emit(JobEvent::StatementStarted { job_id: job_id.clone(), tab_id: req.tab_id.clone(), index: stmt.index, sql: stmt.sql.clone() });
            let started_at = now_ms();
            let t0 = Instant::now();
            let outcome = match self.begin_if_manual(&req, conn_kind, stmt).await {
                Ok(()) => self.run_statement(&job_id, &req, stmt, limit, cancel.child_token()).await,
                Err(e) => Err(e),
            };
            let duration_ms = t0.elapsed().as_millis() as u64;

            let output = match &outcome {
                Ok(StatementOutcome { result: Some(r), .. }) => Some(Box::new(self.outputs.add(NewOutput {
                    result: r.clone(),
                    connection_id: req.connection_id.clone(),
                    connection_name: conn_name.clone(),
                    kind: conn_kind,
                    sql: stmt.sql.clone(),
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
            let _ = self.workspace.add_history(NewHistory {
                origin: req.origin,
                connection_id: Some(req.connection_id.clone()),
                sql: stmt.sql.clone(),
                started_at,
                duration_ms: duration_ms as i64,
                rows,
                status: Some(hist_status),
                error,
                output_handle: output.as_ref().map(|o| o.handle.clone()),
                result_id: output.as_ref().map(|o| o.result_id.clone()),
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
                    if matches!(e.kind.as_str(), "connection") {
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
        self.jobs.lock().remove(&job_id);
        self.outputs_changed();
        let in_transaction = self.in_transaction(&req.connection_id, req.session_key());
        // What ran and how it ended; never the SQL itself.
        tracing::debug!(connection = %req.connection_id, tab = %req.tab_id, origin = req.origin.as_str(), statements = plan.len(), ?status, ms = job_start.elapsed().as_millis() as u64, "run finished");
        emit(JobEvent::JobFinished { job_id, tab_id: req.tab_id, status, duration_ms: job_start.elapsed().as_millis() as u64, in_transaction });
    }

    pub(crate) async fn run_statement(
        &self,
        job_id: &str,
        req: &RunRequest,
        stmt: &PlannedStatement,
        limit: Option<usize>,
        cancel: CancellationToken,
    ) -> Result<StatementOutcome> {
        let started = Instant::now();
        let mut server_ms = None;
        let session = tokio::select! {
            s = self.session_for(&req.connection_id, req.session_key(), req.origin) => s?,
            _ = cancel.cancelled() => return Err(ConnectorError::cancelled().into()),
        };
        let batch_size = limit.map(|l| l.clamp(1, 2000)).unwrap_or(2000);
        let mut stream = session
            .execute(
                &stmt.sql,
                ExecOptions {
                    batch_size,
                    cancel: cancel.clone(),
                    // One row past the cap tells us the result was truncated.
                    max_rows: limit.map(|l| l.saturating_add(1)),
                    output_masks: req.output_masks.clone(),
                },
            )
            .await?;

        let result_id = format!("{job_id}:{}", stmt.index);
        let mut result = None;
        let mut notices = Vec::new();
        let mut truncated = false;
        let mut rows = 0usize;
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
                    rows += batch.num_rows();
                    rs.lock().push(batch);
                    if truncated {
                        cancel.cancel();
                        break;
                    }
                    if last_progress.elapsed() >= PROGRESS_INTERVAL {
                        last_progress = Instant::now();
                        self.events.emit(JobEvent::Progress {
                            job_id: job_id.to_string(),
                            tab_id: req.tab_id.clone(),
                            index: stmt.index,
                            rows,
                            elapsed_ms: t0.elapsed().as_millis() as u64,
                        });
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
                    let info = result.as_ref().map(|rs| {
                        let mut g = rs.lock();
                        g.finish(false);
                        g.info()
                    });
                    return Ok(StatementOutcome { result: info, rows_affected: summary.rows_affected, notices, server_ms });
                }
                Err(e) => {
                    self.results.remove(&result_id);
                    return Err(e.into());
                }
            }
        }

        // Only reached when stopped at the row cap.
        let info = result.as_ref().map(|rs| {
            let mut g = rs.lock();
            g.finish(true);
            g.info()
        });
        notices.push(format!("Showing the first {rows} rows. Increase the row limit to fetch more."));
        // Wait for the connector to finish cancelling before the session is
        // reused; otherwise a server-side cancel could hit the next statement.
        let _ = tokio::time::timeout(Duration::from_secs(15), async { while stream.next().await.is_some() {} }).await;
        Ok(StatementOutcome { result: info, rows_affected: None, notices, server_ms })
    }
}
