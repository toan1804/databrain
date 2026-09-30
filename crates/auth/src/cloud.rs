//! Reuse of existing cloud credentials: Google service accounts and
//! Application Default Credentials, the Databricks CLI, and the Azure CLI.
//! Files are only read, never modified.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::oauth::{self, ClientAuthStyle, OAuthConfig, TokenSet};
use crate::{AuthError, now_secs};

pub const GOOGLE_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";

#[derive(Deserialize)]
struct ServiceAccountKey {
    client_email: String,
    private_key: String,
    #[serde(default)]
    private_key_id: Option<String>,
    #[serde(default)]
    token_uri: Option<String>,
}

/// Exchange a Google service-account JSON key for an access token.
pub async fn google_service_account(json: &str, scopes: &[String]) -> Result<TokenSet, AuthError> {
    let key: ServiceAccountKey = serde_json::from_str(json)
        .map_err(|e| AuthError::Invalid(format!("invalid service account JSON: {e}")))?;
    let rsa = crate::jwt::parse_private_key(&key.private_key, None)?;
    let token_uri = key
        .token_uri
        .clone()
        .unwrap_or_else(|| GOOGLE_TOKEN_URL.into());
    let iat = now_secs();
    let mut header = serde_json::json!({"alg": "RS256", "typ": "JWT"});
    if let Some(kid) = &key.private_key_id {
        header["kid"] = kid.clone().into();
    }
    let claims = serde_json::json!({
        "iss": key.client_email,
        "scope": scopes.join(" "),
        "aud": token_uri,
        "iat": iat,
        "exp": iat + 3600,
    });
    let assertion = crate::jwt::sign_rs256(&rsa, &header, &claims)?;
    let resp = oauth::http()
        .post(&token_uri)
        .form(&[
            ("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"),
            ("assertion", assertion.as_str()),
        ])
        .send()
        .await
        .map_err(|e| AuthError::Network(e.to_string()))?;
    let status = resp.status();
    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| AuthError::Network(e.to_string()))?;
    if !status.is_success() {
        return Err(AuthError::Flow(format!(
            "Google token exchange failed: {}",
            body.get("error_description")
                .or(body.get("error"))
                .unwrap_or(&body)
        )));
    }
    Ok(TokenSet {
        access_token: body["access_token"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        refresh_token: None,
        expires_at: body["expires_in"].as_u64().map(|s| now_secs() + s),
        identity: Some(key.client_email),
    })
}

fn adc_path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("GOOGLE_APPLICATION_CREDENTIALS") {
        return Some(PathBuf::from(p));
    }
    #[cfg(windows)]
    let base = std::env::var("APPDATA").ok().map(PathBuf::from);
    #[cfg(not(windows))]
    let base = dirs::home_dir().map(|h| h.join(".config"));
    Some(
        base?
            .join("gcloud")
            .join("application_default_credentials.json"),
    )
}

/// Google Application Default Credentials (`gcloud auth application-default login`).
pub async fn google_adc(scopes: &[String]) -> Result<TokenSet, AuthError> {
    let path = adc_path()
        .ok_or_else(|| AuthError::NotFound("gcloud application default credentials".into()))?;
    let text = std::fs::read_to_string(&path).map_err(|_| {
        AuthError::NotFound(format!(
            "{} (run `gcloud auth application-default login`)",
            path.display()
        ))
    })?;
    let v: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| AuthError::Invalid(e.to_string()))?;
    match v["type"].as_str() {
        Some("service_account") => google_service_account(&text, scopes).await,
        Some("authorized_user") => {
            let cfg = OAuthConfig {
                provider: "Google".into(),
                authorize_url: String::new(),
                token_url: GOOGLE_TOKEN_URL.into(),
                device_url: None,
                revoke_url: None,
                client_id: v["client_id"].as_str().unwrap_or_default().into(),
                client_secret: v["client_secret"].as_str().map(|s| s.to_string().into()),
                // ADC refresh tokens carry their own scopes.
                scopes: vec![],
                redirect_host: String::new(),
                redirect_port: None,
                redirect_path: String::new(),
                extra_authorize_params: vec![],
                client_auth: ClientAuthStyle::Body,
            };
            let rt = v["refresh_token"]
                .as_str()
                .ok_or_else(|| AuthError::Invalid("ADC file has no refresh_token".into()))?;
            let mut t = oauth::refresh(&cfg, rt).await?;
            t.identity = v["account"]
                .as_str()
                .map(str::to_string)
                .or(Some("gcloud ADC".into()));
            Ok(t)
        }
        other => Err(AuthError::Invalid(format!(
            "unsupported ADC credential type {other:?}"
        ))),
    }
}

/// A profile from `~/.databrickscfg`.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct DatabricksProfile {
    pub name: String,
    pub host: Option<String>,
    pub token: Option<String>,
    pub auth_type: Option<String>,
}

pub fn parse_databrickscfg(text: &str) -> Vec<DatabricksProfile> {
    let mut out: Vec<DatabricksProfile> = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            out.push(DatabricksProfile {
                name: name.trim().to_string(),
                ..Default::default()
            });
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let Some(p) = out.last_mut() else { continue };
        let v = v.trim().to_string();
        match k.trim() {
            "host" => p.host = Some(v),
            "token" => p.token = Some(v),
            "auth_type" => p.auth_type = Some(v),
            _ => {}
        }
    }
    out
}

fn normalize_host(h: &str) -> String {
    h.trim()
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_end_matches('/')
        .to_ascii_lowercase()
}

/// Credentials from the Databricks CLI: a PAT from `~/.databrickscfg`, or an
/// OAuth token from `databricks auth token` (after `databricks auth login`).
pub async fn databricks_cli(profile: Option<&str>, host: &str) -> Result<TokenSet, AuthError> {
    let path = std::env::var("DATABRICKS_CONFIG_FILE")
        .map(PathBuf::from)
        .ok()
        .or_else(|| dirs::home_dir().map(|h| h.join(".databrickscfg")));
    let profiles = path
        .as_ref()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|t| parse_databrickscfg(&t))
        .unwrap_or_default();
    let want = normalize_host(host);
    let chosen = match profile.filter(|p| !p.is_empty()) {
        Some(name) => profiles.iter().find(|p| p.name == name),
        None => profiles
            .iter()
            .find(|p| p.host.as_deref().map(normalize_host) == Some(want.clone())),
    };
    if let Some(p) = chosen {
        if let Some(t) = &p.token {
            return Ok(TokenSet {
                access_token: t.clone(),
                refresh_token: None,
                expires_at: None,
                identity: Some(format!("databricks profile {}", p.name)),
            });
        }
    }
    let mut cmd = tokio::process::Command::new("databricks");
    cmd.args(["auth", "token"]);
    match chosen {
        Some(p) => {
            cmd.args(["--profile", &p.name]);
        }
        None => {
            cmd.args(["--host", &format!("https://{want}")]);
        }
    }
    let out = cmd.output().await.map_err(|e| {
        AuthError::NotFound(format!(
            "Databricks CLI not available ({e}); install it and run `databricks auth login`"
        ))
    })?;
    if !out.status.success() {
        return Err(AuthError::Flow(format!(
            "`databricks auth token` failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    let v: serde_json::Value =
        serde_json::from_slice(&out.stdout).map_err(|e| AuthError::Flow(e.to_string()))?;
    Ok(TokenSet {
        access_token: v["access_token"].as_str().unwrap_or_default().to_string(),
        refresh_token: None,
        // CLI output has an RFC 3339 expiry; refresh conservatively.
        expires_at: Some(now_secs() + 30 * 60),
        identity: Some("databricks CLI".into()),
    })
}

/// Access token from `az account get-access-token`.
pub async fn azure_cli(resource: &str) -> Result<TokenSet, AuthError> {
    let out = tokio::process::Command::new("az")
        .args([
            "account",
            "get-access-token",
            "--resource",
            resource,
            "--output",
            "json",
        ])
        .output()
        .await
        .map_err(|e| {
            AuthError::NotFound(format!(
                "Azure CLI not available ({e}); install it and run `az login`"
            ))
        })?;
    if !out.status.success() {
        return Err(AuthError::Flow(format!(
            "`az account get-access-token` failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    let v: serde_json::Value =
        serde_json::from_slice(&out.stdout).map_err(|e| AuthError::Flow(e.to_string()))?;
    Ok(TokenSet {
        access_token: v["accessToken"].as_str().unwrap_or_default().to_string(),
        refresh_token: None,
        expires_at: v["expires_on"].as_u64().or(Some(now_secs() + 30 * 60)),
        identity: Some("Azure CLI".into()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_databrickscfg() {
        let p = parse_databrickscfg(
            "[DEFAULT]\nhost = https://adb-1.azuredatabricks.net/\ntoken = dapi123\n\n# c\n[oauth]\nhost=https://x.cloud.databricks.com\nauth_type = databricks-cli\n",
        );
        assert_eq!(p.len(), 2);
        assert_eq!(p[0].token.as_deref(), Some("dapi123"));
        assert_eq!(p[1].auth_type.as_deref(), Some("databricks-cli"));
        assert_eq!(
            normalize_host(p[0].host.as_deref().unwrap()),
            "adb-1.azuredatabricks.net"
        );
    }
}
