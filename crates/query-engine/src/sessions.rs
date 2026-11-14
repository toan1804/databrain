//! Sessions per tab (the AI's sandboxed ones too), idle disconnects,
//! manual commit and transactions, the metadata and indexing sessions.

use super::*;
use databrain_connector_core::TxState;

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
        let gate = ConnectGate::enter(&self.connecting, &key);
        let _g = gate.gate.lock().await;
        if let Some(s) = self.sessions.lock().get(&key).filter(|s| s.sandboxed || !sandbox) {
            return Ok(s.session.clone());
        }
        let mut profile = self.workspace.get_connection(connection_id)?;
        profile.config = profile.config.normalized();
        if profile.config.kind == databrain_connector_core::ConnectorKind::Duckdb && profile.config.file_path.as_deref().is_some_and(|f| !f.trim().is_empty()) {
            // One database instance per file: a sandboxed one and an open
            // one can't share it. Close the idle sessions of the other kind
            // that hold no state (the assistant's, or the explorer's).
            self.drop_sessions(|(c, t), live| {
                let other = c == connection_id && live.sandboxed != sandbox;
                // The assistant's sessions (sandboxed) and catalog-only ones.
                let stateless = !sandbox || SessionRole::parse(t).catalog_only();
                other && stateless && Arc::strong_count(&live.session) == 1 && !live.session.in_transaction()
            });
        }
        if sandbox {
            profile.config.options.insert(databrain_connector_core::SANDBOX_OPTION.into(), "1".into());
        }
        let connector = self.registry.get(profile.config.kind)?;
        let creds = self.credentials(&profile)?;
        connector.prepare(&profile.config).await?;
        self.preflight(&profile.config).await?;
        let kind = profile.config.kind;
        // Reaching and signing in to the server is bounded like "Test
        // connection": a server that accepts the socket and never finishes
        // the handshake fails with a message instead of a spinner. Not for
        // browser sign-in (the user may take a while) or local files.
        let limit = (!profile.config.auth.is_interactive() && !kind.traits().local).then(|| self.connect_timeout_for(&profile.config));
        let reach = async {
            let (cfg, tunnel) = self.tunnel(&profile.config, Some(&profile.id), None).await?;
            self.read_stored_secret(&profile, creds.as_ref()).await?;
            let connected = match connector.connect(&cfg, creds.clone()).await {
                Err(e) if e.kind == ErrorKind::Auth => {
                    // Expired/revoked token: refresh once and retry.
                    tracing::info!(connection = connection_id, ?kind, "sign-in refused, refreshing credentials once");
                    creds.invalidate().await;
                    connector.connect(&cfg, creds.clone()).await
                }
                r => r,
            };
            Ok::<_, EngineError>((connected, tunnel))
        };
        let started = Instant::now();
        let (connected, tunnel) = match limit {
            None => reach.await?,
            Some(d) => match tokio::time::timeout(d, reach).await {
                Ok(r) => r?,
                Err(_) => {
                    tracing::warn!(connection = connection_id, ?kind, tab = tab_id, secs = d.as_secs(), "connect timed out");
                    return Err(EngineError::new(
                        "connection",
                        format!(
                            "No connection to \"{}\" after {} s: the server did not finish the handshake/sign-in (check host, port, SSL and firewall; Settings → Queries → Connecting sets the limit)",
                            profile.name,
                            d.as_secs()
                        ),
                    ));
                }
            },
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
        self.sessions
            .lock()
            .insert(key, Live { session: session.clone(), _tunnel: tunnel, sandboxed: sandbox, last_used: parking_lot::Mutex::new(Instant::now()) });
        Ok(session)
    }

    /// Stored secrets are read before connecting, within the connect time
    /// limit: the OS keychain can wait on a permission prompt (another
    /// binary, e.g. databrain-mcp, reading an item the app saved), which
    /// inside the driver's connect would look like a hang with no message.
    /// Read errors are left to the connector, which reports them in context.
    async fn read_stored_secret(&self, profile: &ConnectionProfile, creds: &dyn databrain_auth::CredentialSource) -> Result<()> {
        use databrain_auth::AuthMethod as A;
        if !matches!(profile.config.auth, A::Password { .. } | A::ApiToken { .. } | A::KeyPair { .. }) {
            return Ok(());
        }
        let limit = self.connect_timeout_for(&profile.config);
        if tokio::time::timeout(limit, creds.get()).await.is_err() {
            return Err(EngineError::new(
                "timeout",
                format!(
                    "Reading the credentials of \"{}\" from the credential store took more than {} s. A keychain access prompt may be waiting: allow DataBrain (or databrain-mcp) to read them, or keep secrets in the local vault instead (Settings)",
                    profile.name,
                    limit.as_secs()
                ),
            ));
        }
        Ok(())
    }

    pub(crate) fn drop_session(&self, connection_id: &str, tab_id: &str) {
        let gone = self.sessions.lock().remove(&(connection_id.to_string(), tab_id.to_string()));
        drop(gone); // after the lock: a connector's drop may take a while
    }

    /// Drop the session of `tab` only if it is still `s` (not one another
    /// caller opened since).
    pub(crate) fn drop_session_if(&self, connection_id: &str, tab_id: &str, s: &Arc<dyn Session>) {
        let gone = {
            let mut g = self.sessions.lock();
            let key = (connection_id.to_string(), tab_id.to_string());
            if g.get(&key).is_some_and(|l| Arc::ptr_eq(&l.session, s)) { g.remove(&key) } else { None }
        };
        drop(gone);
    }

    /// Remove the sessions `drop` picks; they are dropped after the lock is
    /// released (a connector's drop may block on the network). Returns how
    /// many.
    pub(crate) fn drop_sessions(&self, mut pick: impl FnMut(&SessionKey, &Live) -> bool) -> usize {
        let gone: Vec<(SessionKey, Live)> = self.sessions.lock().extract_if(|k, l| pick(k, l)).collect();
        gone.len()
    }

    /// Close sessions unused for `idle` (their connections go back to the
    /// server; the next statement reconnects). Kept: sessions with a running
    /// statement, an open transaction, or still referenced elsewhere (an
    /// export or catalog call in progress). Returns how many were closed.
    pub fn close_idle(&self, idle: Duration) -> usize {
        self.close_idle_with(Some(idle))
    }

    /// Close idle sessions, each connection after its own idle time (its
    /// `idle_disconnect_minutes` option; `0` = never), else after `default`
    /// (`None` = never). Returns how many were closed.
    pub fn close_idle_with(&self, default: Option<Duration>) -> usize {
        let busy: Vec<String> = self.jobs.lock().values().map(|j| j.session_key.clone()).collect();
        let now = Instant::now();
        let ids: std::collections::HashSet<String> = self.sessions.lock().keys().map(|(c, _)| c.clone()).collect();
        let limits: HashMap<String, Option<Duration>> = ids
            .into_iter()
            .map(|c| {
                let own = self
                    .workspace
                    .get_connection(&c)
                    .ok()
                    .and_then(|p| p.config.opt(databrain_connector_core::IDLE_DISCONNECT_OPTION).and_then(|v| v.trim().parse::<u64>().ok()));
                let limit = match own {
                    Some(0) => None,
                    Some(m) => Some(minutes(m)),
                    None => default,
                };
                (c, limit)
            })
            .collect();
        self.drop_sessions(|(c, key), live| {
            let in_use = busy.contains(key) || Arc::strong_count(&live.session) > 1;
            if in_use || live.session.in_transaction() {
                return false;
            }
            // A load's or export's session nobody uses any more (its owner
            // ended without closing it): nothing comes back to it.
            if SessionRole::parse(key).one_off() {
                return true;
            }
            let Some(idle) = limits.get(c).copied().flatten() else { return false };
            now.duration_since(*live.last_used.lock()) >= idle
        })
    }

    /// The idle time after which sessions close ([`IDLE_DISCONNECT_SETTING`]); `None` = never.
    pub fn idle_disconnect(&self) -> Option<Duration> {
        let minutes = self.workspace.get_setting(IDLE_DISCONNECT_SETTING).ok().flatten().and_then(|v| v.as_u64()).unwrap_or(IDLE_DISCONNECT_DEFAULT_MINUTES);
        (minutes > 0).then(|| self::minutes(minutes))
    }

    /// Close every session of a connection (after editing or deleting it).
    /// Running statements on it are stopped: a script's next statement
    /// would otherwise reconnect and run outside the transaction it began.
    pub fn disconnect(&self, connection_id: &str) {
        for j in self.jobs.lock().values().filter(|j| j.connection_id == connection_id) {
            j.cancel.cancel();
        }
        self.drop_sessions(|(c, _), _| c == connection_id);
    }

    /// Sign in again on the next call (the explorer's Refresh): closes the
    /// connection's metadata and indexing sessions, and its tab sessions
    /// that have no running statement, no open transaction and are not in
    /// use elsewhere (their next statement reconnects, as after an idle
    /// disconnect). Needed for rights the server applies to new sessions
    /// only, such as Oracle roles granted since the login. Returns how many
    /// sessions were closed.
    pub fn reconnect(&self, connection_id: &str) -> usize {
        let busy: Vec<String> = self.jobs.lock().values().map(|j| j.session_key.clone()).collect();
        self.drop_sessions(|(c, key), live| {
            if c != connection_id {
                return false;
            }
            // A catalog call still running keeps its handle; the map forgets it.
            if SessionRole::parse(key) == SessionRole::Meta {
                return true;
            }
            !(busy.contains(key) || Arc::strong_count(&live.session) > 1 || live.session.in_transaction())
        })
    }

    /// A session on a separate tab, for tests that set up data.
    #[doc(hidden)]
    pub async fn session_for_tests(&self, connection_id: &str) -> Result<Arc<dyn Session>> {
        self.session(connection_id, &SessionRole::Tests.key()).await
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
        {
            let mut jobs = self.jobs.lock();
            for j in jobs.values_mut().filter(|j| j.tab_id == tab_id || j.session_key == tab_id) {
                j.cancel.cancel();
                // Its own tab: what it still outputs is freed when it ends.
                j.tab_closed |= j.tab_id == tab_id;
            }
        }
        self.drop_sessions(|(_, t), _| t == tab_id);
        self.outputs.close_tab(tab_id);
        self.outputs_changed();
    }

    /// Claim the session `session_key` like a running query until the
    /// returned [`Claim`] drops: a run, edit, plan, Commit/Rollback or
    /// export can't start on it meanwhile (checked and claimed under one
    /// lock), and closing the tab cancels `cancel`. `busy` is the message
    /// when something already holds it.
    pub(crate) fn claim(&self, connection_id: &str, session_key: &str, cancel: CancellationToken, busy: &str) -> Result<Claim<'_>> {
        let mut jobs = self.jobs.lock();
        if jobs.values().any(|j| j.session_key == session_key || j.tab_id == session_key) {
            return Err(EngineError::new("busy", busy));
        }
        let id = format!("claim:{}", uuid::Uuid::new_v4());
        jobs.insert(
            id.clone(),
            JobHandle {
                connection_id: connection_id.to_string(),
                tab_id: session_key.to_string(),
                session_key: session_key.to_string(),
                cancel,
                tab_closed: false,
            },
        );
        Ok(Claim { engine: self, id })
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
    /// (false when it isn't connected or the connector can't tell).
    pub fn in_transaction(&self, connection_id: &str, session_key: &str) -> bool {
        self.sessions.lock().get(&(connection_id.to_string(), session_key.to_string())).is_some_and(|l| l.session.transaction_state() == TxState::Open)
    }

    /// COMMIT or ROLLBACK the session's transaction. Returns whether one is
    /// still open afterwards. Nothing happens without a session.
    pub async fn end_transaction(&self, connection_id: &str, session_key: &str, commit: bool) -> Result<bool> {
        let _claim = self.claim(connection_id, session_key, CancellationToken::new(), "A query is running in this tab: wait for it or stop it first")?;
        let Some(session) = self.sessions.lock().get(&(connection_id.to_string(), session_key.to_string())).map(|l| l.session.clone()) else {
            return Ok(false);
        };
        let tx = session.kind().traits().transactions;
        let sql = match (tx, commit) {
            (Some(t), true) => t.commit,
            (Some(t), false) => t.rollback,
            (None, true) => "COMMIT",
            (None, false) => "ROLLBACK",
        };
        // SQL Server nests BEGIN TRAN: each COMMIT ends one level.
        for _ in 0..32 {
            session.execute(sql, ExecOptions::default()).await?.collect().await?;
            // Only a known open one: an unknown state would repeat COMMIT 32 times.
            if !commit || session.transaction_state() != TxState::Open {
                break;
            }
        }
        Ok(session.transaction_state() == TxState::Open)
    }

    /// Manual commit: open a transaction before a write when none is open.
    pub(crate) async fn begin_if_manual(
        &self,
        req: &RunRequest,
        kind: databrain_connector_core::ConnectorKind,
        stmt: &PlannedStatement,
        cancel: &CancellationToken,
    ) -> Result<()> {
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
        // Stop is honored while connecting or signing in.
        let session = tokio::select! {
            s = self.session_for(&req.connection_id, req.session_key(), req.origin) => s?,
            _ = cancel.cancelled() => return Err(databrain_connector_core::ConnectorError::cancelled().into()),
        };
        // Unknown: BEGIN anyway; "already in a transaction" from the server
        // is better than a write that commits on its own in manual mode.
        if session.transaction_state() != TxState::Open {
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
        self.with_tab(connection_id, &meta_tab(), f).await
    }

    pub(crate) async fn with_tab<T, F, Fut>(&self, connection_id: &str, tab: &str, f: F) -> Result<T>
    where
        F: Fn(Arc<dyn Session>) -> Fut,
        Fut: std::future::Future<Output = databrain_connector_core::Result<T>>,
    {
        let s = self.session(connection_id, tab).await?;
        match f(s.clone()).await {
            Ok(v) => Ok(v),
            Err(e) if e.kind == ErrorKind::Connection => {
                // Stale connection: reconnect once (dropping only this
                // session: another caller may have reconnected already).
                self.drop_session_if(connection_id, tab, &s);
                drop(s);
                let s = self.session(connection_id, tab).await?;
                Ok(f(s).await?)
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Session used by AI indexing: its own for server databases; local
    /// files (SQLite, DuckDB) share the metadata session (one handle per file).
    pub(crate) fn index_tab(&self, connection_id: &str) -> String {
        match self.workspace.get_connection(connection_id).map(|p| p.config.kind) {
            Ok(k) if !k.traits().local => SessionRole::Index.key(),
            _ => meta_tab(),
        }
    }

    /// Close indexing's session (after a run or a cancel). Dropping it ends
    /// the server session, which also stops a statement still running there.
    pub fn end_index_session(&self, connection_id: &str) {
        let tab = self.index_tab(connection_id);
        if SessionRole::parse(&tab) == SessionRole::Index {
            self.drop_session(connection_id, &tab);
        }
    }

    /// Close the database sessions of a run key (keeps its outputs).
    pub fn release_sessions(&self, tab: &str) {
        self.drop_sessions(|(_, t), _| t == tab);
    }
}

/// `m` minutes; a huge setting means "practically never", not an overflow.
fn minutes(m: u64) -> Duration {
    Duration::from_secs(m.saturating_mul(60))
}

/// One connection attempt per session key at a time. The entry is removed
/// when the last caller using it is done, so the map doesn't keep one per
/// tab or job step ever opened.
struct ConnectGate<'a> {
    map: &'a Mutex<HashMap<SessionKey, Arc<tokio::sync::Mutex<()>>>>,
    key: SessionKey,
    gate: Arc<tokio::sync::Mutex<()>>,
}

impl<'a> ConnectGate<'a> {
    fn enter(map: &'a Mutex<HashMap<SessionKey, Arc<tokio::sync::Mutex<()>>>>, key: &SessionKey) -> Self {
        let gate = map.lock().entry(key.clone()).or_default().clone();
        Self { map, key: key.clone(), gate }
    }
}

impl Drop for ConnectGate<'_> {
    fn drop(&mut self) {
        let mut m = self.map.lock();
        // Clones are taken under this lock: 2 = the map's and ours, nobody waiting.
        if Arc::strong_count(&self.gate) == 2 && m.get(&self.key).is_some_and(|g| Arc::ptr_eq(g, &self.gate)) {
            m.remove(&self.key);
        }
    }
}

/// A session key held busy (see [`QueryEngine::claim`]); dropping it frees
/// the key, however the holder ends.
pub(crate) struct Claim<'a> {
    engine: &'a QueryEngine,
    id: String,
}

impl Drop for Claim<'_> {
    fn drop(&mut self) {
        self.engine.jobs.lock().remove(&self.id);
    }
}

#[cfg(test)]
mod minutes_tests {
    use super::*;

    #[test]
    fn huge_idle_minutes_dont_overflow() {
        assert_eq!(minutes(2), Duration::from_secs(120));
        assert_eq!(minutes(u64::MAX), Duration::from_secs(u64::MAX));
    }
}

#[cfg(test)]
mod gate_tests {
    use super::*;

    #[tokio::test]
    async fn gates_are_removed_when_done() {
        let map = Mutex::new(HashMap::new());
        let key: SessionKey = ("c".into(), "t".into());
        {
            let a = ConnectGate::enter(&map, &key);
            let held = a.gate.lock().await;
            let b = ConnectGate::enter(&map, &key);
            assert!(Arc::ptr_eq(&a.gate, &b.gate), "one gate per key");
            drop(held);
            drop(a);
            assert_eq!(map.lock().len(), 1, "kept while another caller uses it");
            drop(b);
        }
        assert!(map.lock().is_empty());
    }
}
