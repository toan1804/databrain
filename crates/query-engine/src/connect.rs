//! Signing in and reaching a server: credentials per connection, SSH
//! tunnels, the connect timeout and the Test button.

use super::*;

impl QueryEngine {
    /// Use `interaction` for browser / device-code sign-in prompts.
    pub fn set_interaction(&self, interaction: Arc<dyn Interaction>) {
        *self.interaction.lock() = interaction;
        self.creds.lock().clear();
    }

    pub(crate) fn interaction(&self) -> Arc<dyn Interaction> {
        self.interaction.lock().clone()
    }

    /// Credential source for a saved connection (cached so tokens persist
    /// across sessions and tabs).
    pub fn credentials(&self, profile: &ConnectionProfile) -> Result<Arc<dyn CredentialSource>> {
        if let Some(c) = self.creds.lock().get(&profile.id) {
            return Ok(c.clone());
        }
        let connector = self.registry.get(profile.config.kind)?;
        let config = profile.config.normalized();
        let src = credential_source(
            &config.auth,
            &profile.id,
            self.secrets.clone(),
            self.interaction(),
            connector.auth_context(&config),
        )?;
        self.creds.lock().insert(profile.id.clone(), src.clone());
        Ok(src)
    }

    /// Sign in now (browser / device code / token exchange).
    pub async fn sign_in(&self, connection_id: &str) -> Result<AuthStatus> {
        let profile = self.workspace.get_connection(connection_id)?;
        Ok(self.credentials(&profile)?.sign_in().await?)
    }

    pub async fn sign_out(&self, connection_id: &str) -> Result<()> {
        let profile = self.workspace.get_connection(connection_id)?;
        self.credentials(&profile)?.sign_out().await?;
        self.disconnect(connection_id);
        Ok(())
    }

    pub fn auth_status(&self, connection_id: &str) -> Result<AuthStatus> {
        let profile = self.workspace.get_connection(connection_id)?;
        Ok(self.credentials(&profile)?.status())
    }

    /// Forget cached credentials after a connection is edited.
    pub fn forget_credentials(&self, connection_id: &str) {
        self.creds.lock().remove(connection_id);
    }

    /// Open the SSH tunnel for a config (if configured) and return the
    /// config rewritten to go through it.
    pub(crate) async fn tunnel(&self, cfg: &ConnectionConfig, owner_id: Option<&str>, secret_override: Option<&str>) -> Result<(ConnectionConfig, Option<Arc<Tunnel>>)> {
        let Some(ssh) = cfg.ssh.clone() else { return Ok((cfg.clone(), None)) };
        let stored = match owner_id {
            Some(id) => self.secrets.get(&SecretRef::slot(id, "ssh"))?.map(|s| s.expose_secret().to_string()),
            None => None,
        };
        let secret_text = secret_override.map(str::to_string).or(stored);
        let secret = match (&ssh.auth, secret_text) {
            (databrain_connector_core::SshAuth::Password, Some(p)) => SshSecret::Password(p),
            (databrain_connector_core::SshAuth::Key { .. }, Some(p)) => SshSecret::KeyPassphrase(p),
            _ => SshSecret::None,
        };
        let default_port = self.registry.get(cfg.kind)?.info().default_port.unwrap_or(0);
        let target_port = cfg.port_or(Some(default_port)).unwrap_or(default_port);
        let t = Tunnel::open(TunnelSpec {
            ssh_host: &ssh.host,
            ssh_port: ssh.port,
            user: &ssh.user,
            auth: &ssh.auth,
            secret,
            expected_fingerprint: ssh.host_key_fingerprint.as_deref(),
            target_host: cfg.host_or_default(),
            target_port,
        })
        .await
        .map_err(|e| EngineError::new("connection", e.to_string()))?;
        // Trust on first use: pin the server key.
        if ssh.host_key_fingerprint.is_none() {
            if let Some(id) = owner_id {
                if let Ok(mut p) = self.workspace.get_connection(id) {
                    if let Some(s) = p.config.ssh.as_mut() {
                        s.host_key_fingerprint = Some(t.fingerprint().to_string());
                        let _ = self.workspace.save_connection(p);
                    }
                }
            }
        }
        let mut rewritten = cfg.clone();
        rewritten.host = Some("127.0.0.1".into());
        rewritten.port = Some(t.local_port());
        Ok((rewritten, Some(Arc::new(t))))
    }

    // -------------------------------------------------------------- sessions

    /// Seconds a connection attempt may take before it is reported as failed
    /// (setting `connect_timeout_secs`, default 20). Browser/device sign-in
    /// is not limited (the user may take a while), only cancellable.
    pub fn connect_timeout(&self) -> Duration {
        let secs = self.workspace.get_setting("connect_timeout_secs").ok().flatten().and_then(|v| v.as_u64()).unwrap_or(20);
        Duration::from_secs(secs.clamp(3, 600))
    }

    /// DNS + TCP check of the first network hop, so an unreachable server
    /// fails within seconds with a clear reason instead of the driver's own
    /// (long, sometimes retried) timeout.
    pub(crate) async fn preflight(&self, cfg: &ConnectionConfig) -> Result<()> {
        let default_port = self.registry.get(cfg.kind)?.info().default_port;
        let Some((host, port)) = probe::target(cfg, default_port) else { return Ok(()) };
        probe::reach(&host, port, self.connect_timeout().min(Duration::from_secs(8))).await
    }

    /// Test a configuration without saving it. `secret` overrides the stored
    /// secret; if `None` and `connection_id` is given, the stored one is used.
    /// `test_id` lets [`QueryEngine::cancel_test`] stop it; it also gives up
    /// after [`QueryEngine::connect_timeout`] (unless sign-in is interactive).
    pub async fn test_connection(
        &self,
        cfg: &ConnectionConfig,
        secret: Option<String>,
        connection_id: Option<&str>,
        ssh_secret: Option<String>,
        test_id: Option<&str>,
    ) -> Result<TestResult> {
        let token = CancellationToken::new();
        if let Some(id) = test_id {
            if let Some(old) = self.tests.lock().insert(id.to_string(), token.clone()) {
                old.cancel();
            }
        }
        let limit = (!cfg.auth.is_interactive()).then(|| self.connect_timeout());
        // One-time setup (a driver download) first, outside the time limit.
        let prepared = async {
            let connector = self.registry.get(cfg.kind)?;
            connector.prepare(&cfg.normalized()).await.map_err(EngineError::from)
        };
        let prepared = tokio::select! {
            r = prepared => r,
            _ = token.cancelled() => Err(EngineError::new("cancelled", "Test cancelled")),
        };
        if let Err(e) = prepared {
            if let Some(id) = test_id.filter(|_| !token.is_cancelled()) {
                self.tests.lock().remove(id);
            }
            return Err(e);
        }
        let work = self.test_connection_inner(cfg, secret, connection_id, ssh_secret);
        let r = tokio::select! {
            r = work => r,
            _ = token.cancelled() => Err(EngineError::new("cancelled", "Test cancelled")),
            _ = async { match limit { Some(d) => tokio::time::sleep(d).await, None => std::future::pending().await } } => Err(EngineError::new(
                "connection",
                format!("No connection after {} s: the server did not finish the handshake/sign-in (check host, port, SSL and firewall)", limit.unwrap_or_default().as_secs()),
            )),
        };
        // Not cancelled = the entry is still ours (a newer test with the same
        // id cancels this one, and cancel_test removes it).
        if let Some(id) = test_id.filter(|_| !token.is_cancelled()) {
            self.tests.lock().remove(id);
        }
        r
    }

    /// Stop a running [`QueryEngine::test_connection`]; dropping it closes
    /// the half-open socket/tunnel. `false` when it already finished.
    pub fn cancel_test(&self, test_id: &str) -> bool {
        match self.tests.lock().remove(test_id) {
            Some(t) => {
                t.cancel();
                true
            }
            None => false,
        }
    }

    pub(crate) async fn test_connection_inner(
        &self,
        cfg: &ConnectionConfig,
        secret: Option<String>,
        connection_id: Option<&str>,
        ssh_secret: Option<String>,
    ) -> Result<TestResult> {
        let cfg = &cfg.normalized();
        let connector = self.registry.get(cfg.kind)?;
        let creds: Arc<dyn CredentialSource> = match (secret, connection_id) {
            (Some(s), _) => Arc::new(InlineCredentialSource::new(cfg.auth.clone(), Some(SecretString::from(s)))),
            (None, Some(id)) => credential_source(
                &cfg.auth,
                id,
                self.secrets.clone(),
                self.interaction(),
                connector.auth_context(cfg),
            )?,
            (None, None) if cfg.auth.is_interactive() => credential_source(
                &cfg.auth,
                "__test__",
                Arc::new(databrain_auth::MemoryStore::default()),
                self.interaction(),
                connector.auth_context(cfg),
            )?,
            (None, None) => Arc::new(InlineCredentialSource::new(cfg.auth.clone(), None)),
        };
        let start = Instant::now();
        self.preflight(cfg).await?;
        let (cfg, _tunnel) = self.tunnel(cfg, connection_id, ssh_secret.as_deref()).await?;
        let session = connector.connect(&cfg, creds).await?;
        let server_version = session.server_version().await?;
        Ok(TestResult {
            server_version,
            latency_ms: start.elapsed().as_millis() as u64,
        })
    }
}
