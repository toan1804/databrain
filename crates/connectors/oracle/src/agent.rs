//! Thin Oracle driver: a helper process (`databrain-oracle-agent`, Go +
//! go-ora, no Oracle Instant Client) spoken to over stdin/stdout. See
//! `agent/main.go` for the wire format.
//!
//! The helper is downloaded on first use into the app-data folder (5 MB),
//! verified against the SHA-256 sums compiled into DataBrain
//! (`agent/SHA256SUMS`), and started once per Oracle session.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use databrain_connector_core::{ConnectorError, ErrorKind, Result, Value};
use serde_json::{Value as Json, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

/// Wire protocol version (`Version` in main.go).
pub const PROTOCOL: &str = "1";
/// Release of the helper binaries (download folder and file names).
pub const RELEASE: &str = "0.1.0";
/// Where releases are published: `{base}/v{RELEASE}/{file}.gz`.
pub const DEFAULT_BASE_URL: &str = "https://github.com/databrain-app/databrain/releases/download/oracle-agent";
/// `sha256  file` lines of the uncompressed binaries of [`RELEASE`].
const SHA256SUMS: &str = include_str!("../agent/SHA256SUMS");

static DIR: RwLock<Option<PathBuf>> = RwLock::new(None);
static BASE_URL: RwLock<Option<String>> = RwLock::new(None);

/// First-use download events for the UI.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Download {
    Progress { done: u64, total: Option<u64> },
    Finished { ok: bool },
}
pub type ProgressHook = Arc<dyn Fn(Download) + Send + Sync>;
static PROGRESS: RwLock<Option<ProgressHook>> = RwLock::new(None);

/// Folder for downloaded helpers (the app sets `<app data>/oracle-agent`).
pub fn set_dir(dir: Option<PathBuf>) {
    *DIR.write().unwrap() = dir;
}

/// Override the download location (setting / `DATABRAIN_ORACLE_AGENT_URL`).
pub fn set_base_url(url: Option<String>) {
    *BASE_URL.write().unwrap() = url.filter(|u| !u.trim().is_empty());
}

pub fn set_progress_hook(h: Option<ProgressHook>) {
    *PROGRESS.write().unwrap() = h;
}

/// `databrain-oracle-agent-darwin-arm64` etc. for this computer.
pub fn file_name() -> String {
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        o => o,
    };
    let arch = match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "amd64",
        a => a,
    };
    format!("databrain-oracle-agent-{os}-{arch}{}", if cfg!(windows) { ".exe" } else { "" })
}

fn expected_sha256(file: &str) -> Option<String> {
    SHA256SUMS.lines().find_map(|l| {
        let mut it = l.split_whitespace();
        let (h, f) = (it.next()?, it.next()?);
        (f.trim_start_matches('*') == file).then(|| h.to_ascii_lowercase())
    })
}

fn sha256_hex(b: &[u8]) -> String {
    use sha2::Digest;
    hex::encode(sha2::Sha256::digest(b))
}

/// An installed helper, if any: `DATABRAIN_ORACLE_AGENT`, the download
/// folder, then (debug builds) `target/oracle-agent/` of the source tree.
pub fn installed() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("DATABRAIN_ORACLE_AGENT") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Some(p);
        }
    }
    let file = file_name();
    if let Some(d) = DIR.read().unwrap().clone() {
        let p = d.join(RELEASE).join(&file);
        if p.is_file() {
            return Some(p);
        }
    }
    if cfg!(debug_assertions) {
        let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../target/oracle-agent").join(&file);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

static INSTALL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// The helper path, downloading it on first use.
pub async fn ensure() -> Result<PathBuf> {
    if let Some(p) = installed() {
        return Ok(p);
    }
    let _g = INSTALL.lock().await;
    if let Some(p) = installed() {
        return Ok(p);
    }
    let dir = DIR.read().unwrap().clone().ok_or_else(|| ConnectorError::config("Oracle driver folder not set"))?;
    let base = std::env::var("DATABRAIN_ORACLE_AGENT_URL").ok().or_else(|| BASE_URL.read().unwrap().clone()).unwrap_or_else(|| DEFAULT_BASE_URL.into());
    download_into(&dir, &base).await
}

/// Download `{base}/v{RELEASE}/{file}.gz`, verify it and install it into `<root>/<RELEASE>/`.
async fn download_into(root: &Path, base: &str) -> Result<PathBuf> {
    let dir = root.join(RELEASE);
    let file = file_name();
    let want = expected_sha256(&file).ok_or_else(|| {
        ConnectorError::config(format!("No Oracle driver build for this computer ({file}). Use the Instant Client driver instead.")).with_code("oracle_agent_missing")
    })?;
    let url = format!("{}/v{RELEASE}/{file}.gz", base.trim_end_matches('/'));
    let hook = PROGRESS.read().unwrap().clone();
    let r = install(&dir, &file, &want, &url, hook.as_ref()).await;
    if let Some(h) = &hook {
        h(Download::Finished { ok: r.is_ok() });
    }
    r
}

async fn install(dir: &Path, file: &str, want: &str, url: &str, hook: Option<&ProgressHook>) -> Result<PathBuf> {
    let gz = download(url, hook).await.map_err(|e| {
        let msg = if e.contains("404") {
            format!(
                "The thin Oracle driver {RELEASE} is not published at {url} (HTTP 404). Upload the release files (node scripts/build-oracle-agent.mjs → target/oracle-agent/dist), set the oracle_agent_url setting to where they are, or switch the connection's driver to Instant Client."
            )
        } else {
            format!("Could not download the Oracle driver ({url}): {e}. Check the network, or switch the connection's driver to Instant Client.")
        };
        ConnectorError::connection(msg).with_code("oracle_agent_download")
    })?;
    let bin = {
        use std::io::Read;
        let mut out = Vec::new();
        flate2::read::GzDecoder::new(&gz[..]).read_to_end(&mut out).map_err(|e| ConnectorError::connection(format!("Oracle driver download is corrupt: {e}")))?;
        out
    };
    let got = sha256_hex(&bin);
    if got != want {
        return Err(ConnectorError::connection(format!("Oracle driver checksum mismatch (expected {want}, got {got}); not installed")).with_code("oracle_agent_download"));
    }
    std::fs::create_dir_all(dir).map_err(|e| ConnectorError::internal(format!("cannot create {}: {e}", dir.display())))?;
    let tmp = dir.join(format!("{file}.part"));
    std::fs::write(&tmp, &bin).map_err(|e| ConnectorError::internal(e.to_string()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755));
    }
    let dest = dir.join(file);
    std::fs::rename(&tmp, &dest).map_err(|e| ConnectorError::internal(e.to_string()))?;
    Ok(dest)
}

async fn download(url: &str, hook: Option<&ProgressHook>) -> std::result::Result<Vec<u8>, String> {
    if !url.starts_with("https://") && !url.starts_with("http://127.0.0.1") && !url.starts_with("http://localhost") {
        return Err("only https downloads are allowed".into());
    }
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(15))
        .timeout(std::time::Duration::from_secs(600))
        .build()
        .map_err(|e| e.to_string())?;
    let mut resp = client.get(url).send().await.map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }
    let total = resp.content_length();
    let mut out = Vec::with_capacity(total.unwrap_or(6 << 20) as usize);
    if let Some(h) = hook {
        h(Download::Progress { done: 0, total });
    }
    while let Some(chunk) = resp.chunk().await.map_err(|e| e.to_string())? {
        out.extend_from_slice(&chunk);
        if out.len() > 200 << 20 {
            return Err("download too large".into());
        }
        if let Some(h) = hook {
            h(Download::Progress { done: out.len() as u64, total });
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------- process

pub enum Frame {
    Json(Json),
    Batch { id: u64, ncols: usize, nrows: usize, data: Vec<u8> },
}

impl Frame {
    fn id(&self) -> u64 {
        match self {
            Frame::Json(j) => j["id"].as_u64().unwrap_or(0),
            Frame::Batch { id, .. } => *id,
        }
    }
}

pub struct Agent {
    stdin: tokio::sync::Mutex<ChildStdin>,
    /// Held for the whole of a request (one request at a time).
    stdout: tokio::sync::Mutex<BufReader<ChildStdout>>,
    next: AtomicU64,
    _child: std::sync::Mutex<Child>,
}

fn dead(e: impl std::fmt::Display) -> ConnectorError {
    ConnectorError::connection(format!("Oracle driver process stopped: {e}"))
}

/// Map an error reply of the helper.
pub fn reply_error(j: &Json) -> ConnectorError {
    let msg = j["message"].as_str().unwrap_or("unknown error").to_string();
    let code = j["code"].as_str().unwrap_or("");
    if code == "cancelled" || code == "ORA-01013" {
        return ConnectorError::cancelled();
    }
    let n: u32 = code.strip_prefix("ORA-").and_then(|n| n.parse().ok()).unwrap_or(0);
    let network = msg.contains("connection refused") || msg.contains("i/o timeout") || msg.contains("broken pipe") || msg.contains("no such host") || msg.contains("EOF");
    let kind = if matches!(n, 1017 | 12154 | 12505 | 12514 | 12541 | 12545 | 12170 | 3113 | 3114 | 3135 | 28000 | 28001) || (n == 0 && network) {
        ErrorKind::Connection
    } else {
        ErrorKind::Query
    };
    let mut e = ConnectorError::new(kind, msg);
    if !code.is_empty() {
        e = e.with_code(code.to_string());
    }
    if let Some(off) = j["offset"].as_u64() {
        e = e.with_position(Some(off as u32 + 1));
    }
    e
}

impl Agent {
    pub async fn spawn(path: &Path) -> Result<Arc<Agent>> {
        let mut child = Command::new(path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| ConnectorError::connection(format!("cannot start the Oracle driver ({}): {e}", path.display())).with_code("oracle_agent_missing"))?;
        let stdin = child.stdin.take().ok_or_else(|| dead("no stdin"))?;
        let stdout = child.stdout.take().ok_or_else(|| dead("no stdout"))?;
        let a = Arc::new(Agent {
            stdin: tokio::sync::Mutex::new(stdin),
            stdout: tokio::sync::Mutex::new(BufReader::with_capacity(1 << 20, stdout)),
            next: AtomicU64::new(1),
            _child: std::sync::Mutex::new(child),
        });
        let hello = a.call(json!({"op": "hello"})).await?;
        if hello["version"].as_str() != Some(PROTOCOL) {
            return Err(ConnectorError::connection(format!("Oracle driver speaks protocol {:?}, DataBrain needs {PROTOCOL}", hello["version"])).with_code("oracle_agent_missing"));
        }
        Ok(a)
    }

    async fn send(&self, v: &Json) -> Result<()> {
        let b = serde_json::to_vec(v).map_err(|e| ConnectorError::internal(e.to_string()))?;
        let mut w = self.stdin.lock().await;
        let mut buf = Vec::with_capacity(b.len() + 5);
        buf.extend_from_slice(&(b.len() as u32).to_be_bytes());
        buf.push(b'J');
        buf.extend_from_slice(&b);
        w.write_all(&buf).await.map_err(dead)?;
        w.flush().await.map_err(dead)
    }

    async fn read(r: &mut BufReader<ChildStdout>) -> Result<Frame> {
        let mut h = [0u8; 5];
        r.read_exact(&mut h).await.map_err(dead)?;
        let n = u32::from_be_bytes([h[0], h[1], h[2], h[3]]) as usize;
        let mut b = vec![0u8; n];
        r.read_exact(&mut b).await.map_err(dead)?;
        match h[4] {
            b'J' => Ok(Frame::Json(serde_json::from_slice(&b).map_err(|e| dead(format!("bad reply: {e}")))?)),
            b'B' if b.len() >= 16 => {
                let id = u64::from_be_bytes(b[0..8].try_into().unwrap());
                let ncols = u32::from_be_bytes(b[8..12].try_into().unwrap()) as usize;
                let nrows = u32::from_be_bytes(b[12..16].try_into().unwrap()) as usize;
                b.drain(..16);
                Ok(Frame::Batch { id, ncols, nrows, data: b })
            }
            k => Err(dead(format!("unknown frame kind {k}"))),
        }
    }

    /// Start a request; returns its id and the reader (held until done).
    pub async fn start(&self, mut req: Json) -> Result<(u64, tokio::sync::MutexGuard<'_, BufReader<ChildStdout>>)> {
        let guard = self.stdout.lock().await;
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        req["id"] = json!(id);
        self.send(&req).await?;
        Ok((id, guard))
    }

    /// Next frame of request `id` (frames of abandoned earlier requests are skipped).
    pub async fn next(r: &mut BufReader<ChildStdout>, id: u64) -> Result<Frame> {
        loop {
            let f = Self::read(r).await?;
            if f.id() == id {
                return Ok(f);
            }
        }
    }

    /// A request with a single reply (`ok`, `rows`, `done`); errors mapped.
    pub async fn call(&self, req: Json) -> Result<Json> {
        let (id, mut r) = self.start(req).await?;
        match Self::next(&mut r, id).await? {
            Frame::Json(j) if j["type"] == "error" => Err(reply_error(&j)),
            Frame::Json(j) => Ok(j),
            Frame::Batch { .. } => Err(dead("unexpected batch")),
        }
    }

    pub async fn cancel(&self, id: u64) {
        let _ = self.send(&json!({"id": 0, "op": "cancel", "target": id})).await;
    }

    /// Catalog query: every cell as text.
    pub async fn text(&self, sql: &str, binds: &[(&str, Option<String>)]) -> Result<Vec<Vec<Option<String>>>> {
        let b: serde_json::Map<String, Json> = binds.iter().map(|(k, v)| (k.to_string(), json!(v))).collect();
        let j = self.call(json!({"op": "text", "sql": sql, "binds": b})).await?;
        Ok(j["rows"]
            .as_array()
            .map(|rows| rows.iter().map(|r| r.as_array().map(|c| c.iter().map(|v| v.as_str().map(str::to_string)).collect()).unwrap_or_default()).collect())
            .unwrap_or_default())
    }
}

/// Decode the values of a batch frame, row by row.
pub fn decode_rows(data: &[u8], ncols: usize, nrows: usize, mut row: impl FnMut(Vec<Value>)) -> Result<()> {
    let bad = || ConnectorError::internal("Oracle driver sent a malformed batch");
    let mut p = 0usize;
    let take = |p: &mut usize, n: usize| -> Result<&[u8]> {
        let s = data.get(*p..*p + n).ok_or_else(bad)?;
        *p += n;
        Ok(s)
    };
    for _ in 0..nrows {
        let mut vals = Vec::with_capacity(ncols);
        for _ in 0..ncols {
            let tag = take(&mut p, 1)?[0];
            let i64v = |p: &mut usize| -> Result<i64> { Ok(i64::from_le_bytes(take(p, 8)?.try_into().unwrap())) };
            let v = match tag {
                0 => Value::Null,
                1 => Value::Int(i64v(&mut p)?),
                2 => Value::Float(f64::from_bits(i64v(&mut p)? as u64)),
                3 | 4 => {
                    let n = u32::from_le_bytes(take(&mut p, 4)?.try_into().unwrap()) as usize;
                    let b = take(&mut p, n)?;
                    if tag == 3 { Value::Text(String::from_utf8_lossy(b).into_owned()) } else { Value::Bytes(b.to_vec()) }
                }
                5 => Value::Timestamp(i64v(&mut p)?),
                6 => Value::TimestampTz(i64v(&mut p)?),
                7 => Value::Bool(take(&mut p, 1)?[0] == 1),
                _ => return Err(bad()),
            };
            vals.push(v);
        }
        row(vals);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Download + checksum, from a local server serving `target/oracle-agent/dist`.
    #[tokio::test]
    async fn downloads_and_verifies() {
        let dist = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../target/oracle-agent/dist");
        let gz = match std::fs::read(dist.join(format!("{}.gz", file_name()))) {
            Ok(b) => b,
            Err(_) => return eprintln!("skipping: run `node scripts/build-oracle-agent.mjs` first"),
        };
        async fn serve(body: Vec<u8>) -> String {
            let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://127.0.0.1:{}", l.local_addr().unwrap().port());
            tokio::spawn(async move {
                while let Ok((mut s, _)) = l.accept().await {
                    let body = body.clone();
                    tokio::spawn(async move {
                        let mut req = [0u8; 2048];
                        let _ = s.read(&mut req).await;
                        let head = format!("HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n", body.len());
                        let _ = s.write_all(head.as_bytes()).await;
                        let _ = s.write_all(&body).await;
                    });
                }
            });
            url
        }
        let tmp = std::env::temp_dir().join(format!("dboa-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        // Tampered download: refused, nothing installed.
        let mut bad = gz.clone();
        let n = bad.len();
        bad[n / 2] ^= 0xff;
        let e = download_into(&tmp, &serve(bad).await).await.unwrap_err();
        assert!(e.message.contains("corrupt") || e.message.contains("checksum"), "{}", e.message);
        assert!(!tmp.join(RELEASE).join(file_name()).exists());
        // Plain http to other hosts is refused.
        assert!(download_into(&tmp, "http://example.com").await.unwrap_err().message.contains("https"));
        // Good download: installed and runs.
        let p = download_into(&tmp, &serve(gz).await).await.unwrap();
        assert_eq!(p, tmp.join(RELEASE).join(file_name()));
        drop(Agent::spawn(&p).await.unwrap());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn decodes_values() {
        let mut d = vec![1u8];
        d.extend(42i64.to_le_bytes());
        d.push(3);
        d.extend(2u32.to_le_bytes());
        d.extend(b"hi");
        d.push(0);
        d.push(7);
        d.push(1);
        let mut rows = vec![];
        decode_rows(&d, 4, 1, |r| rows.push(r)).unwrap();
        assert_eq!(rows, vec![vec![Value::Int(42), Value::Text("hi".into()), Value::Null, Value::Bool(true)]]);
        assert!(decode_rows(&d[..5], 4, 1, |_| {}).is_err(), "truncated");
    }

    #[test]
    fn maps_errors_and_names_files() {
        let e = reply_error(&json!({"code": "ORA-00942", "message": "table or view does not exist", "offset": 14}));
        assert_eq!((e.kind, e.code.as_deref(), e.position), (ErrorKind::Query, Some("ORA-00942"), Some(15)));
        assert_eq!(reply_error(&json!({"code": "ORA-01017", "message": "x"})).kind, ErrorKind::Connection);
        assert_eq!(reply_error(&json!({"code": "cancelled", "message": "x"})).kind, ErrorKind::Cancelled);
        assert!(file_name().starts_with("databrain-oracle-agent-"));
        assert!(expected_sha256("databrain-oracle-agent-darwin-arm64").is_some(), "SHA256SUMS lists the builds");
    }
}
