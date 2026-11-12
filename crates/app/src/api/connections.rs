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

/// The user checked an SSH server's key fingerprint and trusts it.
pub fn trust_ssh_host_key(state: &AppState, host: &str, port: u16, fingerprint: &str) -> Result<usize> {
    state.engine.trust_ssh_host_key(host, port, fingerprint)
}
