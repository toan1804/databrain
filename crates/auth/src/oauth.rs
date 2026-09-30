//! OAuth 2.0 / OIDC client flows for native apps.

use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::RngCore;
use rsa::sha2::{Digest, Sha256};
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};

use crate::loopback::Loopback;
use crate::{AuthError, DeviceCodePrompt, Interaction, now_secs};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientAuthStyle {
    /// `client_id` / `client_secret` in the form body.
    #[default]
    Body,
    /// HTTP Basic authentication (Snowflake confidential clients).
    Basic,
}

/// Provider endpoints and client settings for one connection.
#[derive(Clone)]
pub struct OAuthConfig {
    /// Display name, e.g. "Databricks".
    pub provider: String,
    pub authorize_url: String,
    pub token_url: String,
    pub device_url: Option<String>,
    pub revoke_url: Option<String>,
    pub client_id: String,
    pub client_secret: Option<SecretString>,
    pub scopes: Vec<String>,
    /// Host in the redirect URI: `127.0.0.1` or `localhost`.
    pub redirect_host: String,
    /// Fixed port when the provider requires a registered redirect URI.
    pub redirect_port: Option<u16>,
    pub redirect_path: String,
    pub extra_authorize_params: Vec<(String, String)>,
    pub client_auth: ClientAuthStyle,
}

impl std::fmt::Debug for OAuthConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuthConfig")
            .field("provider", &self.provider)
            .field("authorize_url", &self.authorize_url)
            .field("client_id", &self.client_id)
            .field("scopes", &self.scopes)
            .finish_non_exhaustive()
    }
}

/// Tokens persisted (refresh token) and cached (access token).
#[derive(Clone, Serialize, Deserialize)]
pub struct TokenSet {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    /// Unix seconds.
    #[serde(default)]
    pub expires_at: Option<u64>,
    #[serde(default)]
    pub identity: Option<String>,
}

impl TokenSet {
    /// True if the access token is valid for at least `margin` seconds.
    pub fn fresh(&self, margin: u64) -> bool {
        self.expires_at.is_none_or(|e| e > now_secs() + margin)
    }
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<serde_json::Value>,
    #[serde(default)]
    id_token: Option<String>,
    /// Snowflake returns the user name here.
    #[serde(default)]
    username: Option<String>,
}

#[derive(Deserialize)]
struct ErrorResponse {
    error: String,
    #[serde(default)]
    error_description: Option<String>,
}

pub fn http() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .connect_timeout(Duration::from_secs(15))
        .user_agent(concat!("DataBrain/", env!("CARGO_PKG_VERSION")))
        .build()
        .expect("http client")
}

pub fn random_token(bytes: usize) -> String {
    let mut b = vec![0u8; bytes];
    rand::thread_rng().fill_bytes(&mut b);
    URL_SAFE_NO_PAD.encode(b)
}

pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

pub fn pkce() -> Pkce {
    let verifier = random_token(48);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    Pkce {
        verifier,
        challenge,
    }
}

fn identity_from(resp: &TokenResponse) -> Option<String> {
    if let Some(u) = &resp.username {
        return Some(u.clone());
    }
    let claims = crate::jwt::decode_payload(resp.id_token.as_deref()?)?;
    ["email", "preferred_username", "upn", "name", "sub"]
        .iter()
        .find_map(|k| claims.get(*k).and_then(|v| v.as_str()).map(str::to_string))
}

fn expires_at(v: &Option<serde_json::Value>) -> Option<u64> {
    let secs = match v.as_ref()? {
        serde_json::Value::Number(n) => n.as_u64()?,
        serde_json::Value::String(s) => s.parse().ok()?,
        _ => return None,
    };
    Some(now_secs() + secs)
}

async fn token_request(
    cfg: &OAuthConfig,
    mut form: Vec<(&'static str, String)>,
) -> Result<TokenResponse, TokenError> {
    let mut req = http()
        .post(&cfg.token_url)
        .header("Accept", "application/json");
    match (cfg.client_auth, &cfg.client_secret) {
        (ClientAuthStyle::Basic, Some(secret)) => {
            req = req.basic_auth(&cfg.client_id, Some(secret.expose_secret()));
        }
        (ClientAuthStyle::Basic, None) => form.push(("client_id", cfg.client_id.clone())),
        (ClientAuthStyle::Body, secret) => {
            form.push(("client_id", cfg.client_id.clone()));
            if let Some(s) = secret {
                form.push(("client_secret", s.expose_secret().to_string()));
            }
        }
    }
    let resp = req
        .form(&form)
        .send()
        .await
        .map_err(|e| TokenError::Other(AuthError::Network(e.to_string())))?;
    let status = resp.status();
    let body = resp
        .text()
        .await
        .map_err(|e| TokenError::Other(AuthError::Network(e.to_string())))?;
    if status.is_success() {
        return serde_json::from_str(&body).map_err(|e| {
            TokenError::Other(AuthError::Flow(format!(
                "unexpected token response from {}: {e}",
                cfg.provider
            )))
        });
    }
    match serde_json::from_str::<ErrorResponse>(&body) {
        Ok(er) => Err(TokenError::OAuth {
            description: er
                .error_description
                .clone()
                .unwrap_or_else(|| er.error.clone()),
            error: er.error,
        }),
        Err(_) => Err(TokenError::Other(AuthError::Flow(format!(
            "{} token endpoint returned HTTP {status}: {}",
            cfg.provider,
            body.chars().take(300).collect::<String>()
        )))),
    }
}

enum TokenError {
    OAuth { error: String, description: String },
    Other(AuthError),
}

impl From<TokenError> for AuthError {
    fn from(e: TokenError) -> Self {
        match e {
            TokenError::OAuth { error, description } if error == "invalid_grant" => {
                let _ = description;
                AuthError::ReauthRequired
            }
            TokenError::OAuth { error, description } => {
                AuthError::Flow(format!("{error}: {description}"))
            }
            TokenError::Other(e) => e,
        }
    }
}

fn to_set(r: TokenResponse, previous_refresh: Option<String>) -> TokenSet {
    let identity = identity_from(&r);
    TokenSet {
        expires_at: expires_at(&r.expires_in),
        access_token: r.access_token,
        refresh_token: r.refresh_token.or(previous_refresh),
        identity,
    }
}

/// Build the authorization URL (exposed for tests).
pub fn authorize_url(
    cfg: &OAuthConfig,
    redirect_uri: &str,
    state: &str,
    challenge: &str,
) -> Result<String, AuthError> {
    let mut url = url::Url::parse(&cfg.authorize_url)
        .map_err(|e| AuthError::Invalid(format!("invalid authorize URL: {e}")))?;
    {
        let mut q = url.query_pairs_mut();
        q.append_pair("response_type", "code")
            .append_pair("client_id", &cfg.client_id)
            .append_pair("redirect_uri", redirect_uri)
            .append_pair("state", state)
            .append_pair("code_challenge", challenge)
            .append_pair("code_challenge_method", "S256");
        if !cfg.scopes.is_empty() {
            q.append_pair("scope", &cfg.scopes.join(" "));
        }
        for (k, v) in &cfg.extra_authorize_params {
            q.append_pair(k, v);
        }
    }
    Ok(url.to_string())
}

/// Authorization Code + PKCE via the system browser and a loopback redirect.
pub async fn browser_flow(
    cfg: &OAuthConfig,
    interaction: &dyn Interaction,
) -> Result<TokenSet, AuthError> {
    if !interaction.interactive() {
        return Err(AuthError::ReauthRequired);
    }
    let lb = Loopback::bind(
        &cfg.redirect_host,
        cfg.redirect_port.unwrap_or(0),
        &cfg.redirect_path,
    )
    .await?;
    let redirect_uri = lb.redirect_uri();
    let state = random_token(24);
    let p = pkce();
    let url = authorize_url(cfg, &redirect_uri, &state, &p.challenge)?;
    interaction.open_url(&url).await?;
    let cancel = interaction.cancel_token();
    let params = lb
        .wait(Some(&state), &cancel, Duration::from_secs(300))
        .await;
    interaction.finished().await;
    let params = params?;
    if let Some(err) = params.get("error") {
        return Err(AuthError::Flow(format!(
            "{} sign-in failed: {}",
            cfg.provider,
            params.get("error_description").unwrap_or(err)
        )));
    }
    let code = params.get("code").ok_or_else(|| {
        AuthError::Flow("the redirect did not include an authorization code".into())
    })?;
    let r = token_request(
        cfg,
        vec![
            ("grant_type", "authorization_code".into()),
            ("code", code.clone()),
            ("redirect_uri", redirect_uri),
            ("code_verifier", p.verifier),
        ],
    )
    .await?;
    Ok(to_set(r, None))
}

#[derive(Deserialize)]
struct DeviceResponse {
    device_code: String,
    user_code: String,
    #[serde(alias = "verification_url")]
    verification_uri: String,
    #[serde(default)]
    verification_uri_complete: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
    #[serde(default)]
    interval: Option<u64>,
    #[serde(default)]
    message: Option<String>,
}

/// Device Authorization Grant (RFC 8628).
pub async fn device_flow(
    cfg: &OAuthConfig,
    interaction: &dyn Interaction,
) -> Result<TokenSet, AuthError> {
    if !interaction.interactive() {
        return Err(AuthError::ReauthRequired);
    }
    let device_url = cfg.device_url.as_deref().ok_or_else(|| {
        AuthError::Invalid(format!(
            "{} does not support device code sign-in",
            cfg.provider
        ))
    })?;
    let resp = http()
        .post(device_url)
        .header("Accept", "application/json")
        .form(&[
            ("client_id", cfg.client_id.as_str()),
            ("scope", &cfg.scopes.join(" ")),
        ])
        .send()
        .await
        .map_err(|e| AuthError::Network(e.to_string()))?;
    if !resp.status().is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(AuthError::Flow(format!(
            "device authorization failed: {}",
            body.chars().take(300).collect::<String>()
        )));
    }
    let d: DeviceResponse = resp
        .json()
        .await
        .map_err(|e| AuthError::Flow(e.to_string()))?;
    interaction
        .device_code(&DeviceCodePrompt {
            provider: cfg.provider.clone(),
            user_code: d.user_code.clone(),
            verification_uri: d.verification_uri.clone(),
            verification_uri_complete: d.verification_uri_complete.clone(),
            message: d.message.clone(),
        })
        .await;
    let cancel = interaction.cancel_token();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(d.expires_in.unwrap_or(900));
    let mut interval = d.interval.unwrap_or(5).max(1);
    let result = loop {
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(interval)) => {}
            _ = cancel.cancelled() => break Err(AuthError::Cancelled),
        }
        if tokio::time::Instant::now() > deadline {
            break Err(AuthError::Flow("the device code expired".into()));
        }
        match token_request(
            cfg,
            vec![
                (
                    "grant_type",
                    "urn:ietf:params:oauth:grant-type:device_code".into(),
                ),
                ("device_code", d.device_code.clone()),
            ],
        )
        .await
        {
            Ok(r) => break Ok(to_set(r, None)),
            Err(TokenError::OAuth { error, .. }) if error == "authorization_pending" => continue,
            Err(TokenError::OAuth { error, .. }) if error == "slow_down" => interval += 5,
            Err(e) => break Err(e.into()),
        }
    };
    interaction.finished().await;
    result
}

pub async fn refresh(cfg: &OAuthConfig, refresh_token: &str) -> Result<TokenSet, AuthError> {
    let mut form = vec![
        ("grant_type", "refresh_token".to_string()),
        ("refresh_token", refresh_token.to_string()),
    ];
    if !cfg.scopes.is_empty() {
        form.push(("scope", cfg.scopes.join(" ")));
    }
    let r = token_request(cfg, form).await?;
    Ok(to_set(r, Some(refresh_token.to_string())))
}

pub async fn client_credentials(cfg: &OAuthConfig) -> Result<TokenSet, AuthError> {
    let mut form = vec![("grant_type", "client_credentials".to_string())];
    if !cfg.scopes.is_empty() {
        form.push(("scope", cfg.scopes.join(" ")));
    }
    let r = token_request(cfg, form).await?;
    Ok(to_set(r, None))
}

/// Best-effort token revocation (RFC 7009).
pub async fn revoke(cfg: &OAuthConfig, token: &str) {
    if let Some(url) = &cfg.revoke_url {
        let _ = http()
            .post(url)
            .form(&[("token", token), ("client_id", cfg.client_id.as_str())])
            .send()
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> OAuthConfig {
        OAuthConfig {
            provider: "Test".into(),
            authorize_url: "https://idp.example/authorize?tenant=a".into(),
            token_url: "https://idp.example/token".into(),
            device_url: None,
            revoke_url: None,
            client_id: "cid".into(),
            client_secret: None,
            scopes: vec!["sql".into(), "offline_access".into()],
            redirect_host: "localhost".into(),
            redirect_port: Some(8020),
            redirect_path: "/".into(),
            extra_authorize_params: vec![("prompt".into(), "consent".into())],
            client_auth: ClientAuthStyle::Body,
        }
    }

    #[test]
    fn builds_authorize_url() {
        let u = authorize_url(&cfg(), "http://localhost:8020", "st", "ch").unwrap();
        let parsed = url::Url::parse(&u).unwrap();
        let q: std::collections::HashMap<_, _> = parsed.query_pairs().into_owned().collect();
        assert_eq!(q["tenant"], "a");
        assert_eq!(q["client_id"], "cid");
        assert_eq!(q["code_challenge_method"], "S256");
        assert_eq!(q["scope"], "sql offline_access");
        assert_eq!(q["prompt"], "consent");
        assert_eq!(q["redirect_uri"], "http://localhost:8020");
    }

    #[test]
    fn pkce_challenge_is_s256() {
        let p = pkce();
        assert!(p.verifier.len() >= 43);
        assert_eq!(
            p.challenge,
            URL_SAFE_NO_PAD.encode(Sha256::digest(p.verifier.as_bytes()))
        );
    }

    #[test]
    fn token_freshness() {
        let t = TokenSet {
            access_token: "a".into(),
            refresh_token: None,
            expires_at: Some(now_secs() + 30),
            identity: None,
        };
        assert!(t.fresh(10));
        assert!(!t.fresh(60));
    }
}
