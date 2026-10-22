//! List the buckets (S3, R2, GCS, S3-compatible) or containers (Azure) of
//! an account, which DuckDB cannot do: one signed HTTP request to the
//! storage's own API (S3 `ListBuckets` with SigV4, Azure `List Containers`
//! with Shared Key, a SAS token or an Entra ID token).

use std::time::Duration;

use base64::Engine as _;
use hmac::{Hmac, Mac};
use serde::Serialize;
use sha2::{Digest, Sha256};

use super::{Provider, StorageAuth, StorageConfig, connection_string_parts};
use databrain_connector_core::{ConnectorError, ErrorKind, Result};

/// A bucket or container.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Bucket {
    pub name: String,
    /// Creation (S3) or last-modified (Azure) time, as the service writes it.
    pub created: Option<String>,
}

/// AWS-style keys for one request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AwsKeys {
    pub id: String,
    pub secret: String,
    pub token: Option<String>,
}

fn err(msg: impl Into<String>) -> ConnectorError {
    ConnectorError::new(ErrorKind::Config, msg)
}

fn client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .connect_timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| err(e.to_string()))
}

/// Buckets of the account. `secret` is (key id, secret) as for
/// [`StorageConfig::secret_sql`].
pub async fn list_buckets(st: &StorageConfig, secret: Option<(&str, &str)>) -> Result<Vec<Bucket>> {
    if st.auth == StorageAuth::Anonymous {
        return Err(err("Public buckets can't be listed without signing in: type the bucket name"));
    }
    if st.provider == Provider::Azure {
        return list_containers(st, secret).await;
    }
    let keys = match &st.auth {
        StorageAuth::Keys => {
            let (id, s) = secret.filter(|(id, s)| !id.trim().is_empty() && !s.is_empty()).ok_or_else(|| err("Enter the access key id and the secret access key first"))?;
            AwsKeys { id: id.trim().to_string(), secret: s.to_string(), token: None }
        }
        StorageAuth::CredentialChain { profile } => aws_chain_keys(profile.as_deref()).await?,
        _ => return Err(err("This sign-in can't list buckets")),
    };
    let (host, region) = match st.provider {
        Provider::S3 => (st.endpoint.clone().unwrap_or_else(|| "s3.amazonaws.com".into()), st.region.clone().unwrap_or_else(|| "us-east-1".into())),
        Provider::S3Compatible => (st.endpoint.clone().unwrap_or_default(), st.region.clone().unwrap_or_else(|| "us-east-1".into())),
        Provider::R2 => (st.r2_endpoint(), "auto".into()),
        Provider::Gcs => (st.endpoint.clone().unwrap_or_else(|| "storage.googleapis.com".into()), "auto".into()),
        Provider::Azure => unreachable!(),
    };
    // A path in the endpoint is kept (servers behind a reverse proxy).
    let (authority, base_path) = match host.split_once('/') {
        Some((a, p)) => (a.to_string(), format!("/{}", p.trim_matches('/'))),
        None => (host.clone(), String::new()),
    };
    let path = if base_path.is_empty() { "/".to_string() } else { format!("{base_path}/") };
    let scheme = if st.use_ssl { "https" } else { "http" };
    let now = chrono::Utc::now();
    let headers = sigv4_headers("GET", &authority, &path, "", &region, &keys, now);
    let mut req = client()?.get(format!("{scheme}://{authority}{path}"));
    for (k, v) in &headers {
        if k != "host" {
            req = req.header(k.as_str(), v.as_str());
        }
    }
    let resp = req.send().await.map_err(|e| err(format!("cannot reach {authority}: {e}")))?;
    let status = resp.status();
    let body = resp.text().await.map_err(|e| err(e.to_string()))?;
    if !status.is_success() {
        return Err(err(format!("listing buckets failed ({status}): {}", xml_error(&body))));
    }
    Ok(blocks(&body, "Bucket").into_iter().filter_map(|b| Some(Bucket { name: tag(b, "Name")?, created: tag(b, "CreationDate") })).collect())
}

/// Containers of an Azure storage account (all pages, up to 100 000).
async fn list_containers(st: &StorageConfig, secret: Option<(&str, &str)>) -> Result<Vec<Bucket>> {
    let secret = secret.map(|(_, s)| s).filter(|s| !s.is_empty());
    let endpoint = st.azure_blob_endpoint(secret).ok_or_else(|| err("Enter the storage account (or a connection string with AccountName or BlobEndpoint)"))?;
    // How each request is signed.
    enum Sign {
        Key { account: String, key: Vec<u8> },
        Sas(String),
        Bearer(String),
    }
    let decode_key = |k: &str| base64::engine::general_purpose::STANDARD.decode(k.trim()).map_err(|_| err("The storage account key is not valid base64"));
    let sign = match &st.auth {
        StorageAuth::Keys => Sign::Key {
            account: st.account.clone().unwrap_or_default(),
            key: decode_key(secret.ok_or_else(|| err("Enter the storage account key first"))?)?,
        },
        StorageAuth::Sas => Sign::Sas(secret.ok_or_else(|| err("Enter the SAS token first"))?.trim_start_matches('?').to_string()),
        StorageAuth::ConnectionString => {
            let parts = connection_string_parts(secret.ok_or_else(|| err("Enter the connection string first"))?);
            match (parts.get("sharedaccesssignature"), parts.get("accountname"), parts.get("accountkey")) {
                (Some(sas), _, _) => Sign::Sas(sas.trim_start_matches('?').to_string()),
                (None, Some(a), Some(k)) => Sign::Key { account: a.clone(), key: decode_key(k)? },
                _ => return Err(err("The connection string has neither AccountName/AccountKey nor SharedAccessSignature")),
            }
        }
        StorageAuth::ServicePrincipal { tenant, client_id } => Sign::Bearer(entra_token(tenant, client_id, secret.ok_or_else(|| err("Enter the client secret first"))?).await?),
        StorageAuth::CredentialChain { .. } => Sign::Bearer(az_cli_token().await?),
        StorageAuth::Anonymous => unreachable!(),
    };
    let url = reqwest::Url::parse(&endpoint).map_err(|e| err(format!("bad endpoint {endpoint}: {e}")))?;
    let path = if url.path().ends_with('/') { url.path().to_string() } else { format!("{}/", url.path()) };
    let http = client()?;
    let mut out = Vec::new();
    let mut marker: Option<String> = None;
    for _ in 0..20 {
        let mut query = vec![("comp".to_string(), "list".to_string()), ("maxresults".to_string(), "5000".to_string())];
        if let Some(m) = &marker {
            query.push(("marker".into(), m.clone()));
        }
        let date = httpdate(chrono::Utc::now());
        let qs = query.iter().map(|(k, v)| format!("{k}={}", enc(v))).collect::<Vec<_>>().join("&");
        let full = match &sign {
            Sign::Sas(sas) => format!("{}://{}{path}?{qs}&{sas}", url.scheme(), url_authority(&url)),
            _ => format!("{}://{}{path}?{qs}", url.scheme(), url_authority(&url)),
        };
        let mut req = http.get(&full).header("x-ms-date", &date).header("x-ms-version", "2021-08-06");
        match &sign {
            Sign::Key { account, key } => {
                let canon_headers = format!("x-ms-date:{date}\nx-ms-version:2021-08-06\n");
                let mut sorted = query.clone();
                sorted.sort();
                let canon_query: String = sorted.iter().map(|(k, v)| format!("\n{k}:{v}")).collect();
                let string_to_sign = format!("GET\n\n\n\n\n\n\n\n\n\n\n\n{canon_headers}/{account}{path}{canon_query}");
                let mut mac = Hmac::<Sha256>::new_from_slice(key).map_err(|e| err(e.to_string()))?;
                mac.update(string_to_sign.as_bytes());
                let sig = base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes());
                req = req.header("Authorization", format!("SharedKey {account}:{sig}"));
            }
            Sign::Bearer(t) => req = req.bearer_auth(t),
            Sign::Sas(_) => {}
        }
        let resp = req.send().await.map_err(|e| err(format!("cannot reach {}: {e}", url_authority(&url))))?;
        let status = resp.status();
        let body = resp.text().await.map_err(|e| err(e.to_string()))?;
        if !status.is_success() {
            return Err(err(format!("listing containers failed ({status}): {}", xml_error(&body))));
        }
        out.extend(blocks(&body, "Container").into_iter().filter_map(|b| Some(Bucket { name: tag(b, "Name")?, created: tag(b, "Last-Modified") })));
        marker = tag(&body, "NextMarker").filter(|m| !m.is_empty());
        if marker.is_none() {
            break;
        }
    }
    Ok(out)
}

fn url_authority(u: &reqwest::Url) -> String {
    match u.port() {
        Some(p) => format!("{}:{p}", u.host_str().unwrap_or_default()),
        None => u.host_str().unwrap_or_default().to_string(),
    }
}

/// Entra ID token for Azure Storage with client credentials.
async fn entra_token(tenant: &str, client_id: &str, client_secret: &str) -> Result<String> {
    let resp = client()?
        .post(format!("https://login.microsoftonline.com/{}/oauth2/v2.0/token", enc(tenant)))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(format!("grant_type=client_credentials&client_id={}&client_secret={}&scope={}", enc(client_id), enc(client_secret), enc("https://storage.azure.com/.default")))
        .send()
        .await
        .map_err(|e| err(format!("Entra ID sign-in failed: {e}")))?;
    let v: serde_json::Value = resp.json().await.map_err(|e| err(format!("Entra ID sign-in failed: {e}")))?;
    v.get("access_token").and_then(|t| t.as_str()).map(str::to_string).ok_or_else(|| {
        err(format!("Entra ID sign-in failed: {}", v.get("error_description").and_then(|d| d.as_str()).unwrap_or("no token returned")))
    })
}

/// Token from the Azure CLI login (`az login`).
async fn az_cli_token() -> Result<String> {
    let out = tokio::process::Command::new(if cfg!(windows) { "az.cmd" } else { "az" })
        .args(["account", "get-access-token", "--resource", "https://storage.azure.com/", "-o", "json"])
        .output()
        .await
        .map_err(|_| err("Azure CLI not found: install it and run `az login`, or type the container name"))?;
    if !out.status.success() {
        return Err(err(format!("Azure CLI: {} (run `az login`)", String::from_utf8_lossy(&out.stderr).trim())));
    }
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).map_err(|e| err(format!("Azure CLI: {e}")))?;
    v.get("accessToken").and_then(|t| t.as_str()).map(str::to_string).ok_or_else(|| err("Azure CLI returned no token"))
}

/// Keys of the AWS credential chain, for one request: environment
/// variables, then `~/.aws/credentials` (the profile, `AWS_PROFILE` or
/// `default`), then the AWS CLI (`aws configure export-credentials`, which
/// also resolves SSO and roles).
pub async fn aws_chain_keys(profile: Option<&str>) -> Result<AwsKeys> {
    if profile.is_none() {
        if let (Ok(id), Ok(secret)) = (std::env::var("AWS_ACCESS_KEY_ID"), std::env::var("AWS_SECRET_ACCESS_KEY")) {
            if !id.is_empty() && !secret.is_empty() {
                return Ok(AwsKeys { id, secret, token: std::env::var("AWS_SESSION_TOKEN").ok().filter(|t| !t.is_empty()) });
            }
        }
    }
    let name = profile.map(str::to_string).or_else(|| std::env::var("AWS_PROFILE").ok()).unwrap_or_else(|| "default".into());
    let file = std::env::var("AWS_SHARED_CREDENTIALS_FILE").map(std::path::PathBuf::from).ok().or_else(|| dirs::home_dir().map(|h| h.join(".aws").join("credentials")));
    if let Some(text) = file.and_then(|f| std::fs::read_to_string(f).ok()) {
        if let Some(k) = credentials_profile(&text, &name) {
            return Ok(k);
        }
    }
    let out = tokio::process::Command::new(if cfg!(windows) { "aws.exe" } else { "aws" })
        .args(["configure", "export-credentials", "--format", "process", "--profile", &name])
        .output()
        .await
        .map_err(|_| err(format!("No AWS keys found for profile \"{name}\" (environment, ~/.aws/credentials or the AWS CLI); type the bucket name")))?;
    if !out.status.success() {
        return Err(err(format!("AWS CLI: {}", String::from_utf8_lossy(&out.stderr).trim())));
    }
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).map_err(|e| err(format!("AWS CLI: {e}")))?;
    let s = |k: &str| v.get(k).and_then(|x| x.as_str()).map(str::to_string);
    Ok(AwsKeys {
        id: s("AccessKeyId").ok_or_else(|| err("AWS CLI returned no keys"))?,
        secret: s("SecretAccessKey").ok_or_else(|| err("AWS CLI returned no keys"))?,
        token: s("SessionToken"),
    })
}

/// Keys of `[profile]` in an AWS credentials file.
fn credentials_profile(text: &str, profile: &str) -> Option<AwsKeys> {
    let mut current = String::new();
    let (mut id, mut secret, mut token) = (None, None, None);
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') && line.ends_with(']') {
            current = line[1..line.len() - 1].trim().trim_start_matches("profile ").to_string();
            continue;
        }
        if current != profile {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            let v = v.trim().to_string();
            match k.trim() {
                "aws_access_key_id" => id = Some(v),
                "aws_secret_access_key" => secret = Some(v),
                "aws_session_token" => token = Some(v),
                _ => {}
            }
        }
    }
    Some(AwsKeys { id: id?, secret: secret?, token })
}

/// Percent-encoding for query values and form fields (RFC 3986 unreserved kept).
fn enc(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn httpdate(t: chrono::DateTime<chrono::Utc>) -> String {
    t.format("%a, %d %b %Y %H:%M:%S GMT").to_string()
}

fn hmac256(key: &[u8], data: &str) -> Vec<u8> {
    let mut m = Hmac::<Sha256>::new_from_slice(key).expect("any key length");
    m.update(data.as_bytes());
    m.finalize().into_bytes().to_vec()
}

/// AWS Signature V4 headers for a request without a body (`host`,
/// `x-amz-date`, `x-amz-content-sha256`, `authorization`, and
/// `x-amz-security-token` for temporary keys).
pub fn sigv4_headers(method: &str, host: &str, path: &str, query: &str, region: &str, keys: &AwsKeys, now: chrono::DateTime<chrono::Utc>) -> Vec<(String, String)> {
    let amz_date = now.format("%Y%m%dT%H%M%SZ").to_string();
    let day = now.format("%Y%m%d").to_string();
    let payload = hex::encode(Sha256::digest(b""));
    let mut headers = vec![("host".to_string(), host.to_string()), ("x-amz-content-sha256".to_string(), payload.clone()), ("x-amz-date".to_string(), amz_date.clone())];
    if let Some(t) = &keys.token {
        headers.push(("x-amz-security-token".into(), t.clone()));
    }
    headers.sort();
    let canon_headers: String = headers.iter().map(|(k, v)| format!("{k}:{}\n", v.trim())).collect();
    let signed = headers.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>().join(";");
    let canonical = format!("{method}\n{path}\n{query}\n{canon_headers}\n{signed}\n{payload}");
    let scope = format!("{day}/{region}/s3/aws4_request");
    let to_sign = format!("AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}", hex::encode(Sha256::digest(canonical.as_bytes())));
    let k = hmac256(format!("AWS4{}", keys.secret).as_bytes(), &day);
    let k = hmac256(&k, region);
    let k = hmac256(&k, "s3");
    let k = hmac256(&k, "aws4_request");
    let sig = hex::encode(hmac256(&k, &to_sign));
    headers.push(("authorization".into(), format!("AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed}, Signature={sig}", keys.id)));
    headers
}

/// Inner text of every `<tag>…</tag>` block.
fn blocks<'a>(xml: &'a str, tag_name: &str) -> Vec<&'a str> {
    let (open, close) = (format!("<{tag_name}>"), format!("</{tag_name}>"));
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(i) = rest.find(&open) {
        let after = &rest[i + open.len()..];
        let Some(j) = after.find(&close) else { break };
        out.push(&after[..j]);
        rest = &after[j + close.len()..];
    }
    out
}

/// Text of the first `<tag>…</tag>`, XML entities decoded.
fn tag(xml: &str, tag_name: &str) -> Option<String> {
    blocks(xml, tag_name).first().map(|v| v.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'").replace("&amp;", "&"))
}

/// Code and message of an S3/Azure XML error body.
fn xml_error(body: &str) -> String {
    match (tag(body, "Code"), tag(body, "Message")) {
        (Some(c), Some(m)) => format!("{c}: {}", m.lines().next().unwrap_or_default()),
        (Some(c), None) => c,
        _ => body.chars().take(300).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn sigv4_matches_aws_example() {
        // AWS's documented GET Bucket example keys and date.
        let keys = AwsKeys { id: "AKIAIOSFODNN7EXAMPLE".into(), secret: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".into(), token: None };
        let t = chrono::Utc.with_ymd_and_hms(2013, 5, 24, 0, 0, 0).unwrap();
        let h = sigv4_headers("GET", "examplebucket.s3.amazonaws.com", "/", "", "us-east-1", &keys, t);
        let auth = &h.iter().find(|(k, _)| k == "authorization").unwrap().1;
        assert!(auth.starts_with("AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request, SignedHeaders=host;x-amz-content-sha256;x-amz-date, Signature="), "{auth}");
        assert_eq!(auth.rsplit('=').next().unwrap().len(), 64);
        let with_token = sigv4_headers("GET", "h", "/", "", "us-east-1", &AwsKeys { token: Some("T".into()), ..keys }, t);
        assert!(with_token.iter().any(|(k, v)| k == "x-amz-security-token" && v == "T"));
    }

    #[test]
    fn parses_listings_and_errors() {
        let s3 = "<ListAllMyBucketsResult><Buckets><Bucket><Name>a&amp;b</Name><CreationDate>2024-01-01T00:00:00Z</CreationDate></Bucket><Bucket><Name>logs</Name></Bucket></Buckets></ListAllMyBucketsResult>";
        let v: Vec<_> = blocks(s3, "Bucket").into_iter().filter_map(|b| tag(b, "Name")).collect();
        assert_eq!(v, ["a&b", "logs"]);
        assert_eq!(xml_error("<Error><Code>AccessDenied</Code><Message>Access Denied</Message></Error>"), "AccessDenied: Access Denied");
    }

    #[test]
    fn aws_credentials_file() {
        let text = "[default]\naws_access_key_id = A\naws_secret_access_key = B\n\n[profile dev]\naws_access_key_id=C\naws_secret_access_key=D\naws_session_token=E\n";
        assert_eq!(credentials_profile(text, "default"), Some(AwsKeys { id: "A".into(), secret: "B".into(), token: None }));
        assert_eq!(credentials_profile(text, "dev").unwrap().token.as_deref(), Some("E"));
        assert_eq!(credentials_profile(text, "nope"), None);
    }

    #[tokio::test]
    async fn anonymous_cannot_list() {
        let st = StorageConfig { provider: Provider::S3, auth: StorageAuth::Anonymous, region: None, endpoint: None, account_id: None, account: None, url_style: None, use_ssl: true, bucket: None };
        assert!(list_buckets(&st, None).await.unwrap_err().message.contains("type the bucket name"));
    }
}
