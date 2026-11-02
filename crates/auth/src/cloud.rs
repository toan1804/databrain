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
    let key: ServiceAccountKey = serde_json::from_str(json).map_err(|e| AuthError::Invalid(format!("invalid service account JSON: {e}")))?;
    let rsa = crate::jwt::parse_private_key(&key.private_key, None)?;
    let token_uri = key.token_uri.clone().unwrap_or_else(|| GOOGLE_TOKEN_URL.into());
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
        .form(&[("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"), ("assertion", assertion.as_str())])
        .send()
        .await
        .map_err(|e| AuthError::Network(e.to_string()))?;
    let status = resp.status();
    let body: serde_json::Value = resp.json().await.map_err(|e| AuthError::Network(e.to_string()))?;
    if !status.is_success() {
        return Err(AuthError::Flow(format!("Google token exchange failed: {}", body.get("error_description").or(body.get("error")).unwrap_or(&body))));
    }
    Ok(TokenSet {
        access_token: body["access_token"].as_str().unwrap_or_default().to_string(),
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
    Some(base?.join("gcloud").join("application_default_credentials.json"))
}

/// Google Application Default Credentials (`gcloud auth application-default login`).
pub async fn google_adc(scopes: &[String]) -> Result<TokenSet, AuthError> {
    let path = adc_path().ok_or_else(|| AuthError::NotFound("gcloud application default credentials".into()))?;
    let text = std::fs::read_to_string(&path).map_err(|_| AuthError::NotFound(format!("{} (run `gcloud auth application-default login`)", path.display())))?;
    let v: serde_json::Value = serde_json::from_str(&text).map_err(|e| AuthError::Invalid(e.to_string()))?;
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
            let rt = v["refresh_token"].as_str().ok_or_else(|| AuthError::Invalid("ADC file has no refresh_token".into()))?;
            let mut t = oauth::refresh(&cfg, rt).await?;
            t.identity = v["account"].as_str().map(str::to_string).or(Some("gcloud ADC".into()));
            Ok(t)
        }
        other => Err(AuthError::Invalid(format!("unsupported ADC credential type {other:?}"))),
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
            out.push(DatabricksProfile { name: name.trim().to_string(), ..Default::default() });
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
    h.trim().trim_start_matches("https://").trim_start_matches("http://").trim_end_matches('/').to_ascii_lowercase()
}

/// Credentials from the Databricks CLI: a PAT from `~/.databrickscfg`, or an
/// OAuth token from `databricks auth token` (after `databricks auth login`).
pub async fn databricks_cli(profile: Option<&str>, host: &str) -> Result<TokenSet, AuthError> {
    let path = std::env::var("DATABRICKS_CONFIG_FILE").map(PathBuf::from).ok().or_else(|| dirs::home_dir().map(|h| h.join(".databrickscfg")));
    let profiles = path.as_ref().and_then(|p| std::fs::read_to_string(p).ok()).map(|t| parse_databrickscfg(&t)).unwrap_or_default();
    let want = normalize_host(host);
    let chosen = match profile.filter(|p| !p.is_empty()) {
        Some(name) => profiles.iter().find(|p| p.name == name),
        None => profiles.iter().find(|p| p.host.as_deref().map(normalize_host) == Some(want.clone())),
    };
    if let Some(p) = chosen {
        if let Some(t) = &p.token {
            return Ok(TokenSet { access_token: t.clone(), refresh_token: None, expires_at: None, identity: Some(format!("databricks profile {}", p.name)) });
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
    let out = cmd.output().await.map_err(|e| AuthError::NotFound(format!("Databricks CLI not available ({e}); install it and run `databricks auth login`")))?;
    if !out.status.success() {
        return Err(AuthError::Flow(format!("`databricks auth token` failed: {}", String::from_utf8_lossy(&out.stderr).trim())));
    }
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).map_err(|e| AuthError::Flow(e.to_string()))?;
    Ok(TokenSet {
        access_token: v["access_token"].as_str().unwrap_or_default().to_string(),
        refresh_token: None,
        // CLI output has an RFC 3339 expiry; refresh conservatively.
        expires_at: Some(now_secs() + 30 * 60),
        identity: Some("databricks CLI".into()),
    })
}

/// Which Redshift database to get temporary credentials for.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RedshiftTarget {
    /// Provisioned cluster identifier.
    pub cluster: Option<String>,
    /// Serverless workgroup name.
    pub workgroup: Option<String>,
    pub region: Option<String>,
    pub database: String,
    /// Provisioned clusters: sign in as this database user
    /// (`get-cluster-credentials`) instead of the IAM identity's own user.
    pub db_user: Option<String>,
}

impl RedshiftTarget {
    /// Cluster or workgroup and region from an endpoint host name:
    /// `<cluster>.<id>.<region>.redshift.amazonaws.com` or
    /// `<workgroup>.<account>.<region>.redshift-serverless.amazonaws.com`.
    pub fn from_host(host: &str) -> RedshiftTarget {
        let h = host.trim().trim_end_matches('.').to_ascii_lowercase();
        let labels: Vec<&str> = h.split('.').collect();
        let mut t = RedshiftTarget::default();
        if labels.len() >= 6 && labels[labels.len() - 2] == "amazonaws" {
            match labels[labels.len() - 3] {
                "redshift" => {
                    t.cluster = Some(labels[0].to_string());
                    t.region = Some(labels[2].to_string());
                }
                "redshift-serverless" => {
                    t.workgroup = Some(labels[0].to_string());
                    t.region = Some(labels[2].to_string());
                }
                _ => {}
            }
        }
        t
    }
}

/// Seconds the temporary password is valid (the most Redshift allows).
const REDSHIFT_DURATION: u64 = 3600;

/// `aws` arguments for a target (without the program name).
pub fn redshift_args(profile: Option<&str>, t: &RedshiftTarget) -> Result<Vec<String>, AuthError> {
    let mut a: Vec<String> = match (&t.workgroup, &t.cluster) {
        (Some(w), _) => vec!["redshift-serverless".into(), "get-credentials".into(), "--workgroup-name".into(), w.clone()],
        (None, Some(c)) => match t.db_user.as_deref().filter(|u| !u.trim().is_empty()) {
            Some(u) => vec!["redshift".into(), "get-cluster-credentials".into(), "--cluster-identifier".into(), c.clone(), "--db-user".into(), u.trim().into()],
            None => vec!["redshift".into(), "get-cluster-credentials-with-iam".into(), "--cluster-identifier".into(), c.clone()],
        },
        (None, None) => {
            return Err(AuthError::Invalid(
                "IAM sign-in is for Amazon Redshift: use the cluster or workgroup endpoint as the host (…redshift.amazonaws.com), or set the Cluster ID or Workgroup".into(),
            ))
        }
    };
    a.extend(["--db-name".into(), if t.database.trim().is_empty() { "dev".into() } else { t.database.trim().into() }]);
    a.extend(["--duration-seconds".into(), REDSHIFT_DURATION.to_string(), "--output".into(), "json".into()]);
    if let Some(r) = t.region.as_deref().filter(|r| !r.trim().is_empty()) {
        a.extend(["--region".into(), r.trim().into()]);
    }
    if let Some(p) = profile.filter(|p| !p.trim().is_empty()) {
        a.extend(["--profile".into(), p.trim().into()]);
    }
    Ok(a)
}

/// `2026-11-01T12:34:56+00:00` / `…56.123Z` / epoch seconds → unix seconds.
fn parse_timestamp(v: &serde_json::Value) -> Option<u64> {
    if let Some(n) = v.as_f64() {
        return Some(n as u64);
    }
    let s = v.as_str()?.trim();
    let (date, rest) = s.split_once('T').or_else(|| s.split_once(' '))?;
    let mut d = date.split('-').map(|x| x.parse::<i64>().ok());
    let (y, m, day) = (d.next()??, d.next()??, d.next()??);
    let (time, offset) = match rest.find(['+', '-', 'Z', 'z']) {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    let mut t = time.split(':');
    let hh: i64 = t.next()?.parse().ok()?;
    let mm: i64 = t.next()?.parse().ok()?;
    let ss: i64 = t.next().unwrap_or("0").split('.').next()?.parse().ok()?;
    let off = match offset.chars().next() {
        Some(c @ ('+' | '-')) => {
            let o = offset[1..].replace(':', "");
            let (oh, om) = (o.get(..2)?.parse::<i64>().ok()?, o.get(2..4).unwrap_or("0").parse::<i64>().unwrap_or(0));
            (if c == '-' { -1 } else { 1 }) * (oh * 3600 + om * 60)
        }
        _ => 0,
    };
    // Days from the civil date (Howard Hinnant's algorithm).
    let (y2, m2) = if m <= 2 { (y - 1, m + 9) } else { (y, m - 3) };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let doy = (153 * m2 + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    u64::try_from(days * 86_400 + hh * 3600 + mm * 60 + ss - off).ok()
}

/// The CLI's JSON answer: user and password (both APIs' spellings) and expiry.
pub fn parse_redshift_credentials(out: &[u8]) -> Result<TokenSet, AuthError> {
    let v: serde_json::Value = serde_json::from_slice(out).map_err(|e| AuthError::Flow(format!("unexpected AWS CLI output: {e}")))?;
    let get = |a: &str, b: &str| v.get(a).or_else(|| v.get(b)).and_then(|x| x.as_str()).map(str::to_string);
    let password = get("DbPassword", "dbPassword").ok_or_else(|| AuthError::Flow("the AWS CLI returned no database password".into()))?;
    let user = get("DbUser", "dbUser").ok_or_else(|| AuthError::Flow("the AWS CLI returned no database user".into()))?;
    let expires = v.get("Expiration").or_else(|| v.get("expiration")).and_then(parse_timestamp).unwrap_or(now_secs() + REDSHIFT_DURATION - 60);
    Ok(TokenSet { access_token: password, refresh_token: None, expires_at: Some(expires), identity: Some(user) })
}

/// The `aws` program: on the PATH, else where installers put it (a GUI
/// app's PATH on macOS often lacks Homebrew's folder).
fn aws_program() -> PathBuf {
    let exe = if cfg!(windows) { "aws.exe" } else { "aws" };
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect()).unwrap_or_default();
    dirs.extend(["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin"].map(PathBuf::from));
    if let Some(h) = dirs::home_dir() {
        dirs.push(h.join(".local/bin"));
    }
    if cfg!(windows) {
        dirs.push(PathBuf::from(r"C:\Program Files\Amazon\AWSCLIV2"));
    }
    dirs.into_iter().map(|d| d.join(exe)).find(|p| p.is_file()).unwrap_or_else(|| PathBuf::from(exe))
}

/// Temporary Redshift credentials from the AWS CLI (its profiles, SSO,
/// MFA and assumed roles all apply). The password is the token; the
/// database user comes back as the identity.
pub async fn redshift_cli(profile: Option<&str>, target: &RedshiftTarget) -> Result<TokenSet, AuthError> {
    let args = redshift_args(profile, target)?;
    let shown = format!("aws {} {}", args[0], args[1]);
    let out = tokio::process::Command::new(aws_program())
        .args(&args)
        .output()
        .await
        .map_err(|e| AuthError::NotFound(format!("AWS CLI not available ({e}); install it and run `aws configure` or `aws sso login`")))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        let login = match profile.filter(|p| !p.trim().is_empty()) {
            Some(p) => format!("aws sso login --profile {}", p.trim()),
            None => "aws sso login".into(),
        };
        let hint = if err.contains("SSO") || err.contains("expired") || err.contains("Unable to locate credentials") || err.contains("refresh") {
            format!(" Sign in again with `{login}` (or `aws configure`), then connect.")
        } else {
            String::new()
        };
        return Err(AuthError::Flow(format!("`{shown}` failed: {err}.{hint}")));
    }
    parse_redshift_credentials(&out.stdout)
}

/// Access token from `az account get-access-token`.
pub async fn azure_cli(resource: &str) -> Result<TokenSet, AuthError> {
    let out = tokio::process::Command::new("az")
        .args(["account", "get-access-token", "--resource", resource, "--output", "json"])
        .output()
        .await
        .map_err(|e| AuthError::NotFound(format!("Azure CLI not available ({e}); install it and run `az login`")))?;
    if !out.status.success() {
        return Err(AuthError::Flow(format!("`az account get-access-token` failed: {}", String::from_utf8_lossy(&out.stderr).trim())));
    }
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).map_err(|e| AuthError::Flow(e.to_string()))?;
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
    fn redshift_cli_args_and_output() {
        let t = RedshiftTarget::from_host("analytics.c1ab2cd3ef4g.eu-west-1.redshift.amazonaws.com");
        assert_eq!((t.cluster.as_deref(), t.region.as_deref(), t.workgroup.as_deref()), (Some("analytics"), Some("eu-west-1"), None));
        let s = RedshiftTarget::from_host("wg1.123456789012.us-east-1.redshift-serverless.amazonaws.com");
        assert_eq!((s.workgroup.as_deref(), s.region.as_deref()), (Some("wg1"), Some("us-east-1")));
        assert_eq!(RedshiftTarget::from_host("db.local"), RedshiftTarget::default());
        let a = redshift_args(Some("prod"), &RedshiftTarget { database: "sales".into(), ..t.clone() }).unwrap();
        assert_eq!(
            a.join(" "),
            "redshift get-cluster-credentials-with-iam --cluster-identifier analytics --db-name sales --duration-seconds 3600 --output json --region eu-west-1 --profile prod"
        );
        let a = redshift_args(None, &RedshiftTarget { db_user: Some("etl".into()), ..t }).unwrap();
        assert!(a.join(" ").starts_with("redshift get-cluster-credentials --cluster-identifier analytics --db-user etl --db-name dev"));
        assert!(redshift_args(None, &s).unwrap().join(" ").starts_with("redshift-serverless get-credentials --workgroup-name wg1 --db-name dev"));
        assert!(matches!(redshift_args(None, &RedshiftTarget::default()), Err(AuthError::Invalid(_))));

        let t = parse_redshift_credentials(
            br#"{"DbUser": "IAM:alice", "DbPassword": "pw", "Expiration": "2026-11-01T12:00:00+00:00", "NextRefreshTime": "2026-11-01T23:00:00+00:00"}"#,
        )
        .unwrap();
        assert_eq!((t.access_token.as_str(), t.identity.as_deref(), t.expires_at), ("pw", Some("IAM:alice"), Some(1_793_534_400)));
        let s = parse_redshift_credentials(br#"{"dbUser": "IAMR:role", "dbPassword": "pw2", "expiration": "2026-11-01T13:30:00.5-01:30"}"#).unwrap();
        assert_eq!((s.identity.as_deref(), s.expires_at), (Some("IAMR:role"), Some(1_793_534_400 + 3 * 3600)));
        assert!(parse_redshift_credentials(b"{}").is_err());
    }

    #[test]
    fn parses_databrickscfg() {
        let p = parse_databrickscfg(
            "[DEFAULT]\nhost = https://adb-1.azuredatabricks.net/\ntoken = dapi123\n\n# c\n[oauth]\nhost=https://x.cloud.databricks.com\nauth_type = databricks-cli\n",
        );
        assert_eq!(p.len(), 2);
        assert_eq!(p[0].token.as_deref(), Some("dapi123"));
        assert_eq!(p[1].auth_type.as_deref(), Some("databricks-cli"));
        assert_eq!(normalize_host(p[0].host.as_deref().unwrap()), "adb-1.azuredatabricks.net");
    }
}
