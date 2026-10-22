//! Object storage for DuckDB connections: S3, Cloudflare R2, Google Cloud
//! Storage (HMAC keys), S3-compatible servers (MinIO, Wasabi, Backblaze B2,
//! DigitalOcean Spaces, Ceph…) through DuckDB's httpfs extension, and Azure
//! Blob Storage / ADLS Gen2 through its azure extension.
//!
//! The connection's options say where the storage is (`storage`,
//! `storage_region`, `storage_endpoint`, `storage_account`, …). The secret
//! (secret access key, Azure account key, SAS token, connection string or
//! client secret) is the connection's keychain secret; an S3 access key id
//! is the auth user. Each session gets them as a TEMPORARY DuckDB secret
//! (memory only, never written to a database file), so
//! `SELECT * FROM 's3://bucket/x.parquet'` just works.

use databrain_connector_core::{ConnectionConfig, ConnectorError, Result, quote_literal};

pub mod buckets;

/// Which service the connection reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    /// Amazon S3.
    S3,
    /// Cloudflare R2 (`r2://` and `s3://` URLs).
    R2,
    /// Google Cloud Storage with HMAC keys (`gs://`, `gcs://`).
    Gcs,
    /// Any S3-compatible server at a custom endpoint (MinIO, Wasabi, B2…).
    S3Compatible,
    /// Azure Blob Storage and ADLS Gen2 (`az://container/…`, `abfss://container@account.dfs.core.windows.net/…`).
    Azure,
}

impl Provider {
    fn parse(s: &str) -> Option<Self> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "s3" | "aws" => Provider::S3,
            "r2" => Provider::R2,
            "gcs" | "gs" => Provider::Gcs,
            "s3_compatible" | "minio" | "custom" => Provider::S3Compatible,
            "azure" | "az" | "adls" => Provider::Azure,
            _ => return None,
        })
    }

    /// URL scheme of its paths.
    pub fn scheme(self) -> &'static str {
        match self {
            Provider::Gcs => "gs",
            Provider::R2 => "r2",
            Provider::Azure => "az",
            _ => "s3",
        }
    }

    pub fn is_s3_api(self) -> bool {
        self != Provider::Azure
    }
}

/// How the session signs its requests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StorageAuth {
    /// S3 family: access key id (auth user) + secret access key (keychain).
    /// Azure: the storage account key (keychain).
    Keys,
    /// AWS SDK credential chain (environment, `~/.aws` profiles, SSO,
    /// instance role), or for Azure the Azure CLI login (`az login`),
    /// environment and managed identity.
    CredentialChain { profile: Option<String> },
    /// Azure: a full connection string (keychain).
    ConnectionString,
    /// Azure: a SAS token (keychain).
    Sas,
    /// Azure: an Entra ID app (tenant, client id; client secret in the keychain).
    ServicePrincipal { tenant: String, client_id: String },
    /// Public buckets: unsigned requests.
    Anonymous,
}

impl StorageAuth {
    /// The connection's keychain secret is used.
    pub fn uses_secret(&self) -> bool {
        matches!(self, StorageAuth::Keys | StorageAuth::ConnectionString | StorageAuth::Sas | StorageAuth::ServicePrincipal { .. })
    }
}

/// Object storage settings of a connection (no secrets).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageConfig {
    pub provider: Provider,
    pub auth: StorageAuth,
    pub region: Option<String>,
    /// Host[:port][/path] without scheme (custom endpoints only).
    pub endpoint: Option<String>,
    /// Cloudflare account id (R2).
    pub account_id: Option<String>,
    /// Azure storage account name.
    pub account: Option<String>,
    /// `path` or `vhost`.
    pub url_style: Option<String>,
    pub use_ssl: bool,
    /// Bucket (and optional prefix) checked by Test and opened first when browsing.
    pub bucket: Option<String>,
}

/// DuckDB secret names (one per URL scheme they cover).
pub const SECRET_NAME: &str = "databrain_storage";

/// `s3://…`, `r2://…`, `gs://…`, `az://…`, `https://…` and other non-local paths.
pub fn is_remote(path: &str) -> bool {
    let p = path.trim();
    match p.find("://") {
        // `C://x` is not a URL; schemes are at least two letters.
        Some(i) => i >= 2 && p[..i].chars().all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '-' || c == '.'),
        None => false,
    }
}

fn opt(cfg: &ConnectionConfig, key: &str) -> Option<String> {
    cfg.opt(key).map(str::trim).filter(|v| !v.is_empty()).map(str::to_string)
}

/// `Key=Value;…` parts of an Azure connection string (keys lowercased).
pub fn connection_string_parts(cs: &str) -> std::collections::HashMap<String, String> {
    cs.split(';')
        .filter_map(|p| p.split_once('='))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect()
}

impl StorageConfig {
    /// The connection's object storage, or `None` when it has none.
    pub fn from_config(cfg: &ConnectionConfig) -> Result<Option<Self>> {
        let Some(kind) = opt(cfg, "storage") else { return Ok(None) };
        let provider = Provider::parse(&kind).ok_or_else(|| ConnectorError::config(format!("unknown object storage \"{kind}\" (use s3, r2, gcs, s3_compatible or azure)")))?;
        let auth = match opt(cfg, "storage_auth").as_deref() {
            Some("anonymous") | Some("none") => StorageAuth::Anonymous,
            Some("chain") | Some("credential_chain") | Some("cli") => StorageAuth::CredentialChain { profile: opt(cfg, "storage_profile") },
            Some("connection_string") => StorageAuth::ConnectionString,
            Some("sas") => StorageAuth::Sas,
            Some("service_principal") => StorageAuth::ServicePrincipal {
                tenant: opt(cfg, "storage_tenant").ok_or_else(|| ConnectorError::config("Azure service principal: enter the tenant (directory) id"))?,
                client_id: opt(cfg, "storage_client_id").ok_or_else(|| ConnectorError::config("Azure service principal: enter the client (application) id"))?,
            },
            _ => StorageAuth::Keys,
        };
        match (&auth, provider) {
            (StorageAuth::CredentialChain { .. }, Provider::R2 | Provider::Gcs) => {
                return Err(ConnectorError::config("the AWS credential chain only works for S3 and S3-compatible storage; use access keys"));
            }
            (StorageAuth::ConnectionString | StorageAuth::Sas | StorageAuth::ServicePrincipal { .. }, p) if p != Provider::Azure => {
                return Err(ConnectorError::config("connection strings, SAS tokens and service principals are for Azure storage"));
            }
            _ => {}
        }
        // Endpoints are written as URLs or host:port; `http://` turns TLS off.
        let mut use_ssl = !matches!(opt(cfg, "storage_use_ssl").as_deref(), Some("false") | Some("0") | Some("off"));
        let endpoint = opt(cfg, "storage_endpoint").map(|e| {
            let lower = e.to_ascii_lowercase();
            if lower.starts_with("http://") {
                use_ssl = false;
            }
            let host = e.split_once("://").map(|(_, h)| h).unwrap_or(&e);
            host.trim_end_matches('/').to_string()
        });
        if provider == Provider::S3Compatible && endpoint.is_none() {
            return Err(ConnectorError::config("S3-compatible storage needs an endpoint (e.g. localhost:9000 for MinIO)"));
        }
        let account_id = opt(cfg, "storage_account_id");
        if provider == Provider::R2 && account_id.is_none() && endpoint.is_none() {
            return Err(ConnectorError::config("Cloudflare R2 needs the account id (Cloudflare dashboard → R2 → Account ID)"));
        }
        let account = opt(cfg, "storage_account");
        if provider == Provider::Azure && account.is_none() && auth != StorageAuth::ConnectionString {
            return Err(ConnectorError::config("Azure storage needs the storage account name"));
        }
        let url_style = opt(cfg, "storage_url_style").map(|s| s.to_ascii_lowercase()).filter(|s| s == "path" || s == "vhost");
        let bucket = opt(cfg, "storage_bucket").map(|b| {
            // Accept `bucket`, `bucket/prefix` or a full URL.
            let b = b.split_once("://").map(|(_, r)| r.to_string()).unwrap_or(b);
            b.trim_matches('/').to_string()
        });
        Ok(Some(StorageConfig { provider, auth, region: opt(cfg, "storage_region"), endpoint, account_id, account, url_style, use_ssl, bucket }))
    }

    /// `s3://bucket/prefix/` for the configured bucket.
    pub fn root_url(&self) -> Option<String> {
        self.bucket.as_ref().map(|b| format!("{}://{b}/", self.provider.scheme()))
    }

    /// Extensions the secret needs (httpfs; aws for the AWS credential chain; azure).
    pub fn extensions(&self) -> &'static [&'static str] {
        match (self.provider, &self.auth) {
            (Provider::Azure, _) => &["azure"],
            (_, StorageAuth::CredentialChain { .. }) => &["httpfs", "aws"],
            _ => &["httpfs"],
        }
    }

    /// Azure Blob endpoint, e.g. `https://acct.blob.core.windows.net` or
    /// `http://127.0.0.1:10000/devstoreaccount1` (Azurite). `secret` is the
    /// connection string for [`StorageAuth::ConnectionString`].
    pub fn azure_blob_endpoint(&self, secret: Option<&str>) -> Option<String> {
        if self.auth == StorageAuth::ConnectionString {
            let parts = connection_string_parts(secret?);
            if let Some(e) = parts.get("blobendpoint") {
                return Some(e.trim_end_matches('/').to_string());
            }
            let account = parts.get("accountname")?;
            let proto = parts.get("defaultendpointsprotocol").map(String::as_str).unwrap_or("https");
            let suffix = parts.get("endpointsuffix").map(String::as_str).unwrap_or("core.windows.net");
            return Some(format!("{proto}://{account}.blob.{suffix}"));
        }
        let scheme = if self.use_ssl { "https" } else { "http" };
        match &self.endpoint {
            Some(e) => Some(format!("{scheme}://{e}")),
            None => Some(format!("https://{}.blob.core.windows.net", self.account.as_deref()?)),
        }
    }

    /// `CREATE TEMPORARY SECRET` statements. `secret` is (key id, secret):
    /// the access key id and secret access key for the S3 family, or ("", the
    /// account key / connection string / SAS token / client secret) for Azure.
    pub fn secret_sql(&self, secret: Option<(&str, &str)>) -> Result<Vec<String>> {
        let lit = |s: &str| quote_literal(s);
        let need = |what: &str| -> Result<(&str, &str)> {
            match secret {
                Some((id, s)) if !s.is_empty() => Ok((id, s)),
                _ => Err(ConnectorError::config(format!("object storage: enter the {what}"))),
            }
        };
        if self.provider == Provider::Azure {
            let account = self.account.clone().unwrap_or_default();
            let mut params: Vec<(&str, String)> = Vec::new();
            match &self.auth {
                StorageAuth::Keys => {
                    let (_, key) = need("storage account key")?;
                    let endpoint = self.azure_blob_endpoint(None).unwrap_or_default();
                    let proto = if self.use_ssl { "https" } else { "http" };
                    let cs = if self.endpoint.is_some() {
                        format!("DefaultEndpointsProtocol={proto};AccountName={account};AccountKey={key};BlobEndpoint={endpoint};")
                    } else {
                        format!("DefaultEndpointsProtocol=https;AccountName={account};AccountKey={key};EndpointSuffix=core.windows.net")
                    };
                    params.push(("CONNECTION_STRING", lit(&cs)));
                }
                StorageAuth::ConnectionString => params.push(("CONNECTION_STRING", lit(need("connection string")?.1))),
                StorageAuth::Sas => {
                    let (_, sas) = need("SAS token")?;
                    let endpoint = self.azure_blob_endpoint(None).unwrap_or_default();
                    params.push(("CONNECTION_STRING", lit(&format!("BlobEndpoint={endpoint};SharedAccessSignature={}", sas.trim_start_matches('?')))));
                }
                StorageAuth::ServicePrincipal { tenant, client_id } => {
                    let (_, cs) = need("client secret")?;
                    params.push(("PROVIDER", "service_principal".into()));
                    params.push(("TENANT_ID", lit(tenant)));
                    params.push(("CLIENT_ID", lit(client_id)));
                    params.push(("CLIENT_SECRET", lit(cs)));
                    params.push(("ACCOUNT_NAME", lit(&account)));
                }
                StorageAuth::CredentialChain { .. } => {
                    params.push(("PROVIDER", "credential_chain".into()));
                    params.push(("CHAIN", lit("cli;env;managed_identity")));
                    params.push(("ACCOUNT_NAME", lit(&account)));
                }
                StorageAuth::Anonymous => {
                    params.push(("PROVIDER", "config".into()));
                    params.push(("ACCOUNT_NAME", lit(&account)));
                }
            }
            let body = std::iter::once(("TYPE", "azure".to_string())).chain(params).map(|(k, v)| format!("{k} {v}")).collect::<Vec<_>>().join(", ");
            return Ok(vec![format!("CREATE OR REPLACE TEMPORARY SECRET {SECRET_NAME} ({body})")]);
        }
        let mut params: Vec<(&str, String)> = Vec::new();
        match &self.auth {
            StorageAuth::Keys => {
                let (id, secret) = secret.ok_or_else(|| ConnectorError::config("object storage: enter the access key id and secret access key (or choose public bucket / AWS credential chain)"))?;
                if id.trim().is_empty() || secret.is_empty() {
                    return Err(ConnectorError::config("object storage: the access key id and the secret access key are both required"));
                }
                params.push(("KEY_ID", lit(id.trim())));
                params.push(("SECRET", lit(secret)));
            }
            StorageAuth::CredentialChain { profile } => {
                params.push(("PROVIDER", "credential_chain".into()));
                if let Some(p) = profile {
                    params.push(("PROFILE", lit(p)));
                }
            }
            _ => {}
        }
        let mut common = Vec::new();
        if let Some(r) = &self.region {
            common.push(("REGION", lit(r)));
        }
        if let Some(s) = &self.url_style {
            common.push(("URL_STYLE", lit(s)));
        }
        let build = |ty: &str, name: &str, extra: &[(&str, String)]| {
            let body = std::iter::once(("TYPE", ty.to_string()))
                .chain(params.iter().cloned())
                .chain(extra.iter().cloned())
                .map(|(k, v)| format!("{k} {v}"))
                .collect::<Vec<_>>()
                .join(", ");
            format!("CREATE OR REPLACE TEMPORARY SECRET {name} ({body})")
        };
        Ok(match self.provider {
            Provider::S3 | Provider::S3Compatible => {
                let mut extra = common.clone();
                if let Some(e) = &self.endpoint {
                    extra.push(("ENDPOINT", lit(e)));
                    // Custom servers (MinIO…) usually serve buckets as paths.
                    if self.url_style.is_none() && self.provider == Provider::S3Compatible {
                        extra.push(("URL_STYLE", lit("path")));
                    }
                }
                if !self.use_ssl {
                    extra.push(("USE_SSL", "false".into()));
                }
                vec![build("s3", SECRET_NAME, &extra)]
            }
            Provider::R2 => {
                // r2://bucket/… through DuckDB's R2 secret, and s3://bucket/…
                // through an S3 secret at the account's R2 endpoint.
                let endpoint = self.r2_endpoint();
                let mut r2 = Vec::new();
                if let Some(a) = &self.account_id {
                    r2.push(("ACCOUNT_ID", lit(a)));
                }
                let mut s3 = vec![("ENDPOINT", lit(&endpoint)), ("REGION", lit(self.region.as_deref().unwrap_or("auto")))];
                if let Some(s) = &self.url_style {
                    s3.push(("URL_STYLE", lit(s)));
                }
                let mut out = Vec::new();
                if self.account_id.is_some() {
                    out.push(build("r2", &format!("{SECRET_NAME}_r2"), &r2));
                }
                out.push(build("s3", SECRET_NAME, &s3));
                out
            }
            Provider::Gcs => vec![build("gcs", SECRET_NAME, &common)],
            Provider::Azure => unreachable!("handled above"),
        })
    }

    /// Host of the R2 S3 API for this account.
    pub fn r2_endpoint(&self) -> String {
        self.endpoint.clone().unwrap_or_else(|| format!("{}.r2.cloudflarestorage.com", self.account_id.as_deref().unwrap_or_default()))
    }
}

/// SQL listing up to `limit` objects under `url` (a prefix such as
/// `s3://bucket/data/`, or a glob): path, size in bytes, last modified.
/// Objects are listed recursively; the caller groups them into folders.
pub fn list_sql(url: &str, limit: usize) -> String {
    let u = url.trim();
    let pattern = if u.contains(['*', '?', '[']) {
        u.to_string()
    } else if u.ends_with('/') {
        format!("{u}**")
    } else {
        // A bucket or a folder written without the slash.
        format!("{u}/**")
    };
    format!(
        "SELECT filename, size, CAST(last_modified AS VARCHAR) FROM read_blob({}) ORDER BY filename LIMIT {limit}",
        quote_literal(&pattern)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use databrain_auth::AuthMethod;
    use databrain_connector_core::ConnectorKind;

    fn cfg(opts: &[(&str, &str)]) -> ConnectionConfig {
        let mut c = ConnectionConfig::new(ConnectorKind::Duckdb, AuthMethod::None);
        for (k, v) in opts {
            c.options.insert((*k).into(), (*v).into());
        }
        c
    }

    #[test]
    fn remote_paths() {
        for p in ["s3://b/x.parquet", "r2://b/x.csv", "gs://b/x", "https://example.com/a.csv", "az://c/x"] {
            assert!(is_remote(p), "{p}");
        }
        for p in ["/tmp/x.csv", "C:\\data\\x.csv", "C://x.csv", "data/x.parquet"] {
            assert!(!is_remote(p), "{p}");
        }
    }

    #[test]
    fn no_storage_without_the_option() {
        assert_eq!(StorageConfig::from_config(&cfg(&[])).unwrap(), None);
    }

    #[test]
    fn s3_keys_secret() {
        let s = StorageConfig::from_config(&cfg(&[("storage", "s3"), ("storage_region", "eu-west-1"), ("storage_bucket", "s3://my-bucket/raw/")])).unwrap().unwrap();
        assert_eq!(s.root_url().as_deref(), Some("s3://my-bucket/raw/"));
        let sql = s.secret_sql(Some(("AKIA1", "se'cret"))).unwrap();
        assert_eq!(sql, vec!["CREATE OR REPLACE TEMPORARY SECRET databrain_storage (TYPE s3, KEY_ID 'AKIA1', SECRET 'se''cret', REGION 'eu-west-1')"]);
        assert!(s.secret_sql(None).is_err(), "keys are required");
    }

    #[test]
    fn minio_endpoint_from_url() {
        let s = StorageConfig::from_config(&cfg(&[("storage", "s3_compatible"), ("storage_endpoint", "http://localhost:9000/")])).unwrap().unwrap();
        assert_eq!(s.endpoint.as_deref(), Some("localhost:9000"));
        assert!(!s.use_ssl);
        let sql = s.secret_sql(Some(("minio", "minio123"))).unwrap().remove(0);
        assert!(sql.contains("ENDPOINT 'localhost:9000'") && sql.contains("URL_STYLE 'path'") && sql.contains("USE_SSL false"), "{sql}");
        assert!(StorageConfig::from_config(&cfg(&[("storage", "s3_compatible")])).is_err(), "needs an endpoint");
    }

    #[test]
    fn r2_both_schemes() {
        let s = StorageConfig::from_config(&cfg(&[("storage", "r2"), ("storage_account_id", "abc123")])).unwrap().unwrap();
        let sql = s.secret_sql(Some(("k", "s"))).unwrap();
        assert_eq!(sql.len(), 2);
        assert!(sql[0].contains("TYPE r2") && sql[0].contains("ACCOUNT_ID 'abc123'"));
        assert!(sql[1].contains("TYPE s3") && sql[1].contains("ENDPOINT 'abc123.r2.cloudflarestorage.com'") && sql[1].contains("REGION 'auto'"));
        assert!(StorageConfig::from_config(&cfg(&[("storage", "r2")])).is_err(), "needs the account id");
    }

    #[test]
    fn gcs_chain_and_anonymous() {
        let g = StorageConfig::from_config(&cfg(&[("storage", "gcs")])).unwrap().unwrap();
        assert!(g.secret_sql(Some(("GOOG1", "s"))).unwrap()[0].starts_with("CREATE OR REPLACE TEMPORARY SECRET databrain_storage (TYPE gcs, KEY_ID"));
        assert!(StorageConfig::from_config(&cfg(&[("storage", "gcs"), ("storage_auth", "chain")])).is_err());
        let c = StorageConfig::from_config(&cfg(&[("storage", "s3"), ("storage_auth", "chain"), ("storage_profile", "dev")])).unwrap().unwrap();
        assert_eq!(c.extensions(), &["httpfs", "aws"]);
        assert_eq!(c.secret_sql(None).unwrap()[0], "CREATE OR REPLACE TEMPORARY SECRET databrain_storage (TYPE s3, PROVIDER credential_chain, PROFILE 'dev')");
        let a = StorageConfig::from_config(&cfg(&[("storage", "s3"), ("storage_auth", "anonymous"), ("storage_region", "us-east-1")])).unwrap().unwrap();
        assert_eq!(a.secret_sql(None).unwrap()[0], "CREATE OR REPLACE TEMPORARY SECRET databrain_storage (TYPE s3, REGION 'us-east-1')");
    }

    #[test]
    fn azure_secrets() {
        let k = StorageConfig::from_config(&cfg(&[("storage", "azure"), ("storage_account", "acct")])).unwrap().unwrap();
        assert_eq!(k.extensions(), &["azure"]);
        assert_eq!(k.root_url(), None);
        assert_eq!(
            k.secret_sql(Some(("", "KEY=="))).unwrap()[0],
            "CREATE OR REPLACE TEMPORARY SECRET databrain_storage (TYPE azure, CONNECTION_STRING 'DefaultEndpointsProtocol=https;AccountName=acct;AccountKey=KEY==;EndpointSuffix=core.windows.net')"
        );
        assert_eq!(k.azure_blob_endpoint(None).as_deref(), Some("https://acct.blob.core.windows.net"));
        // Azurite: endpoint with a path, http turns TLS off.
        let a = StorageConfig::from_config(&cfg(&[("storage", "azure"), ("storage_account", "devstoreaccount1"), ("storage_endpoint", "http://127.0.0.1:10000/devstoreaccount1")])).unwrap().unwrap();
        assert!(a.secret_sql(Some(("", "k"))).unwrap()[0].contains("DefaultEndpointsProtocol=http;AccountName=devstoreaccount1;AccountKey=k;BlobEndpoint=http://127.0.0.1:10000/devstoreaccount1;"));
        let sas = StorageConfig::from_config(&cfg(&[("storage", "azure"), ("storage_account", "acct"), ("storage_auth", "sas")])).unwrap().unwrap();
        assert!(sas.secret_sql(Some(("", "?sv=1&sig=x"))).unwrap()[0].contains("'BlobEndpoint=https://acct.blob.core.windows.net;SharedAccessSignature=sv=1&sig=x'"));
        let cs = StorageConfig::from_config(&cfg(&[("storage", "azure"), ("storage_auth", "connection_string")])).unwrap().unwrap();
        assert_eq!(cs.azure_blob_endpoint(Some("DefaultEndpointsProtocol=https;AccountName=x;AccountKey=y")).as_deref(), Some("https://x.blob.core.windows.net"));
        assert!(cs.secret_sql(None).is_err());
        let sp = StorageConfig::from_config(&cfg(&[("storage", "azure"), ("storage_account", "acct"), ("storage_auth", "service_principal"), ("storage_tenant", "t"), ("storage_client_id", "c")])).unwrap().unwrap();
        assert_eq!(sp.secret_sql(Some(("", "s"))).unwrap()[0], "CREATE OR REPLACE TEMPORARY SECRET databrain_storage (TYPE azure, PROVIDER service_principal, TENANT_ID 't', CLIENT_ID 'c', CLIENT_SECRET 's', ACCOUNT_NAME 'acct')");
        let chain = StorageConfig::from_config(&cfg(&[("storage", "azure"), ("storage_account", "acct"), ("storage_auth", "chain")])).unwrap().unwrap();
        assert!(chain.secret_sql(None).unwrap()[0].contains("PROVIDER credential_chain, CHAIN 'cli;env;managed_identity', ACCOUNT_NAME 'acct'"));
        assert!(StorageConfig::from_config(&cfg(&[("storage", "azure")])).is_err(), "needs the account");
        assert!(StorageConfig::from_config(&cfg(&[("storage", "s3"), ("storage_auth", "sas")])).is_err());
        assert!(StorageConfig::from_config(&cfg(&[("storage", "azure"), ("storage_account", "a"), ("storage_auth", "service_principal")])).is_err());
    }

    #[test]
    fn listing_patterns() {
        assert!(list_sql("s3://b/data/", 10).contains("read_blob('s3://b/data/**')"));
        assert!(list_sql("s3://b", 10).contains("read_blob('s3://b/**')"));
        assert!(list_sql("s3://b/*.csv", 10).contains("read_blob('s3://b/*.csv')"));
    }
}

