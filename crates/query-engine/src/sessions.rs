//! Sessions per tab (the AI's sandboxed ones too), idle disconnects,
//! manual commit and transactions, the metadata and indexing sessions.

use super::*;

impl QueryEngine {
    pub(crate) async fn session(&self, connection_id: &str, tab_id: &str) -> Result<Arc<dyn Session>> {
        self.session_for(connection_id, tab_id, Origin::User).await
    }

    /// Whether SQL from `origin` runs in a sandboxed session on this
    /// connection: AI and MCP on DuckDB, unless the connection lets the AI
    /// access files and the network.
    pub(crate) fn sandboxed(&self, connection_id: &str, origin: Origin) -> Result<bool> {
        if !matches!(origin, Origin::Ai | Origin::Mcp) {
            return Ok(false);
        }
        let p = self.workspace.get_connection(connection_id)?;
        Ok(p.config.kind == databrain_connector_core::ConnectorKind::Duckdb && !p.ai_policy.external_access)
    }

    /// The session of `tab_id` for SQL from `origin` (see [`Self::sandboxed`]).
    pub(crate) async fn session_for(&self, connection_id: &str, tab_id: &str, origin: Origin) -> Result<Arc<dyn Session>> {
        let sandbox = self.sandboxed(connection_id, origin)?;
        let key = (connection_id.to_string(), tab_id.to_string());
        {
            let mut g = self.sessions.lock();
            match g.get(&key) {
                // A session opened without the sandbox is never used for it.
                Some(s) if sandbox && !s.sandboxed => {
                    g.remove(&key);
                }
                Some(s) => {
                    *s.last_used.lock() = Instant::now();
                    return Ok(s.session.clone());
                }
                None => {}
            }
        }
        let gate = self
            .connecting
            .lock()
            .entry(key.clone())
            .or_default()
            .clone();
        let _g = gate.lock().await;
        if let Some(s) = self.sessions.lock().get(&key).filter(|s| s.sandboxed || !sandbox) {
            return Ok(s.session.clone());
        }
        let mut profile = self.workspace.get_connection(connection_id)?;
        profile.config = profile.config.normalized();
        if profile.config.kind == databrain_connector_core::ConnectorKind::Duckdb && profile.config.file_path.as_deref().is_some_and(|f| !f.trim().is_empty()) {
            // One database instance per file: a sandboxed one and an open
            // one can't share it. Close the idle sessions of the other kind
            // that hold no state (the assistant's, or the explorer's).
            self.sessions.lock().retain(|(c, t), live| {
                let other = c == connection_id && live.sandboxed != sandbox;
                let stateless = !sandbox || t == META_TAB || t == INDEX_TAB;
                !(other && stateless && Arc::strong_count(&live.session) == 1 && !live.session.in_transaction())
            });
        }
        if sandbox {
            profile.config.options.insert(databrain_connector_core::SANDBOX_OPTION.into(), "1".into());
        }
        let connector = self.registry.get(profile.config.kind)?;
        let creds = self.credentials(&profile)?;
        self.preflight(&profile.config).await?;
        let (cfg, tunnel) = self.tunnel(&profile.config, Some(&profile.id), None).await?;
        let started = Instant::now();
        let kind = profile.config.kind;
        let connected = match connector.connect(&cfg, creds.clone()).await {
            Err(e) if e.kind == ErrorKind::Auth => {
                // Expired/revoked token: refresh once and retry.
                tracing::info!(connection = connection_id, ?kind, "sign-in refused, refreshing credentials once");
                creds.invalidate().await;
                connector.connect(&cfg, creds).await
            }
            r => r,
        };
        let session: Arc<dyn Session> = match connected {
            Ok(s) => {
                tracing::info!(connection = connection_id, ?kind, tab = tab_id, sandbox, ms = started.elapsed().as_millis() as u64, "connected");
                Arc::from(s)
            }
            Err(e) => {
                tracing::warn!(connection = connection_id, ?kind, tab = tab_id, error = %e.message, "connection failed");
                return Err(e.into());
            }
        };
        self.sessions.lock().insert(key, Live { session: session.clone(), _tunnel: tunnel, sandboxed: sandbox, last_used: parking_lot::Mutex::new(Instant::now()) });
        Ok(session)
    }

    pub(crate) fn drop_session(&self, connection_id: &str, tab_id: &str) {
        self.sessions
            .lock()
            .remove(&(connection_id.to_string(), tab_id.to_string()));
    }

    /// Close sessions unused for `idle` (their connections go back to the
    /// server; the next statement reconnects). Kept: sessions with a running
    /// statement, an open transaction, or still referenced elsewhere (an
    /// export or catalog call in progress). Returns how many were closed.
    pub fn close_idle(&self, idle: Duration) -> usize {
        let busy: Vec<String> = self.jobs.lock().values().map(|j| j.session_key.clone()).collect();
        let now = Instant::now();
        let mut g = self.sessions.lock();
        let before = g.len();
        g.retain(|(_, key), live| {
            let in_use = busy.contains(key) || Arc::strong_count(&live.session) > 1;
            in_use || live.session.in_transaction() || now.duration_since(*live.last_used.lock()) < idle
        });
        before - g.len()
    }

    /// The idle time after which sessions close ([`IDLE_DISCONNECT_SETTING`]); `None` = never.
    pub fn idle_disconnect(&self) -> Option<Duration> {
        let minutes = self
            .workspace
            .get_setting(IDLE_DISCONNECT_SETTING)
            .ok()
            .flatten()
            .and_then(|v| v.as_u64())
            .unwrap_or(IDLE_DISCONNECT_DEFAULT_MINUTES);
        (minutes > 0).then(|| Duration::from_secs(minutes * 60))
    }

    /// Close every session of a connection (after editing or deleting it).
    pub fn disconnect(&self, connection_id: &str) {
        self.sessions.lock().retain(|(c, _), _| c != connection_id);
    }

    /// A session on a separate tab, for tests that set up data.
    #[doc(hidden)]
    pub async fn session_for_tests(&self, connection_id: &str) -> Result<Arc<dyn Session>> {
        self.session(connection_id, "__tests__").await
    }

    /// Connection ids that currently have at least one open session.
    pub fn connected_ids(&self) -> Vec<String> {
        let mut v: Vec<String> = self.sessions.lock().keys().map(|(c, _)| c.clone()).collect();
        v.sort();
        v.dedup();
        v
    }

    /// Release the session and results that belong to a closed tab.
    pub fn close_tab(&self, tab_id: &str) {
        let running: Vec<String> = self
            .jobs
            .lock()
            .iter()
            .filter(|(_, j)| j.tab_id == tab_id || j.session_key == tab_id)
            .map(|(id, _)| id.clone())
            .collect();
        for id in running {
            self.cancel(&id);
        }
        self.sessions.lock().retain(|(_, t), _| t != tab_id);
        self.outputs.close_tab(tab_id);
        self.outputs_changed();
    }

    // -------------------------------------------------------------- transactions

    /// Manual commit on or off for a session key (a tab).
    pub fn set_manual_commit(&self, session_key: &str, manual: bool) {
        let mut g = self.manual_commit.lock();
        if manual {
            g.insert(session_key.to_string());
        } else {
            g.remove(session_key);
        }
    }

    pub fn is_manual_commit(&self, session_key: &str) -> bool {
        self.manual_commit.lock().contains(session_key)
    }

    /// The session `session_key` of a connection has an open transaction
    /// (false when it isn't connected).
    pub fn in_transaction(&self, connection_id: &str, session_key: &str) -> bool {
        self.sessions
            .lock()
            .get(&(connection_id.to_string(), session_key.to_string()))
            .is_some_and(|l| l.session.in_transaction())
    }

    /// COMMIT or ROLLBACK the session's transaction. Returns whether one is
    /// still open afterwards. Nothing happens without a session.
    pub async fn end_transaction(&self, connection_id: &str, session_key: &str, commit: bool) -> Result<bool> {
        if self.jobs.lock().values().any(|j| j.session_key == session_key) {
            return Err(EngineError::new("busy", "A query is running in this tab: wait for it or stop it first"));
        }
        let Some(session) = self.sessions.lock().get(&(connection_id.to_string(), session_key.to_string())).map(|l| l.session.clone()) else {
            return Ok(false);
        };
        let sql = if commit { "COMMIT" } else { "ROLLBACK" };
        // SQL Server nests BEGIN TRAN: each COMMIT ends one level.
        for _ in 0..32 {
            session.execute(sql, ExecOptions::default()).await?.collect().await?;
            if !commit || !session.in_transaction() {
                break;
            }
        }
        Ok(session.in_transaction())
    }

    /// Manual commit: open a transaction before a write when none is open.
    pub(crate) async fn begin_if_manual(&self, req: &RunRequest, kind: databrain_connector_core::ConnectorKind, stmt: &PlannedStatement) -> Result<()> {
        use databrain_connector_core::sql::StatementKind as K;
        let write = match stmt.classification.kind {
            K::Dml => true,
            // MySQL commits DDL at once; elsewhere DDL is transactional.
            K::Ddl => kind != databrain_connector_core::ConnectorKind::Mysql,
            _ => false,
        };
        if !write || !self.is_manual_commit(req.session_key()) {
            return Ok(());
        }
        let Some((begin, _, _)) = edit::apply_style(kind) else { return Ok(()) };
        let session = self.session_for(&req.connection_id, req.session_key(), req.origin).await?;
        if !session.in_transaction() {
            session.execute(begin, ExecOptions::default()).await?.collect().await?;
        }
        Ok(())
    }

    // -------------------------------------------------------------- metadata

    pub(crate) async fn with_meta<T, F, Fut>(&self, connection_id: &str, f: F) -> Result<T>
    where
        F: Fn(Arc<dyn Session>) -> Fut,
        Fut: std::future::Future<Output = databrain_connector_core::Result<T>>,
    {
        self.with_tab(connection_id, META_TAB, f).await
    }

    pub(crate) async fn with_tab<T, F, Fut>(&self, connection_id: &str, tab: &str, f: F) -> Result<T>
    where
        F: Fn(Arc<dyn Session>) -> Fut,
        Fut: std::future::Future<Output = databrain_connector_core::Result<T>>,
    {
        let s = self.session(connection_id, tab).await?;
        match f(s).await {
            Ok(v) => Ok(v),
            Err(e) if e.kind == ErrorKind::Connection => {
                // Stale connection: reconnect once.
                self.drop_session(connection_id, tab);
                let s = self.session(connection_id, tab).await?;
                Ok(f(s).await?)
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Session used by AI indexing: its own for server databases; local
    /// files (SQLite, DuckDB) share the metadata session (one handle per file).
    pub(crate) fn index_tab(&self, connection_id: &str) -> &'static str {
        match self.workspace.get_connection(connection_id).map(|p| p.config.kind) {
            Ok(k) if !k.traits().local => INDEX_TAB,
            _ => META_TAB,
        }
    }

    /// Close indexing's session (after a run or a cancel). Dropping it ends
    /// the server session, which also stops a statement still running there.
    pub fn end_index_session(&self, connection_id: &str) {
        if self.index_tab(connection_id) == INDEX_TAB {
            self.drop_session(connection_id, INDEX_TAB);
        }
    }

    /// Close the database sessions of a run key (keeps its outputs).
    pub fn release_sessions(&self, tab: &str) {
        self.sessions.lock().retain(|(_, t), _| t != tab);
    }
}
