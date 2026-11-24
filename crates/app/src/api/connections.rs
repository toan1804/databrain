//! Connection types and saved connections: list, save (secrets to the
//! keychain), test, connect, delete.

use super::*;

// ------------------------------------------------------------ connectors & connections

pub fn list_connectors(state: &AppState) -> Vec<ConnectorInfo> {
    state.engine.registry().infos()
}

#[derive(Debug, Serialize, ts_rs::TS)]
#[ts(export)]
pub struct ConnectionView {
    #[serde(flatten)]
    pub profile: ConnectionProfile,
    pub connected: bool,
}

pub fn list_connections(state: &AppState) -> Result<Vec<ConnectionView>> {
    let connected = state.engine.connected_ids();
    Ok(state.workspace.list_connections()?.into_iter().map(|p| ConnectionView { connected: connected.contains(&p.id), profile: p }).collect())
}

/// Secondary secret slots a connection may use.
pub const SECRET_SLOTS: &[&str] = &["ssh", "client_secret", "passphrase"];

#[derive(Debug, Deserialize)]
pub struct SaveConnectionArgs {
    pub profile: ConnectionProfile,
    /// New password/token/key. `None` keeps the current secret.
    #[serde(default)]
    pub secret: Option<String>,
    /// Remove the stored secret.
    #[serde(default)]
    pub clear_secret: bool,
    /// Secondary secrets by slot (`ssh`, `client_secret`, `passphrase`).
    /// Empty string removes the slot; absent keeps it.
    #[serde(default)]
    pub extra_secrets: std::collections::BTreeMap<String, String>,
}

pub fn save_connection(state: &AppState, args: SaveConnectionArgs) -> Result<ConnectionProfile> {
    validate_config(state, &args.profile.config)?;
    store_connection(state, args)
}

/// [`save_connection`] without checking the settings are complete (imports:
/// a connection missing its user is saved, to be finished in its form).
pub(crate) fn store_connection(state: &AppState, mut args: SaveConnectionArgs) -> Result<ConnectionProfile> {
    // A password in a MongoDB/Neo4j connection string goes to the keychain,
    // not into workspace.db (one typed in its field, or already stored, wins).
    if let Some(pw) = crate::conn_io::take_uri_password(&mut args.profile.config) {
        let stored = !args.profile.id.is_empty() && !args.clear_secret && state.workspace.get_connection(&args.profile.id).is_ok_and(|c| c.has_secret);
        if args.secret.is_none() && !stored {
            args.secret = Some(pw);
            args.clear_secret = false;
        }
    }
    let mut profile = args.profile;
    let before = if profile.id.is_empty() { None } else { state.workspace.get_connection(&profile.id).ok() };
    // Saved with another server, user or encryption setting and no new
    // secret: the old secrets are not kept for it (Connect would send them
    // there). The user types them again.
    let moved = before.as_ref().is_some_and(|b| b.config.secret_destination() != profile.config.secret_destination());
    if moved && args.secret.is_none() {
        args.clear_secret = true;
        for slot in SECRET_SLOTS {
            if !args.extra_secrets.contains_key(*slot) {
                args.extra_secrets.insert((*slot).to_string(), String::new());
            }
        }
    }
    let new_secrets = args.secret.is_some() || args.clear_secret || !args.extra_secrets.is_empty();
    if args.secret.is_some() {
        profile.has_secret = true;
    } else if args.clear_secret {
        profile.has_secret = false;
    } else if !profile.id.is_empty() {
        // Keep the flag from the stored record; the UI may not know it.
        if let Ok(existing) = state.workspace.get_connection(&profile.id) {
            profile.has_secret = existing.has_secret;
        }
    }
    let saved = state.workspace.save_connection(profile)?;
    let r = SecretRef::for_connection(&saved.id);
    if let Some(secret) = args.secret {
        if let Err(e) = state.secrets.set(&r, &SecretString::from(secret)) {
            let mut revert = saved.clone();
            revert.has_secret = false;
            let _ = state.workspace.save_connection(revert);
            return Err(e.into());
        }
    } else if args.clear_secret {
        state.secrets.delete(&r)?;
    }
    for (slot, value) in &args.extra_secrets {
        if !SECRET_SLOTS.contains(&slot.as_str()) {
            continue;
        }
        let sr = SecretRef::slot(&saved.id, slot);
        if value.is_empty() {
            state.secrets.delete(&sr)?;
        } else {
            state.secrets.set(&sr, &SecretString::from(value.clone()))?;
        }
    }
    // Apply new settings (and auth method) on the next query. A new name,
    // color or folder changes nothing there: running queries go on.
    let same = before.is_some_and(|b| b.config == saved.config && b.ai_policy == saved.ai_policy);
    if !same || new_secrets {
        state.engine.disconnect(&saved.id);
        state.engine.forget_credentials(&saved.id);
    }
    Ok(saved)
}

pub(crate) fn validate_config(state: &AppState, cfg: &ConnectionConfig) -> Result<()> {
    use databrain_auth::AuthMethod;
    // What the connector itself requires (a file, an account, a project…).
    state.engine.registry().get(cfg.kind)?.validate(cfg).map_err(invalid)?;
    match &cfg.auth {
        AuthMethod::Password { user } | AuthMethod::KeyPair { user } | AuthMethod::ExternalBrowser { user } if user.trim().is_empty() => {
            return Err(invalid("User name is required"));
        }
        _ => {}
    }
    if let Some(ssh) = &cfg.ssh {
        if cfg.kind.is_cloud() || cfg.kind.traits().local {
            return Err(invalid("SSH tunnels are only available for server databases"));
        }
        if ssh.host.trim().is_empty() || ssh.user.trim().is_empty() {
            return Err(invalid("SSH host and user are required"));
        }
    }
    Ok(())
}

pub fn move_to_folder(state: &AppState, kind: FolderKind, item_id: &str, folder_id: Option<String>) -> Result<()> {
    Ok(state.workspace.move_to_folder(kind, item_id, folder_id.as_deref())?)
}

/// Delete a connection: closes its sessions, removes it, its AI knowledge
/// and every secret it owns (password, SSH, client secret, key passphrase,
/// cached OAuth tokens). Saved queries, history and notebooks are kept.
pub fn delete_connection(state: &AppState, id: &str) -> Result<()> {
    state.engine.disconnect(id);
    state.engine.forget_credentials(id);
    state.workspace.delete_connection(id)?;
    // The connection is gone either way; a secret that cannot be removed
    // now is unreachable, so report but don't fail.
    let mut errors = Vec::new();
    let refs = std::iter::once(SecretRef::for_connection(id)).chain(SECRET_SLOTS.iter().chain(["oauth"].iter()).map(|s| SecretRef::slot(id, s)));
    for r in refs {
        if let Err(e) = state.secrets.delete(&r) {
            errors.push(e.to_string());
        }
    }
    if let Some(e) = errors.first() {
        return Err(EngineError::new("partial", format!("connection deleted, but a saved secret could not be removed: {e}")));
    }
    Ok(())
}

pub async fn test_connection(
    state: &AppState,
    config: ConnectionConfig,
    secret: Option<String>,
    connection_id: Option<String>,
    ssh_secret: Option<String>,
    test_id: Option<String>,
) -> Result<TestResult> {
    validate_config(state, &config)?;
    // The saved secrets go only where they were saved for: another server,
    // user or encryption setting typed in the form doesn't get them.
    let mut connection_id = connection_id;
    if let Some(saved) = connection_id.as_deref().and_then(|id| state.workspace.get_connection(id).ok())
        && saved.config.secret_destination() != config.secret_destination()
    {
        if saved.has_secret && secret.is_none() {
            return Err(EngineError::new(
                "invalid",
                "The server, user or encryption settings changed: type the password (or key) to test against them. The saved one is only sent where it was saved for.",
            ));
        }
        connection_id = None;
    }
    state.engine.test_connection(&config, secret, connection_id.as_deref(), ssh_secret, test_id.as_deref()).await
}

/// Stop a running connection test (dialog "Cancel").
pub fn cancel_test_connection(state: &AppState, test_id: &str) -> bool {
    state.engine.cancel_test(test_id)
}

pub async fn connect(state: &AppState, id: &str) -> Result<String> {
    state.engine.connect(id).await
}

pub fn disconnect(state: &AppState, id: &str) {
    state.engine.disconnect(id);
}

/// Sign in again on the next call, so rights granted since the login
/// apply (see [`QueryEngine::reconnect`](databrain_query_engine::QueryEngine::reconnect)).
pub fn reconnect(state: &AppState, id: &str) -> usize {
    state.engine.reconnect(id)
}

/// Change the connection's password on the server, then the one saved for
/// it (when one is saved): the next sign-in uses it. `current` must sign
/// in (checked first: PostgreSQL and older MySQL don't check it themselves)
/// and, with a saved password, be it. Idle sessions sign in again with the
/// new one; running queries, jobs and open transactions go on. Neither
/// password is logged or kept anywhere else.
pub async fn change_password(state: &AppState, id: &str, current: String, new: String) -> Result<PasswordChanged> {
    if new.is_empty() {
        return Err(invalid("Type the new password"));
    }
    if new == current {
        return Err(invalid("The new password is the current one"));
    }
    let profile = state.workspace.get_connection(id)?;
    let saved = SecretRef::for_connection(id);
    if profile.has_secret {
        let stored = state.secrets.get(&saved)?;
        if stored.as_ref().map(secrecy::ExposeSecret::expose_secret) != Some(current.as_str()) {
            return Err(invalid("The current password isn't the one saved for this connection"));
        }
    }
    // Said before the sign-in below (the engine refuses these too).
    let traits = profile.config.kind.traits();
    if !traits.change_password {
        return Err(EngineError::new("unsupported", format!("{} passwords can't be changed from DataBrain", traits.name)));
    }
    if !matches!(profile.config.auth, databrain_auth::AuthMethod::Password { .. }) {
        return Err(invalid("Only a connection that signs in with a password can change it here"));
    }
    if profile.config.read_only {
        return Err(EngineError::new("read_only", format!("\"{}\" is read-only: change its password with a connection that isn't", profile.name)));
    }
    // A refused sign-in is the wrong password; anything else (unreachable,
    // a timeout, an SSH host key to trust) keeps its own error.
    match state.engine.test_connection(&profile.config, Some(current.clone()), Some(id), None, None).await {
        Err(e) if password_refused(&e) => return Err(EngineError::new("invalid", format!("The current password doesn't sign in: {}", e.message))),
        Err(e) => return Err(e),
        Ok(_) => {}
    }
    state.engine.change_password(id, &current, &new).await?;
    // Saved before anything signs in again, so nothing caches the old one.
    let saved_ok = if profile.has_secret { state.secrets.set(&saved, &SecretString::from(new)) } else { Ok(()) };
    state.engine.forget_credentials(id);
    state.engine.reconnect(id);
    if let Err(e) = saved_ok {
        return Err(EngineError::new(
            "partial",
            format!("The password was changed on the server, but the new one couldn't be saved ({e}): type it in the connection's settings."),
        ));
    }
    let others = state
        .workspace
        .list_connections()
        .unwrap_or_default()
        .into_iter()
        .filter(|c| same_login(&profile, c))
        .map(|c| OtherLogin { id: c.id, name: c.name })
        .collect();
    Ok(PasswordChanged { others })
}

/// Whether a sign-in failed on its password (not an unreachable server):
/// an auth error, or the database's own code or words for a wrong password
/// (PostgreSQL 28P01, MySQL 1045, SQL Server 18456, Oracle ORA-01017; the
/// English messages too, as a translated server sends no code we read),
/// which these connectors report as connection errors.
pub fn password_refused(e: &EngineError) -> bool {
    // Drivers add to the code: MySQL "1045 (28000)", SQL Server "18456 (line 1)".
    let code = e.code.as_deref().and_then(|c| c.split_whitespace().next()).unwrap_or("");
    let m = e.message.to_lowercase();
    e.kind == "auth"
        // Not PostgreSQL's 28000: it is also "no pg_hba.conf entry".
        || matches!(code, "28P01" | "1045" | "18456" | "1017" | "ORA-01017")
        || m.contains("password authentication failed")
        || m.contains("access denied for user")
        || m.contains("login failed for user")
        || m.contains("ora-01017")
}

/// After a password change: the other saved connections that sign in as
/// the same user ([`same_login`]), whose saved password may want the new one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ts_rs::TS)]
#[ts(export)]
pub struct PasswordChanged {
    pub others: Vec<OtherLogin>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, ts_rs::TS)]
#[ts(export)]
pub struct OtherLogin {
    pub id: String,
    pub name: String,
}

/// Whether `other` is another saved connection signing in as `of`'s user:
/// the same type, server (host and port as the connector reads them, the
/// same SSH tunnel) and user, with a password saved; for SQL Server the
/// same instance, for Oracle the same service (or database) or connect
/// string, as a pluggable database has its own users. Users compare
/// case-insensitively where the database does (SQL Server, Oracle).
pub fn same_login(of: &ConnectionProfile, other: &ConnectionProfile) -> bool {
    use databrain_auth::AuthMethod;
    use databrain_connector_core::{ConnectionConfig, ConnectorKind};
    let (AuthMethod::Password { user: a }, AuthMethod::Password { user: b }) = (&of.config.auth, &other.config.auth) else { return false };
    let (x, y) = (&of.config, &other.config);
    // A port left empty is the database's default (Redshift's is its own).
    let default_port = match x.kind {
        ConnectorKind::Postgres => Some(5432),
        ConnectorKind::Mysql => Some(3306),
        ConnectorKind::Mssql => Some(1433),
        ConnectorKind::Oracle => Some(1521),
        _ => None,
    };
    let server = |c: &ConnectionConfig| (c.host_or_default().trim().to_lowercase(), c.port_or(default_port));
    let tunnel = |c: &ConnectionConfig| c.ssh.as_ref().map(|s| (s.host.trim().to_lowercase(), s.port, s.user.trim().to_string()));
    let opt = |c: &ConnectionConfig, k: &str| c.opt(k).map(str::to_lowercase);
    let oracle_db = |c: &ConnectionConfig| {
        (opt(c, "connect_string"), opt(c, "service_name").or(c.database.as_deref().map(str::trim).filter(|d| !d.is_empty()).map(str::to_lowercase)))
    };
    let users = match x.kind {
        ConnectorKind::Mssql | ConnectorKind::Oracle => a.trim().eq_ignore_ascii_case(b.trim()),
        _ => a.trim() == b.trim(),
    };
    other.id != of.id
        && other.has_secret
        && x.kind == y.kind
        && x.is_redshift() == y.is_redshift()
        && users
        && server(x) == server(y)
        && tunnel(x) == tunnel(y)
        && (x.kind != ConnectorKind::Mssql || opt(x, "instance") == opt(y, "instance"))
        && (x.kind != ConnectorKind::Oracle || oracle_db(x) == oracle_db(y))
}

/// What updating one other connection's saved password did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ts_rs::TS)]
#[ts(export)]
pub struct PasswordUpdate {
    pub id: String,
    pub name: String,
    /// Why it wasn't updated (`None`: saved).
    pub error: Option<String>,
}

/// Save `new` as the password of the connections `ids` after `source`'s
/// was changed: only those signing in as the same user ([`same_login`]),
/// and each only once the new password signs in there (a connection that
/// turns out to be another login, or can't be reached, keeps its own).
pub async fn update_saved_passwords(state: &AppState, source: &str, ids: &[String], new: String) -> Result<Vec<PasswordUpdate>> {
    let of = state.workspace.get_connection(source)?;
    let all = state.workspace.list_connections()?;
    // Checked together: one unreachable server doesn't hold up the rest.
    let checks = ids.iter().filter_map(|id| all.iter().find(|c| &c.id == id)).map(|c| {
        let new = new.clone();
        let of = &of;
        async move {
            let (id, name) = (c.id.clone(), c.name.clone());
            if !same_login(of, c) {
                return PasswordUpdate { id, name, error: Some("It doesn't sign in as the same user".into()) };
            }
            let error = match state.engine.test_connection(&c.config, Some(new.clone()), Some(&id), None, None).await {
                Err(e) if password_refused(&e) => Some(format!("The new password doesn't sign in there: {}", e.message)),
                Err(e) => Some(format!("Couldn't sign in to check the new password: {}", e.message)),
                Ok(_) => match state.secrets.set(&SecretRef::for_connection(&id), &SecretString::from(new)) {
                    Ok(()) => {
                        state.engine.forget_credentials(&id);
                        state.engine.reconnect(&id);
                        None
                    }
                    Err(e) => Some(format!("The new password couldn't be saved: {e}")),
                },
            };
            PasswordUpdate { id, name, error }
        }
    });
    Ok(futures::future::join_all(checks).await)
}

/// The user checked an SSH server's key fingerprint and trusts it.
pub fn trust_ssh_host_key(state: &AppState, host: &str, port: u16, fingerprint: &str) -> Result<usize> {
    state.engine.trust_ssh_host_key(host, port, fingerprint)
}
