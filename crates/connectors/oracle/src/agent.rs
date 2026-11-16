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
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

/// Wire protocol version (`Version` in main.go).
pub const PROTOCOL: &str = "1";
/// Release of the helper binaries (download folder and file names).
/// 0.1.1: reconnects after a lost connection ("sql: connection is already closed").
/// 0.1.2: columns go-ora can't decode (SDO_GEOMETRY, XMLTYPE, objects: "TTC
///        error") are converted to text on the server and the first page retried.
/// 0.1.3: a read-only connection's READ ONLY transaction is begun per
///        statement on a context of its own (it was rolled back after each
///        request, so every statement reconnected), and DBMS_OUTPUT is read
///        after a block or CALL (`output` in the exec reply).
pub const RELEASE: &str = "0.1.4";
/// Where releases are published: `{base}/v{RELEASE}/{file}.gz`.
pub const DEFAULT_BASE_URL: &str = "https://github.com/toan1804/databrain/releases/download/oracle-agent";
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

/// The largest frame the helper sends (`maxFrame` in agent/main.go): it
/// refuses a row larger than this rather than wrap the 32-bit length.
const MAX_FRAME: usize = 1 << 30;

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

/// The helper built from this source tree (`node scripts/build-oracle-agent.mjs`,
/// run by `tauri dev` / `tauri build` when Go is installed), if present and
/// matching SHA256SUMS. Found in debug and release builds made from the tree.
pub fn built_from_source() -> Option<PathBuf> {
    let file = file_name();
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../target/oracle-agent").join(&file);
    if !p.is_file() {
        return None;
    }
    // A stale build (other source than the sums) is not used in release builds.
    if !cfg!(debug_assertions) {
        let want = expected_sha256(&file)?;
        if sha256_hex(&std::fs::read(&p).ok()?) != want {
            return None;
        }
    }
    Some(p)
}

/// An installed helper, if any: `DATABRAIN_ORACLE_AGENT`, the one built
/// from this source tree, then the download folder.
pub fn installed() -> Option<PathBuf> {
    let d = DIR.read().unwrap().clone();
    if let Some(p) = std::env::var_os("DATABRAIN_ORACLE_AGENT").and_then(|p| trusted_override(Path::new(&p), cfg!(debug_assertions), d.as_deref())) {
        return Some(p);
    }
    if let Some(p) = built_from_source() {
        return Some(p);
    }
    installed_in(&d?)
}

/// A helper named by `DATABRAIN_ORACLE_AGENT`: any file in a development
/// build (`dev`); in a release, only the published helper of this release
/// (its SHA-256 in SHA256SUMS), so the variable can't make DataBrain start
/// another program and hand it the connection's password.
///
/// In a release the file is read once, its bytes checked, and a copy of
/// those bytes put in DataBrain's own download folder (`root`), which is
/// what starts (checked again there, as a download is): the named file
/// could be changed between the check and the start. A link is refused.
fn trusted_override(p: &Path, dev: bool, root: Option<&Path>) -> Option<PathBuf> {
    if dev {
        return p.is_file().then(|| p.to_path_buf());
    }
    if !std::fs::symlink_metadata(p).ok()?.is_file() {
        tracing::warn!(path = %p.display(), "DATABRAIN_ORACLE_AGENT is not a plain file; not starting it");
        return None;
    }
    let file = file_name();
    let want = expected_sha256(&file)?;
    let bin = std::fs::read(p).ok()?;
    if sha256_hex(&bin) != want {
        tracing::warn!(path = %p.display(), "DATABRAIN_ORACLE_AGENT is not this release's Oracle driver (SHA-256); not starting it");
        return None;
    }
    // It is the published helper: the copy of the bytes just checked starts.
    let root = root?;
    if let Some(installed) = installed_in(root) {
        return Some(installed);
    }
    write_helper(&root.join(RELEASE), &file, &bin).ok()?;
    installed_in(root)
}

/// The helper in the download folder `root`, checked against SHA256SUMS
/// each time it is looked up (so before every launch), not only when it was
/// installed: a file changed since, or a link put in its place, is not
/// started, and [`ensure`] downloads it again.
fn installed_in(root: &Path) -> Option<PathBuf> {
    let file = file_name();
    let p = root.join(RELEASE).join(&file);
    let meta = std::fs::symlink_metadata(&p).ok()?;
    if !meta.is_file() {
        return None;
    }
    let want = expected_sha256(&file)?;
    match std::fs::read(&p) {
        Ok(bin) if sha256_hex(&bin) == want => Some(p),
        _ => {
            tracing::warn!(path = %p.display(), "the installed Oracle driver doesn't match its SHA-256; not starting it");
            None
        }
    }
}

/// What the user needs to install the helper by hand (blocked networks).
#[derive(Debug, Clone, serde::Serialize)]
pub struct AgentInfo {
    /// The helper in use, if one is installed.
    pub installed: Option<String>,
    /// File for this computer (the `.gz` is what the release offers).
    pub file_name: String,
    /// Where DataBrain downloads it from.
    pub url: String,
    /// SHA-256 of the uncompressed helper (checked when installing).
    pub sha256: Option<String>,
    pub release: String,
}

pub fn info() -> AgentInfo {
    let file = file_name();
    let base = std::env::var("DATABRAIN_ORACLE_AGENT_URL").ok().or_else(|| BASE_URL.read().unwrap().clone()).unwrap_or_else(|| DEFAULT_BASE_URL.into());
    AgentInfo {
        installed: installed().map(|p| p.to_string_lossy().into_owned()),
        url: format!("{}/v{RELEASE}/{file}.gz", base.trim_end_matches('/')),
        sha256: expected_sha256(&file),
        file_name: file,
        release: RELEASE.into(),
    }
}

/// Install the helper from a file the user downloaded (the release's `.gz`,
/// or the uncompressed helper). It must match the SHA-256 built into
/// DataBrain, so only DataBrain's own helper can be installed.
pub fn install_from_file(path: &Path) -> Result<PathBuf> {
    let root = DIR.read().unwrap().clone().ok_or_else(|| ConnectorError::config("Oracle driver folder not set"))?;
    install_file_into(&root, path)
}

fn install_file_into(root: &Path, path: &Path) -> Result<PathBuf> {
    let file = file_name();
    let want = expected_sha256(&file)
        .ok_or_else(|| ConnectorError::config(format!("No Oracle driver build for this computer ({file}). Use the Instant Client driver instead.")))?;
    let dir = root.join(RELEASE);
    let raw = std::fs::read(path).map_err(|e| ConnectorError::config(format!("cannot read {}: {e}", path.display())))?;
    let bin = if raw.starts_with(&[0x1f, 0x8b]) {
        use std::io::Read;
        let mut out = Vec::new();
        flate2::read::GzDecoder::new(&raw[..])
            .read_to_end(&mut out)
            .map_err(|e| ConnectorError::config(format!("{} is not a valid .gz file: {e}", path.display())))?;
        out
    } else {
        raw
    };
    let got = sha256_hex(&bin);
    if got != want {
        return Err(ConnectorError::config(format!(
            "{} is not the Oracle driver {RELEASE} for this computer (SHA-256 {got}, expected {want}). Download {file}.gz from the release page.",
            path.display()
        )));
    }
    write_helper(&dir, &file, &bin)
}

fn write_helper(dir: &Path, file: &str, bin: &[u8]) -> Result<PathBuf> {
    std::fs::create_dir_all(dir).map_err(|e| ConnectorError::internal(format!("cannot create {}: {e}", dir.display())))?;
    let tmp = dir.join(format!("{file}.part"));
    std::fs::write(&tmp, bin).map_err(|e| ConnectorError::internal(e.to_string()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755));
    }
    let dest = dir.join(file);
    std::fs::rename(&tmp, &dest).map_err(|e| ConnectorError::internal(e.to_string()))?;
    Ok(dest)
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
        ConnectorError::config(format!("No Oracle driver build for this computer ({file}). Use the Instant Client driver instead."))
            .with_code("oracle_agent_missing")
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
            format!("Could not download the Oracle driver ({url}): {e}. If the network blocks it, download that file another way and choose it in the connection's Driver section (\"Install from a downloaded file\"), or switch the driver to Instant Client.")
        };
        ConnectorError::connection(msg).with_code("oracle_agent_download")
    })?;
    let bin = {
        use std::io::Read;
        let mut out = Vec::new();
        flate2::read::GzDecoder::new(&gz[..])
            .read_to_end(&mut out)
            .map_err(|e| ConnectorError::connection(format!("Oracle driver download is corrupt: {e}")))?;
        out
    };
    let got = sha256_hex(&bin);
    if got != want {
        return Err(ConnectorError::connection(format!("Oracle driver checksum mismatch (expected {want}, got {got}); not installed"))
            .with_code("oracle_agent_download"));
    }
    write_helper(dir, file, &bin)
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
    stdout: tokio::sync::Mutex<Reader>,
    next: AtomicU64,
    _child: std::sync::Mutex<Child>,
}

/// The helper's output, read a frame at a time.
pub type Reader = FrameReader<BufReader<ChildStdout>>;

/// Reads frames, keeping a frame read in part in itself, not in the
/// future reading it: a read dropped midway (Stop during a fetch, a
/// caller's timeout) loses no bytes, and the next read goes on where it
/// stopped. Reading the stream in a future that can be dropped
/// (`read_exact`) left the rest of a frame to be read as the next frame's
/// header, and the session dead or hung.
pub struct FrameReader<R> {
    r: R,
    /// The header read so far (5 bytes: u32 length, kind).
    head: Vec<u8>,
    /// The body read so far, once the header is complete.
    body: Vec<u8>,
}

impl<R: AsyncBufRead + Unpin> FrameReader<R> {
    pub fn new(r: R) -> Self {
        FrameReader { r, head: Vec::with_capacity(5), body: Vec::new() }
    }

    /// Up to `want` more bytes into the header or the body. Cancel-safe:
    /// `fill_buf` keeps what it read in the buffer, and bytes leave it
    /// (`consume`) only once stored here, with no await in between.
    async fn fill(&mut self, body: bool, want: usize) -> Result<()> {
        let buf = self.r.fill_buf().await.map_err(dead)?;
        if buf.is_empty() {
            return Err(dead("end of output"));
        }
        let n = buf.len().min(want);
        let into = if body { &mut self.body } else { &mut self.head };
        into.extend_from_slice(&buf[..n]);
        self.r.consume(n);
        Ok(())
    }

    /// The next frame. Cancel-safe (see [`FrameReader`]).
    pub async fn read(&mut self) -> Result<Frame> {
        while self.head.len() < 5 {
            self.fill(false, 5 - self.head.len()).await?;
        }
        let n = u32::from_be_bytes([self.head[0], self.head[1], self.head[2], self.head[3]]) as usize;
        // The helper refuses a larger row; a larger length is a broken stream.
        if n > MAX_FRAME {
            return Err(dead(format!("a reply of {n} bytes is larger than a frame holds")));
        }
        if self.body.capacity() < n {
            self.body.reserve_exact(n - self.body.len());
        }
        while self.body.len() < n {
            self.fill(true, n - self.body.len()).await?;
        }
        let kind = self.head[4];
        self.head.clear();
        let mut b = std::mem::take(&mut self.body);
        match kind {
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
    let network =
        msg.contains("connection refused") || msg.contains("i/o timeout") || msg.contains("broken pipe") || msg.contains("no such host") || msg.contains("EOF");
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
        let mut child =
            Command::new(path).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).kill_on_drop(true).spawn().map_err(|e| {
                ConnectorError::connection(format!("cannot start the Oracle driver ({}): {e}", path.display())).with_code("oracle_agent_missing")
            })?;
        let stdin = child.stdin.take().ok_or_else(|| dead("no stdin"))?;
        let stdout = child.stdout.take().ok_or_else(|| dead("no stdout"))?;
        let a = Arc::new(Agent {
            stdin: tokio::sync::Mutex::new(stdin),
            stdout: tokio::sync::Mutex::new(FrameReader::new(BufReader::with_capacity(1 << 20, stdout))),
            next: AtomicU64::new(1),
            _child: std::sync::Mutex::new(child),
        });
        let hello = a.call(json!({"op": "hello"})).await?;
        if hello["version"].as_str() != Some(PROTOCOL) {
            return Err(ConnectorError::connection(format!("Oracle driver speaks protocol {:?}, DataBrain needs {PROTOCOL}", hello["version"]))
                .with_code("oracle_agent_missing"));
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

    /// Start a request; returns its id and the reader (held until done).
    pub async fn start(&self, req: Json) -> Result<(u64, tokio::sync::MutexGuard<'_, Reader>)> {
        let guard = self.stdout.lock().await;
        self.start_with(guard, req).await
    }

    /// [`Self::start`], unless `cancel` fires while it waits for the helper
    /// (another request holds it): then nothing is sent and the statement
    /// doesn't run.
    pub async fn start_unless(&self, req: Json, cancel: &databrain_connector_core::CancellationToken) -> Result<(u64, tokio::sync::MutexGuard<'_, Reader>)> {
        let guard = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(ConnectorError::cancelled()),
            g = self.stdout.lock() => g,
        };
        if cancel.is_cancelled() {
            return Err(ConnectorError::cancelled());
        }
        self.start_with(guard, req).await
    }

    async fn start_with<'a>(&self, guard: tokio::sync::MutexGuard<'a, Reader>, mut req: Json) -> Result<(u64, tokio::sync::MutexGuard<'a, Reader>)> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        req["id"] = json!(id);
        self.send(&req).await?;
        Ok((id, guard))
    }

    /// Next frame of request `id` (frames of abandoned earlier requests are skipped).
    pub async fn next(r: &mut Reader, id: u64) -> Result<Frame> {
        next_of(r, id).await
    }

    /// A request with a single reply (`ok`, `rows`, `done`); errors mapped.
    ///
    /// Cancel-safe: if the caller stops waiting (cancelled indexing, a
    /// timeout), the helper is told to cancel the request, so the server
    /// stops the statement and the session is free for the next request
    /// instead of finishing a catalog query nobody reads.
    pub async fn call(self: &Arc<Self>, req: Json) -> Result<Json> {
        let (id, mut r) = self.start(req).await?;
        let mut pending = CancelOnDrop { agent: Some(self.clone()), id };
        let reply = Self::next(&mut r, id).await;
        pending.agent = None; // answered (or the helper is gone)
        match reply? {
            Frame::Json(j) if j["type"] == "error" => Err(reply_error(&j)),
            Frame::Json(j) => Ok(j),
            Frame::Batch { .. } => Err(dead("unexpected batch")),
        }
    }

    pub async fn cancel(&self, id: u64) {
        let _ = self.send(&json!({"id": 0, "op": "cancel", "target": id})).await;
    }

    /// Catalog query: every cell as text.
    pub async fn text(self: &Arc<Self>, sql: &str, binds: &[(&str, Option<String>)]) -> Result<Vec<Vec<Option<String>>>> {
        let b: serde_json::Map<String, Json> = binds.iter().map(|(k, v)| (k.to_string(), json!(v))).collect();
        let j = self.call(json!({"op": "text", "sql": sql, "binds": b})).await?;
        Ok(j["rows"]
            .as_array()
            .map(|rows| rows.iter().map(|r| r.as_array().map(|c| c.iter().map(|v| v.as_str().map(str::to_string)).collect()).unwrap_or_default()).collect())
            .unwrap_or_default())
    }
}

/// Next frame of request `id` from `r` (see [`Agent::next`]).
async fn next_of<R: AsyncBufRead + Unpin>(r: &mut FrameReader<R>, id: u64) -> Result<Frame> {
    loop {
        let f = r.read().await?;
        if f.id() == id {
            return Ok(f);
        }
    }
}

/// Sends a cancel for request `id` when dropped while still armed.
struct CancelOnDrop {
    agent: Option<Arc<Agent>>,
    id: u64,
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        let (Some(a), Ok(rt)) = (self.agent.take(), tokio::runtime::Handle::try_current()) else { return };
        let id = self.id;
        // Its reply frames are skipped by the next request (`next`).
        rt.spawn(async move { a.cancel(id).await });
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

    /// `DATABRAIN_ORACLE_AGENT` in a release starts only this release's
    /// published helper; any file in a development build.
    #[test]
    fn the_agent_override_is_checked_in_a_release() {
        let dir = std::env::temp_dir().join(format!("dbtest-agent-override-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let other = dir.join("not-the-helper");
        std::fs::write(&other, b"#!/bin/sh\necho hi\n").unwrap();
        let root = dir.join("downloads");
        assert_eq!(trusted_override(&other, true, Some(&root)).as_deref(), Some(other.as_path()), "development: used");
        assert_eq!(trusted_override(&other, false, Some(&root)), None, "release: not the published helper");
        assert_eq!(trusted_override(&dir.join("missing"), true, Some(&root)), None);
        #[cfg(unix)]
        {
            // A link is refused in a release, whatever it points at.
            let link = dir.join("link");
            std::os::unix::fs::symlink(&other, &link).unwrap();
            assert_eq!(trusted_override(&link, false, Some(&root)), None);
        }
        assert!(!root.exists(), "nothing copied for a file that isn't the helper");
        let _ = std::fs::remove_dir_all(&dir);
    }
    use tokio::io::AsyncReadExt;

    fn frame(kind: u8, body: &[u8]) -> Vec<u8> {
        let mut f = (body.len() as u32).to_be_bytes().to_vec();
        f.push(kind);
        f.extend_from_slice(body);
        f
    }

    /// A read dropped midway (Stop during a fetch: the read loses the
    /// `select!` to the cancel) keeps what it read: the next read finishes
    /// that frame, and the frames after it are read whole. Before, the
    /// rest of the frame was read as the next header.
    #[tokio::test]
    async fn a_read_dropped_midway_loses_nothing() {
        // A hang fails the test instead of blocking the run.
        tokio::time::timeout(std::time::Duration::from_secs(10), a_read_dropped_midway_loses_nothing_body()).await.expect("hangs");
    }

    async fn a_read_dropped_midway_loses_nothing_body() {
        let (mut w, r) = tokio::io::duplex(1 << 16);
        let mut rd = FrameReader::new(BufReader::new(r));
        let mut batch = 9u64.to_be_bytes().to_vec();
        batch.extend_from_slice(&1u32.to_be_bytes());
        batch.extend_from_slice(&1u32.to_be_bytes());
        batch.extend_from_slice(&[0u8; 300]);
        let mut bytes = frame(b'B', &batch);
        bytes.extend(frame(b'J', br#"{"id":9,"type":"done"}"#));
        let wait = std::time::Duration::from_millis(50);
        // Part of the header, then part of the body: each read is dropped.
        for cut in [3usize, 120] {
            let at = if cut == 3 { 0 } else { 3 };
            w.write_all(&bytes[at..cut]).await.unwrap();
            assert!(tokio::time::timeout(wait, rd.read()).await.is_err(), "waits for the rest");
        }
        w.write_all(&bytes[120..]).await.unwrap();
        match tokio::time::timeout(wait, rd.read()).await.unwrap().unwrap() {
            Frame::Batch { id, ncols, nrows, data } => assert_eq!((id, ncols, nrows, data.len()), (9, 1, 1, 300)),
            Frame::Json(j) => panic!("batch expected, got {j}"),
        }
        match tokio::time::timeout(wait, rd.read()).await.unwrap().unwrap() {
            Frame::Json(j) => assert_eq!(j["type"], "done"),
            Frame::Batch { .. } => panic!("the reply after it"),
        }
    }

    /// A request stopped while another holds the helper is never sent.
    /// `cat` stands in for the helper: it sends each request back, so what
    /// it sends shows what was sent.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_request_stopped_while_waiting_is_not_sent() {
        // A hang fails the test instead of blocking the run.
        tokio::time::timeout(std::time::Duration::from_secs(10), a_request_stopped_while_waiting_is_not_sent_body()).await.expect("hangs");
    }

    #[cfg(unix)]
    async fn a_request_stopped_while_waiting_is_not_sent_body() {
        let mut child = Command::new("cat").stdin(Stdio::piped()).stdout(Stdio::piped()).kill_on_drop(true).spawn().unwrap();
        let a = Arc::new(Agent {
            stdin: tokio::sync::Mutex::new(child.stdin.take().unwrap()),
            stdout: tokio::sync::Mutex::new(FrameReader::new(BufReader::new(child.stdout.take().unwrap()))),
            next: AtomicU64::new(1),
            _child: std::sync::Mutex::new(child),
        });
        // Another request holds the helper; this one is stopped meanwhile.
        let held = a.stdout.lock().await;
        let cancel = databrain_connector_core::CancellationToken::new();
        let (a2, c2) = (a.clone(), cancel.clone());
        let waiting = tokio::spawn(async move { a2.start_unless(json!({"op": "exec", "sql": "delete from t"}), &c2).await.map(|(id, _)| id) });
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        cancel.cancel();
        let r = waiting.await.unwrap();
        assert!(r.is_err_and(|e| e.kind == ErrorKind::Cancelled));
        drop(held);
        // The next request's echo is the first thing out: the stopped one never went.
        let (id, mut r) = a.start(json!({"op": "ping"})).await.unwrap();
        match tokio::time::timeout(std::time::Duration::from_secs(5), r.read()).await.unwrap().unwrap() {
            Frame::Json(j) => assert_eq!((j["op"].as_str(), j["id"].as_u64()), (Some("ping"), Some(id))),
            Frame::Batch { .. } => panic!("echo expected"),
        }
        drop(r); // the request is done: the helper is free
        // Not stopped: sent as before.
        let (id, mut r) = a.start_unless(json!({"op": "exec"}), &databrain_connector_core::CancellationToken::new()).await.unwrap();
        match r.read().await.unwrap() {
            Frame::Json(j) => assert_eq!(j["id"].as_u64(), Some(id)),
            Frame::Batch { .. } => panic!("echo expected"),
        }
    }

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
        // A file the user downloaded: a wrong one is refused, the right one installed.
        let other = tmp.join("other.gz");
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(&other, b"not the driver").unwrap();
        assert!(install_file_into(&tmp, &other).unwrap_err().message.contains("SHA-256"));
        let mine = tmp.join("manual.gz");
        std::fs::write(&mine, &gz).unwrap();
        let p = install_file_into(&tmp, &mine).unwrap();
        assert_eq!(p, tmp.join(RELEASE).join(file_name()));
        std::fs::remove_file(&p).unwrap();
        // Good download: installed and runs.
        let p = download_into(&tmp, &serve(gz).await).await.unwrap();
        assert_eq!(p, tmp.join(RELEASE).join(file_name()));
        drop(Agent::spawn(&p).await.unwrap());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// A helper changed after it was installed is not used.
    #[test]
    fn checks_the_installed_helper_each_time() {
        let tmp = std::env::temp_dir().join(format!("dboa-tamper-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        assert_eq!(installed_in(&tmp), None);
        std::fs::create_dir_all(tmp.join(RELEASE)).unwrap();
        std::fs::write(tmp.join(RELEASE).join(file_name()), b"#!/bin/sh\necho changed\n").unwrap();
        assert_eq!(installed_in(&tmp), None, "a file that doesn't match SHA256SUMS");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// The helper built from the source tree is used without downloading
    /// (debug and release builds), and only when it matches SHA256SUMS in release.
    #[tokio::test]
    async fn uses_helper_built_from_source() {
        let Some(p) = built_from_source() else {
            return eprintln!("skipping: run `node scripts/build-oracle-agent.mjs --host` first");
        };
        assert_eq!(installed().as_deref(), Some(p.as_path()));
        set_dir(None); // no download folder: ensure() must not need one
        assert_eq!(ensure().await.unwrap(), p);
        drop(Agent::spawn(&p).await.unwrap());
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
