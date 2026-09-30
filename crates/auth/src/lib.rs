//! Shared authentication for database connections and AI providers.
//!
//! Connectors never hold raw secrets. They receive an [`Arc<dyn CredentialSource>`]
//! and ask it for a [`Credential`] right before connecting. Sources cover
//! passwords, API tokens / PATs, RSA key pairs, OAuth (browser + PKCE, device
//! code, client credentials) with refresh, Google service accounts / ADC and
//! existing cloud CLI logins.

pub mod cloud;
pub mod jwt;
pub mod loopback;
pub mod oauth;

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
pub use oauth::{ClientAuthStyle, OAuthConfig, TokenSet};
pub use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
pub use tokio_util::sync::CancellationToken;

/// Keychain service name used for every DataBrain secret.
pub const KEYCHAIN_SERVICE: &str = "dev.databrain.app";

pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum AuthError {
    #[error("secret not found for '{0}'")]
    NotFound(String),
    #[error("secret store error: {0}")]
    Store(String),
    #[error("sign-in required")]
    ReauthRequired,
    #[error("sign-in cancelled")]
    Cancelled,
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    Flow(String),
    #[error("network error: {0}")]
    Network(String),
}

// ------------------------------------------------------------------ secrets

/// Opaque reference to a secret stored in a [`SecretStore`] (the keychain "account").
#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SecretRef(pub String);

impl SecretRef {
    /// Primary secret (password, token, private key, service account JSON).
    pub fn for_connection(connection_id: &str) -> Self {
        Self(format!("connection:{connection_id}"))
    }
    /// Secondary secret of a connection: `ssh`, `client_secret`, `passphrase`, `oauth`.
    pub fn slot(owner_id: &str, slot: &str) -> Self {
        Self(format!("connection:{owner_id}:{slot}"))
    }
    pub fn for_ai_provider(provider_id: &str) -> Self {
        Self(format!("ai:{provider_id}"))
    }
}

impl fmt::Debug for SecretRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SecretRef({})", self.0)
    }
}

pub trait SecretStore: Send + Sync {
    fn get(&self, r: &SecretRef) -> Result<Option<SecretString>, AuthError>;
    fn set(&self, r: &SecretRef, value: &SecretString) -> Result<(), AuthError>;
    fn delete(&self, r: &SecretRef) -> Result<(), AuthError>;
}

/// OS keychain (macOS Keychain, Windows Credential Manager, Secret Service).
#[derive(Debug, Default, Clone)]
pub struct KeychainStore;

impl KeychainStore {
    fn entry(r: &SecretRef) -> Result<keyring::Entry, AuthError> {
        keyring::Entry::new(KEYCHAIN_SERVICE, &r.0).map_err(|e| AuthError::Store(e.to_string()))
    }
}

impl SecretStore for KeychainStore {
    fn get(&self, r: &SecretRef) -> Result<Option<SecretString>, AuthError> {
        match Self::entry(r)?.get_password() {
            Ok(v) => Ok(Some(SecretString::from(v))),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(AuthError::Store(e.to_string())),
        }
    }
    fn set(&self, r: &SecretRef, value: &SecretString) -> Result<(), AuthError> {
        Self::entry(r)?
            .set_password(value.expose_secret())
            .map_err(|e| AuthError::Store(e.to_string()))
    }
    fn delete(&self, r: &SecretRef) -> Result<(), AuthError> {
        match Self::entry(r)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(AuthError::Store(e.to_string())),
        }
    }
}

/// In-memory store for tests and ephemeral sessions.
#[derive(Default)]
pub struct MemoryStore {
    inner: Mutex<HashMap<SecretRef, SecretString>>,
}

impl SecretStore for MemoryStore {
    fn get(&self, r: &SecretRef) -> Result<Option<SecretString>, AuthError> {
        Ok(self.inner.lock().expect("poisoned").get(r).cloned())
    }
    fn set(&self, r: &SecretRef, value: &SecretString) -> Result<(), AuthError> {
        self.inner
            .lock()
            .expect("poisoned")
            .insert(r.clone(), value.clone());
        Ok(())
    }
    fn delete(&self, r: &SecretRef) -> Result<(), AuthError> {
        self.inner.lock().expect("poisoned").remove(r);
        Ok(())
    }
}

// ------------------------------------------------------------------ methods

/// OAuth settings a user may override per connection.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    /// Space-separated scopes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scopes: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redirect_port: Option<u16>,
    /// Entra ID tenant (Azure SQL).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tenant: Option<String>,
    /// Optional login hint / Snowflake user.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// Custom issuer endpoints (Snowflake External OAuth via Okta/Entra...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authorize_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_url: Option<String>,
}

/// How a connection authenticates. Serialized into the connection config
/// (never contains secrets).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "method", rename_all = "snake_case")]
pub enum AuthMethod {
    None,
    Password {
        user: String,
    },
    /// API key / PAT / programmatic access token.
    ApiToken {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        user: Option<String>,
    },
    /// RSA key pair (Snowflake). The PEM is the primary secret.
    KeyPair {
        user: String,
    },
    OauthBrowser(OAuthParams),
    DeviceCode(OAuthParams),
    /// Service principal / M2M; the client secret is the primary secret.
    ClientCredentials(OAuthParams),
    /// Snowflake SAML SSO in the browser.
    ExternalBrowser {
        user: String,
    },
    /// Reuse `gcloud` ADC, `~/.databrickscfg` / Databricks CLI, or `az login`.
    CloudCli {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        profile: Option<String>,
    },
    /// Google service account JSON key (primary secret).
    ServiceAccount,
}

impl AuthMethod {
    pub fn kind(&self) -> AuthMethodKind {
        match self {
            AuthMethod::None => AuthMethodKind::None,
            AuthMethod::Password { .. } => AuthMethodKind::Password,
            AuthMethod::ApiToken { .. } => AuthMethodKind::ApiToken,
            AuthMethod::KeyPair { .. } => AuthMethodKind::KeyPair,
            AuthMethod::OauthBrowser(_) => AuthMethodKind::OauthBrowser,
            AuthMethod::DeviceCode(_) => AuthMethodKind::DeviceCode,
            AuthMethod::ClientCredentials(_) => AuthMethodKind::ClientCredentials,
            AuthMethod::ExternalBrowser { .. } => AuthMethodKind::ExternalBrowser,
            AuthMethod::CloudCli { .. } => AuthMethodKind::CloudCli,
            AuthMethod::ServiceAccount => AuthMethodKind::ServiceAccount,
        }
    }

    /// Methods that sign in through the browser or a device code.
    pub fn is_interactive(&self) -> bool {
        matches!(
            self,
            AuthMethod::OauthBrowser(_)
                | AuthMethod::DeviceCode(_)
                | AuthMethod::ExternalBrowser { .. }
        )
    }

    pub fn user(&self) -> Option<&str> {
        match self {
            AuthMethod::Password { user }
            | AuthMethod::KeyPair { user }
            | AuthMethod::ExternalBrowser { user } => Some(user),
            AuthMethod::ApiToken { user } => user.as_deref(),
            AuthMethod::OauthBrowser(p) | AuthMethod::DeviceCode(p) => p.user.as_deref(),
            _ => None,
        }
    }

    pub fn oauth_params(&self) -> Option<&OAuthParams> {
        match self {
            AuthMethod::OauthBrowser(p)
            | AuthMethod::DeviceCode(p)
            | AuthMethod::ClientCredentials(p) => Some(p),
            _ => None,
        }
    }
}

/// Kinds of auth a connector can accept; drives the connection form.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthMethodKind {
    None,
    Password,
    ApiToken,
    KeyPair,
    OauthBrowser,
    DeviceCode,
    ClientCredentials,
    ExternalBrowser,
    CloudCli,
    ServiceAccount,
}

// ------------------------------------------------------------------ interaction

#[derive(Debug, Clone, Serialize)]
pub struct DeviceCodePrompt {
    pub provider: String,
    pub user_code: String,
    pub verification_uri: String,
    pub verification_uri_complete: Option<String>,
    pub message: Option<String>,
}

/// UI hooks for interactive sign-in.
#[async_trait]
pub trait Interaction: Send + Sync {
    /// Open a URL in the system browser.
    async fn open_url(&self, url: &str) -> Result<(), AuthError>;
    /// Show a device code to the user.
    async fn device_code(&self, prompt: &DeviceCodePrompt);
    /// Interactive step finished (dismiss prompts).
    async fn finished(&self) {}
    /// Cancelled when the user aborts the sign-in.
    fn cancel_token(&self) -> CancellationToken {
        CancellationToken::new()
    }
    /// `false` for background contexts (e.g. the MCP server): interactive
    /// flows fail with [`AuthError::ReauthRequired`] instead.
    fn interactive(&self) -> bool {
        true
    }
}

/// Interaction that never prompts.
pub struct NonInteractive;

#[async_trait]
impl Interaction for NonInteractive {
    async fn open_url(&self, _url: &str) -> Result<(), AuthError> {
        Err(AuthError::ReauthRequired)
    }
    async fn device_code(&self, _prompt: &DeviceCodePrompt) {}
    fn interactive(&self) -> bool {
        false
    }
}

// ------------------------------------------------------------------ credentials

/// A usable credential, materialized just before connecting.
pub enum Credential {
    None,
    Password {
        user: String,
        password: SecretString,
    },
    Bearer {
        token: SecretString,
        user: Option<String>,
    },
    KeyPair {
        user: String,
        private_key_pem: SecretString,
        passphrase: Option<SecretString>,
    },
    /// The connector runs the provider-specific browser SSO itself.
    ExternalBrowser {
        user: String,
        interaction: Arc<dyn Interaction>,
    },
}

impl fmt::Debug for Credential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Credential::None => f.write_str("Credential::None"),
            Credential::Password { user, .. } => write!(
                f,
                "Credential::Password {{ user: {user:?}, password: *** }}"
            ),
            Credential::Bearer { user, .. } => {
                write!(f, "Credential::Bearer {{ user: {user:?}, token: *** }}")
            }
            Credential::KeyPair { user, .. } => {
                write!(f, "Credential::KeyPair {{ user: {user:?}, key: *** }}")
            }
            Credential::ExternalBrowser { user, .. } => {
                write!(f, "Credential::ExternalBrowser {{ user: {user:?} }}")
            }
        }
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct AuthStatus {
    pub signed_in: bool,
    pub identity: Option<String>,
    /// Unix seconds.
    pub expires_at: Option<u64>,
}

#[async_trait]
pub trait CredentialSource: Send + Sync {
    /// Returns a valid credential, refreshing silently if needed. May start an
    /// interactive sign-in if the method requires it and none is cached.
    async fn get(&self) -> Result<Credential, AuthError>;
    /// Called by connectors on auth failures; the next `get` refreshes.
    async fn invalidate(&self) {}
    /// Run the interactive sign-in now (browser / device code).
    async fn sign_in(&self) -> Result<AuthStatus, AuthError> {
        self.get().await?;
        Ok(self.status())
    }
    /// Forget cached tokens (and revoke where supported).
    async fn sign_out(&self) -> Result<(), AuthError> {
        Ok(())
    }
    fn status(&self) -> AuthStatus {
        AuthStatus::default()
    }
    fn identity(&self) -> Option<String> {
        self.status().identity
    }
}

fn read_secret(store: &dyn SecretStore, r: &SecretRef) -> Result<Option<SecretString>, AuthError> {
    Ok(store.get(r)?.filter(|s| !s.expose_secret().is_empty()))
}

/// Password / API token / key pair backed by the secret store.
pub struct StoredCredentialSource {
    method: AuthMethod,
    secret_ref: SecretRef,
    passphrase_ref: Option<SecretRef>,
    store: Arc<dyn SecretStore>,
}

impl StoredCredentialSource {
    pub fn new(method: AuthMethod, secret_ref: SecretRef, store: Arc<dyn SecretStore>) -> Self {
        Self {
            method,
            secret_ref,
            passphrase_ref: None,
            store,
        }
    }
    pub fn with_passphrase(mut self, r: SecretRef) -> Self {
        self.passphrase_ref = Some(r);
        self
    }
}

#[async_trait]
impl CredentialSource for StoredCredentialSource {
    async fn get(&self) -> Result<Credential, AuthError> {
        let secret = || read_secret(self.store.as_ref(), &self.secret_ref);
        let required = || -> Result<SecretString, AuthError> {
            secret()?.ok_or_else(|| AuthError::NotFound(self.secret_ref.0.clone()))
        };
        match &self.method {
            AuthMethod::None => Ok(Credential::None),
            AuthMethod::Password { user } => Ok(Credential::Password {
                user: user.clone(),
                // Missing password is allowed (e.g. trust auth).
                password: secret()?.unwrap_or_else(|| SecretString::from(String::new())),
            }),
            AuthMethod::ApiToken { user } => Ok(Credential::Bearer {
                token: required()?,
                user: user.clone(),
            }),
            AuthMethod::KeyPair { user } => Ok(Credential::KeyPair {
                user: user.clone(),
                private_key_pem: required()?,
                passphrase: match &self.passphrase_ref {
                    Some(r) => read_secret(self.store.as_ref(), r)?,
                    None => None,
                },
            }),
            other => Err(AuthError::Invalid(format!(
                "{:?} is not a stored-secret method",
                other.kind()
            ))),
        }
    }
    fn status(&self) -> AuthStatus {
        let has = matches!(self.method, AuthMethod::None)
            || read_secret(self.store.as_ref(), &self.secret_ref)
                .ok()
                .flatten()
                .is_some();
        AuthStatus {
            signed_in: has,
            identity: self.method.user().map(str::to_string),
            expires_at: None,
        }
    }
}

/// Credential held in memory (used for "Test connection" before saving).
pub struct InlineCredentialSource {
    method: AuthMethod,
    secret: Option<SecretString>,
}

impl InlineCredentialSource {
    pub fn new(method: AuthMethod, secret: Option<SecretString>) -> Self {
        Self { method, secret }
    }
}

#[async_trait]
impl CredentialSource for InlineCredentialSource {
    async fn get(&self) -> Result<Credential, AuthError> {
        let secret = || {
            self.secret
                .clone()
                .unwrap_or_else(|| SecretString::from(String::new()))
        };
        Ok(match &self.method {
            AuthMethod::None => Credential::None,
            AuthMethod::Password { user } => Credential::Password {
                user: user.clone(),
                password: secret(),
            },
            AuthMethod::ApiToken { user } => Credential::Bearer {
                token: secret(),
                user: user.clone(),
            },
            AuthMethod::KeyPair { user } => Credential::KeyPair {
                user: user.clone(),
                private_key_pem: secret(),
                passphrase: None,
            },
            other => {
                return Err(AuthError::Invalid(format!(
                    "{:?} cannot be tested before the connection is saved",
                    other.kind()
                )));
            }
        })
    }
    fn status(&self) -> AuthStatus {
        AuthStatus {
            signed_in: true,
            identity: self.method.user().map(str::to_string),
            expires_at: None,
        }
    }
}

/// How a cached/refreshable token is (re)obtained.
#[derive(Clone)]
enum TokenMode {
    Browser(OAuthConfig),
    Device(OAuthConfig),
    ClientCredentials(OAuthConfig),
    GoogleServiceAccount { scopes: Vec<String> },
    Cli(CliKind),
}

/// Existing CLI login to reuse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliKind {
    GoogleAdc { scopes: Vec<String> },
    Databricks { host: String },
    Azure { resource: String },
}

/// Token-based source with in-memory caching, persisted refresh tokens,
/// single-flight refresh and interactive fallback.
pub struct TokenSource {
    mode: TokenMode,
    user: Option<String>,
    store: Arc<dyn SecretStore>,
    /// Keychain slot for the token cache (refresh token).
    cache_ref: SecretRef,
    /// Primary secret (service account JSON / client secret).
    primary_ref: SecretRef,
    cli_profile: Option<String>,
    interaction: Arc<dyn Interaction>,
    state: Mutex<Option<TokenSet>>,
    flight: tokio::sync::Mutex<()>,
}

impl TokenSource {
    fn cached(&self) -> Option<TokenSet> {
        self.state.lock().expect("poisoned").clone()
    }

    fn store_tokens(&self, t: &TokenSet) {
        *self.state.lock().expect("poisoned") = Some(t.clone());
        if t.refresh_token.is_some() {
            // Persist without the short-lived access token.
            let persisted = TokenSet {
                access_token: String::new(),
                ..t.clone()
            };
            if let Ok(json) = serde_json::to_string(&persisted) {
                let _ = self.store.set(&self.cache_ref, &SecretString::from(json));
            }
        }
    }

    fn persisted(&self) -> Option<TokenSet> {
        let s = self.store.get(&self.cache_ref).ok().flatten()?;
        serde_json::from_str(s.expose_secret()).ok()
    }

    async fn obtain(&self, interactive_ok: bool) -> Result<TokenSet, AuthError> {
        // 1. refresh token (memory or keychain)
        let refresh = self
            .cached()
            .and_then(|t| t.refresh_token)
            .or_else(|| self.persisted().and_then(|t| t.refresh_token));
        let oauth_cfg = match &self.mode {
            TokenMode::Browser(c) | TokenMode::Device(c) => Some(c),
            _ => None,
        };
        if let (Some(cfg), Some(rt)) = (oauth_cfg, refresh) {
            match oauth::refresh(cfg, &rt).await {
                Ok(mut t) => {
                    if t.identity.is_none() {
                        t.identity = self.persisted().and_then(|p| p.identity);
                    }
                    return Ok(t);
                }
                Err(AuthError::ReauthRequired) => {
                    let _ = self.store.delete(&self.cache_ref);
                }
                Err(e) => return Err(e),
            }
        }
        match &self.mode {
            TokenMode::Browser(cfg) => {
                if !interactive_ok {
                    return Err(AuthError::ReauthRequired);
                }
                oauth::browser_flow(cfg, self.interaction.as_ref()).await
            }
            TokenMode::Device(cfg) => {
                if !interactive_ok {
                    return Err(AuthError::ReauthRequired);
                }
                oauth::device_flow(cfg, self.interaction.as_ref()).await
            }
            TokenMode::ClientCredentials(cfg) => {
                let mut cfg = cfg.clone();
                if cfg.client_secret.is_none() {
                    cfg.client_secret = Some(
                        read_secret(self.store.as_ref(), &self.primary_ref)?
                            .ok_or_else(|| AuthError::NotFound("client secret".into()))?,
                    );
                }
                let mut t = oauth::client_credentials(&cfg).await?;
                t.identity = Some(cfg.client_id.clone());
                Ok(t)
            }
            TokenMode::GoogleServiceAccount { scopes } => {
                let json = read_secret(self.store.as_ref(), &self.primary_ref)?
                    .ok_or_else(|| AuthError::NotFound("service account JSON key".into()))?;
                cloud::google_service_account(json.expose_secret(), scopes).await
            }
            TokenMode::Cli(CliKind::GoogleAdc { scopes }) => cloud::google_adc(scopes).await,
            TokenMode::Cli(CliKind::Databricks { host }) => {
                cloud::databricks_cli(self.cli_profile.as_deref(), host).await
            }
            TokenMode::Cli(CliKind::Azure { resource }) => cloud::azure_cli(resource).await,
        }
    }

    async fn token(&self, interactive_ok: bool, force: bool) -> Result<TokenSet, AuthError> {
        if !force {
            if let Some(t) = self
                .cached()
                .filter(|t| !t.access_token.is_empty() && t.fresh(120))
            {
                return Ok(t);
            }
        }
        let _g = self.flight.lock().await;
        if !force {
            if let Some(t) = self
                .cached()
                .filter(|t| !t.access_token.is_empty() && t.fresh(120))
            {
                return Ok(t);
            }
        }
        let t = self.obtain(interactive_ok).await?;
        self.store_tokens(&t);
        Ok(t)
    }
}

#[async_trait]
impl CredentialSource for TokenSource {
    async fn get(&self) -> Result<Credential, AuthError> {
        let t = self.token(true, false).await?;
        Ok(Credential::Bearer {
            token: SecretString::from(t.access_token),
            user: self.user.clone().or(t.identity),
        })
    }

    async fn invalidate(&self) {
        if let Some(t) = self.state.lock().expect("poisoned").as_mut() {
            t.expires_at = Some(0);
        }
    }

    async fn sign_in(&self) -> Result<AuthStatus, AuthError> {
        let force_interactive = matches!(self.mode, TokenMode::Browser(_) | TokenMode::Device(_));
        if force_interactive {
            let _g = self.flight.lock().await;
            let t = match &self.mode {
                TokenMode::Browser(cfg) => {
                    oauth::browser_flow(cfg, self.interaction.as_ref()).await?
                }
                TokenMode::Device(cfg) => {
                    oauth::device_flow(cfg, self.interaction.as_ref()).await?
                }
                _ => unreachable!(),
            };
            self.store_tokens(&t);
        } else {
            self.token(true, true).await?;
        }
        Ok(self.status())
    }

    async fn sign_out(&self) -> Result<(), AuthError> {
        let t = self.cached().or_else(|| self.persisted());
        *self.state.lock().expect("poisoned") = None;
        self.store.delete(&self.cache_ref)?;
        if let (TokenMode::Browser(cfg) | TokenMode::Device(cfg), Some(t)) = (&self.mode, t) {
            if let Some(rt) = t.refresh_token {
                oauth::revoke(cfg, &rt).await;
            }
        }
        Ok(())
    }

    fn status(&self) -> AuthStatus {
        match self.cached().filter(|t| !t.access_token.is_empty()) {
            Some(t) => AuthStatus {
                signed_in: true,
                identity: t.identity.or(self.user.clone()),
                expires_at: t.expires_at,
            },
            None => match self.persisted() {
                Some(p) => AuthStatus {
                    signed_in: p.refresh_token.is_some(),
                    identity: p.identity,
                    expires_at: None,
                },
                None => AuthStatus {
                    signed_in: false,
                    identity: self.user.clone(),
                    expires_at: None,
                },
            },
        }
    }
}

/// Snowflake-style browser SSO executed by the connector.
struct ExternalBrowserSource {
    user: String,
    interaction: Arc<dyn Interaction>,
}

#[async_trait]
impl CredentialSource for ExternalBrowserSource {
    async fn get(&self) -> Result<Credential, AuthError> {
        Ok(Credential::ExternalBrowser {
            user: self.user.clone(),
            interaction: self.interaction.clone(),
        })
    }
    fn status(&self) -> AuthStatus {
        AuthStatus {
            signed_in: false,
            identity: Some(self.user.clone()),
            expires_at: None,
        }
    }
}

/// Connector-provided context for building credential sources.
#[derive(Clone, Default)]
pub struct AuthContext {
    /// Endpoints for OAuth methods (browser, device, client credentials).
    pub oauth: Option<OAuthConfig>,
    /// CLI login to reuse for [`AuthMethod::CloudCli`].
    pub cli: Option<CliKind>,
    /// Scopes for Google service accounts.
    pub google_scopes: Vec<String>,
}

/// Build the credential source for a saved connection (or AI provider).
/// `owner_id` scopes the keychain entries.
pub fn credential_source(
    method: &AuthMethod,
    owner_id: &str,
    store: Arc<dyn SecretStore>,
    interaction: Arc<dyn Interaction>,
    ctx: AuthContext,
) -> Result<Arc<dyn CredentialSource>, AuthError> {
    let primary = SecretRef::for_connection(owner_id);
    let token_source = |mode: TokenMode| -> Arc<dyn CredentialSource> {
        Arc::new(TokenSource {
            mode,
            user: method.user().map(str::to_string),
            store: store.clone(),
            cache_ref: SecretRef::slot(owner_id, "oauth"),
            primary_ref: primary.clone(),
            cli_profile: match method {
                AuthMethod::CloudCli { profile } => profile.clone(),
                _ => None,
            },
            interaction: interaction.clone(),
            state: Mutex::new(None),
            flight: tokio::sync::Mutex::new(()),
        })
    };
    let oauth_cfg = || -> Result<OAuthConfig, AuthError> {
        let mut cfg = ctx.oauth.clone().ok_or_else(|| {
            AuthError::Invalid("this connection type does not support OAuth sign-in".into())
        })?;
        if cfg.client_id.trim().is_empty() {
            return Err(AuthError::Invalid(format!(
                "{} OAuth needs a client ID (Advanced → OAuth client ID)",
                cfg.provider
            )));
        }
        if cfg.client_secret.is_none() {
            cfg.client_secret =
                read_secret(store.as_ref(), &SecretRef::slot(owner_id, "client_secret"))?;
        }
        Ok(cfg)
    };
    Ok(match method {
        AuthMethod::None | AuthMethod::Password { .. } | AuthMethod::ApiToken { .. } => Arc::new(
            StoredCredentialSource::new(method.clone(), primary, store.clone()),
        ),
        AuthMethod::KeyPair { .. } => Arc::new(
            StoredCredentialSource::new(method.clone(), primary, store.clone())
                .with_passphrase(SecretRef::slot(owner_id, "passphrase")),
        ),
        AuthMethod::OauthBrowser(_) => token_source(TokenMode::Browser(oauth_cfg()?)),
        AuthMethod::DeviceCode(_) => token_source(TokenMode::Device(oauth_cfg()?)),
        AuthMethod::ClientCredentials(_) => {
            let mut cfg = ctx.oauth.clone().ok_or_else(|| {
                AuthError::Invalid(
                    "this connection type does not support client credentials".into(),
                )
            })?;
            cfg.client_secret = None; // primary secret
            token_source(TokenMode::ClientCredentials(cfg))
        }
        AuthMethod::ExternalBrowser { user } => Arc::new(ExternalBrowserSource {
            user: user.clone(),
            interaction: interaction.clone(),
        }),
        AuthMethod::CloudCli { .. } => {
            token_source(TokenMode::Cli(ctx.cli.clone().ok_or_else(|| {
                AuthError::Invalid("this connection type has no CLI login to reuse".into())
            })?))
        }
        AuthMethod::ServiceAccount => token_source(TokenMode::GoogleServiceAccount {
            scopes: ctx.google_scopes.clone(),
        }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stored_password_roundtrip() {
        let store: Arc<dyn SecretStore> = Arc::new(MemoryStore::default());
        let r = SecretRef::for_connection("c1");
        store.set(&r, &SecretString::from("s3cret")).unwrap();
        let src = credential_source(
            &AuthMethod::Password {
                user: "alice".into(),
            },
            "c1",
            store.clone(),
            Arc::new(NonInteractive),
            AuthContext::default(),
        )
        .unwrap();
        match src.get().await.unwrap() {
            Credential::Password { user, password } => {
                assert_eq!(user, "alice");
                assert_eq!(password.expose_secret(), "s3cret");
            }
            other => panic!("unexpected {other:?}"),
        }
        assert!(src.status().signed_in);
    }

    #[tokio::test]
    async fn missing_token_is_error() {
        let store: Arc<dyn SecretStore> = Arc::new(MemoryStore::default());
        let src = StoredCredentialSource::new(
            AuthMethod::ApiToken { user: None },
            SecretRef::for_connection("x"),
            store,
        );
        assert!(matches!(src.get().await, Err(AuthError::NotFound(_))));
    }

    #[tokio::test]
    async fn oauth_without_session_is_reauth_when_noninteractive() {
        let store: Arc<dyn SecretStore> = Arc::new(MemoryStore::default());
        let ctx = AuthContext {
            oauth: Some(OAuthConfig {
                provider: "T".into(),
                authorize_url: "https://idp.invalid/a".into(),
                token_url: "https://idp.invalid/t".into(),
                device_url: None,
                revoke_url: None,
                client_id: "cid".into(),
                client_secret: None,
                scopes: vec![],
                redirect_host: "127.0.0.1".into(),
                redirect_port: None,
                redirect_path: "/callback".into(),
                extra_authorize_params: vec![],
                client_auth: ClientAuthStyle::Body,
            }),
            ..Default::default()
        };
        let src = credential_source(
            &AuthMethod::OauthBrowser(OAuthParams::default()),
            "c",
            store,
            Arc::new(NonInteractive),
            ctx,
        )
        .unwrap();
        assert!(matches!(src.get().await, Err(AuthError::ReauthRequired)));
        assert!(!src.status().signed_in);
    }

    #[test]
    fn debug_redacts_secrets() {
        let c = Credential::Password {
            user: "u".into(),
            password: SecretString::from("topsecret"),
        };
        assert!(!format!("{c:?}").contains("topsecret"));
    }

    #[test]
    fn auth_method_serde() {
        let m = AuthMethod::Password { user: "bob".into() };
        let s = serde_json::to_string(&m).unwrap();
        assert_eq!(s, r#"{"method":"password","user":"bob"}"#);
        assert_eq!(serde_json::from_str::<AuthMethod>(&s).unwrap(), m);
        // v1 token config still parses
        assert_eq!(
            serde_json::from_str::<AuthMethod>(r#"{"method":"api_token"}"#).unwrap(),
            AuthMethod::ApiToken { user: None }
        );
        let o: AuthMethod =
            serde_json::from_str(r#"{"method":"oauth_browser","client_id":"x"}"#).unwrap();
        assert_eq!(o.oauth_params().unwrap().client_id.as_deref(), Some("x"));
    }
}
