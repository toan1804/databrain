//! `dbr` without `-i` hands what to open to the running app: data files as
//! results, `.sql` files in tabs (run with `--run`), SQL in tabs that run it. The app listens on a
//! socket in its data folder that only its user may use; on Windows on a
//! named pipe with a random name, written to `dbr.pipe` in the data folder
//! (other users can't read it, so they can't take the name first, and the
//! pipe's default access lets them read but not write). `dbr` writes one
//! JSON line and reads the app's one-line answer. Nothing runs on the
//! socket's say-so alone: the request opens tabs, and they run as the
//! editor's do (the read-only setting and the production confirmation).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use databrain_connector_core::sql::ParamValue;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};

/// Longest request read (SQL texts included).
const MAX_REQUEST: u64 = 4 * 1024 * 1024;

/// What to open: a data file or folder (by its full path), a `.sql` file,
/// or SQL text.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ts_rs::TS)]
#[ts(export)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OpenItem {
    Data { path: String },
    SqlFile { path: String },
    Sql { text: String },
}

/// What `dbr` sends: the connection as typed after `-c` (the app finds it
/// as `dbr -i` does), the items in order and the `-p` values.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OpenRequest {
    pub connection: Option<String>,
    pub items: Vec<OpenItem>,
    #[serde(default)]
    pub params: HashMap<String, ParamValue>,
    /// `--run`: run the `.sql` files too (SQL text always runs).
    #[serde(default)]
    pub run: bool,
}

/// What the UI is handed: the request, its connection found (`None`: the
/// local DuckDB, "Local files").
#[derive(Debug, Clone, PartialEq, Serialize, ts_rs::TS)]
#[ts(export)]
pub struct DbrOpen {
    pub connection_id: Option<String>,
    pub items: Vec<OpenItem>,
    pub params: HashMap<String, ParamValue>,
    /// Run the `.sql` files too (`--run`).
    pub run: bool,
}

/// The app's answer: `error` when it opened nothing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Reply {
    pub error: Option<String>,
}

/// The request as the UI takes it: the connection found among `conns` as
/// `dbr -i` finds it, the files checked to be there by their full paths.
pub fn resolve(conns: &[databrain_workspace::ConnectionProfile], req: OpenRequest) -> Result<DbrOpen, String> {
    if req.items.is_empty() {
        return Err("nothing to open".into());
    }
    for item in &req.items {
        match item {
            OpenItem::Data { path } | OpenItem::SqlFile { path } => {
                let p = Path::new(path);
                if !p.is_absolute() {
                    return Err(format!("{path}: not a full path"));
                }
                if !p.exists() {
                    return Err(format!("{path}: no such file"));
                }
            }
            OpenItem::Sql { text } if text.trim().is_empty() => return Err("empty SQL".into()),
            OpenItem::Sql { .. } => {}
        }
    }
    let connection_id = match req.connection.as_deref() {
        Some(name) => Some(crate::cli::query::find_connection(conns, name)?.id.clone()),
        None => None,
    };
    Ok(DbrOpen { connection_id, items: req.items, params: req.params, run: req.run })
}

/// Where the app listens for the app data folder `dir`: `dbr.sock` in it,
/// or, when that path is too long for a socket (about 104 bytes on macOS),
/// a socket named after the folder in a folder of the user's own
/// (`databrain-<uid>`, 0700) in the runtime folder (`XDG_RUNTIME_DIR`) or
/// the temporary folder. Each side also checks that the other is the
/// folder's user ([`same_user`]).
#[cfg(unix)]
pub fn endpoint(dir: &Path) -> PathBuf {
    let p = dir.join("dbr.sock");
    if p.as_os_str().len() <= 100 {
        return p;
    }
    // FNV-1a: the same name in the app and in dbr, whatever Rust built them.
    let hash = dir.as_os_str().as_encoded_bytes().iter().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| (h ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3));
    short_dir(dir).join(format!("{hash:016x}.sock"))
}

/// The user's own folder for sockets whose path in the data folder is too long.
#[cfg(unix)]
fn short_dir(dir: &Path) -> PathBuf {
    use std::os::unix::fs::MetadataExt;
    let base = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).filter(|d| d.is_dir()).unwrap_or_else(std::env::temp_dir);
    let uid = std::fs::metadata(dir).map(|m| m.uid()).unwrap_or(0);
    base.join(format!("databrain-{uid}"))
}

/// Make the folder the socket goes in when it isn't the data folder: the
/// user's own (0700). Another user's folder there (made first in a shared
/// `/tmp`) is refused.
#[cfg(unix)]
fn ensure_socket_dir(dir: &Path, socket: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
    let Some(parent) = socket.parent().filter(|p| *p != dir) else { return Ok(()) };
    match std::fs::DirBuilder::new().mode(0o700).create(parent) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    let (m, owner) = (std::fs::symlink_metadata(parent)?, std::fs::metadata(dir)?.uid());
    if !m.is_dir() || m.uid() != owner || m.permissions().mode() & 0o077 != 0 {
        return Err(std::io::Error::other(format!("{} isn't this user's own folder", parent.display())));
    }
    Ok(())
}

/// Whether the program at the other end of `stream` runs as the user who
/// owns the data folder `dir`.
#[cfg(unix)]
fn same_user(stream: &tokio::net::UnixStream, dir: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (stream.peer_cred(), std::fs::metadata(dir)) {
        (Ok(c), Ok(m)) => c.uid() == m.uid(),
        _ => false,
    }
}

/// The file in the data folder holding the app's pipe name.
#[cfg(windows)]
fn pipe_file(dir: &Path) -> PathBuf {
    dir.join("dbr.pipe")
}

/// The pipe the app on data folder `dir` listens on, as it wrote it.
#[cfg(windows)]
fn pipe_name(dir: &Path) -> Option<String> {
    let name = std::fs::read_to_string(pipe_file(dir)).ok()?;
    let name = name.trim();
    name.starts_with(r"\\.\pipe\databrain-").then(|| name.to_string())
}

/// ERROR_PIPE_BUSY: every instance taken for a moment.
#[cfg(windows)]
const PIPE_BUSY: i32 = 231;

/// Read one request line from `stream`, answer it with `handle`'s result.
async fn answer<S: AsyncRead + AsyncWrite + Unpin>(stream: S, handle: &(dyn Fn(OpenRequest) -> Result<(), String> + Send + Sync)) {
    let (r, mut w) = tokio::io::split(stream);
    let mut line = String::new();
    let read = tokio::time::timeout(Duration::from_secs(10), BufReader::new(r.take_limit()).read_line(&mut line)).await;
    let reply = match read {
        Ok(Ok(n)) if n > 0 && line.ends_with('\n') => match serde_json::from_str::<OpenRequest>(&line) {
            Ok(req) => Reply { error: handle(req).err() },
            Err(e) => Reply { error: Some(format!("not a dbr request: {e}")) },
        },
        Ok(Ok(_)) => Reply { error: Some("the request was cut or too long".into()) },
        Ok(Err(e)) => Reply { error: Some(e.to_string()) },
        Err(_) => return,
    };
    let mut text = serde_json::to_string(&reply).unwrap_or_default();
    text.push('\n');
    let _ = w.write_all(text.as_bytes()).await;
    let _ = w.shutdown().await;
}

trait TakeLimit: AsyncRead + Sized {
    fn take_limit(self) -> tokio::io::Take<Self> {
        tokio::io::AsyncReadExt::take(self, MAX_REQUEST)
    }
}
impl<T: AsyncRead> TakeLimit for T {}

pub type Handler = std::sync::Arc<dyn Fn(OpenRequest) -> Result<(), String> + Send + Sync>;

/// Listen for `dbr` while the app runs (on the current Tokio runtime). A
/// second DataBrain on the same data folder leaves the first one listening.
#[cfg(unix)]
pub async fn serve(dir: PathBuf, handle: Handler) {
    use std::os::unix::fs::PermissionsExt;
    let path = endpoint(&dir);
    if let Err(e) = ensure_socket_dir(&dir, &path) {
        tracing::warn!(error = %e, "dbr can't reach this app");
        return;
    }
    if let Ok(s) = tokio::net::UnixStream::connect(&path).await
        && same_user(&s, &dir)
    {
        tracing::info!("another DataBrain answers dbr");
        return;
    }
    // Left by an app that didn't quit cleanly.
    let _ = std::fs::remove_file(&path);
    let listener = match tokio::net::UnixListener::bind(&path) {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!(error = %e, path = %path.display(), "dbr can't reach this app");
            return;
        }
    };
    let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            // Out of file handles, say: not a loop at full speed.
            tokio::time::sleep(Duration::from_millis(100)).await;
            continue;
        };
        // Only this user's programs: the one owning the data folder.
        if !same_user(&stream, &dir) {
            continue;
        }
        let handle = handle.clone();
        tokio::spawn(async move { answer(stream, &*handle).await });
    }
}

#[cfg(windows)]
pub async fn serve(dir: PathBuf, handle: Handler) {
    use tokio::net::windows::named_pipe::{ClientOptions, ServerOptions};
    // Another DataBrain on this data folder answers already.
    if let Some(old) = pipe_name(&dir) {
        match ClientOptions::new().open(&old) {
            Ok(_) => {
                tracing::info!("another DataBrain answers dbr");
                return;
            }
            Err(e) if e.raw_os_error() == Some(PIPE_BUSY) => {
                tracing::info!("another DataBrain answers dbr");
                return;
            }
            Err(_) => {}
        }
    }
    let name = format!(r"\\.\pipe\databrain-{}", uuid::Uuid::new_v4().simple());
    let mut server = match ServerOptions::new().first_pipe_instance(true).reject_remote_clients(true).create(&name) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "dbr can't reach this app");
            return;
        }
    };
    if let Err(e) = std::fs::write(pipe_file(&dir), &name) {
        tracing::warn!(error = %e, "dbr can't reach this app");
        return;
    }
    loop {
        if server.connect().await.is_err() {
            // A client that left before it was connected: this instance is
            // spent (connecting it again fails at once), so a new one.
            server = next_instance(&name).await;
            continue;
        }
        let next = next_instance(&name).await;
        let conn = std::mem::replace(&mut server, next);
        let handle = handle.clone();
        tokio::spawn(async move { answer(conn, &*handle).await });
    }
}

/// Another instance of pipe `name`, made before the one in hand is given
/// up, so the pipe never vanishes (a dbr then would think the app isn't
/// running); retried until it can be made.
#[cfg(windows)]
async fn next_instance(name: &str) -> tokio::net::windows::named_pipe::NamedPipeServer {
    use tokio::net::windows::named_pipe::ServerOptions;
    loop {
        match ServerOptions::new().reject_remote_clients(true).create(name) {
            Ok(s) => return s,
            Err(e) => {
                tracing::warn!(error = %e, "dbr pipe");
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }
}

/// Why a request wasn't answered.
#[derive(Debug, PartialEq)]
pub enum SendError {
    /// No app listens (it isn't running).
    NotRunning,
    Failed(String),
}

/// Send `req` to the app on data folder `dir`; its answer.
pub async fn send(dir: &Path, req: &OpenRequest) -> Result<Reply, SendError> {
    let mut text = serde_json::to_string(req).map_err(|e| SendError::Failed(e.to_string()))?;
    text.push('\n');
    let stream = connect(dir).await?;
    exchange(stream, text).await
}

async fn exchange<S: AsyncRead + AsyncWrite + Unpin>(stream: S, text: String) -> Result<Reply, SendError> {
    let (r, mut w) = tokio::io::split(stream);
    let fail = |e: std::io::Error| SendError::Failed(e.to_string());
    w.write_all(text.as_bytes()).await.map_err(fail)?;
    w.flush().await.map_err(fail)?;
    let mut line = String::new();
    match tokio::time::timeout(Duration::from_secs(30), BufReader::new(r).read_line(&mut line)).await {
        Ok(Ok(n)) if n > 0 => serde_json::from_str(&line).map_err(|e| SendError::Failed(format!("the app's answer: {e}"))),
        Ok(Ok(_)) => Err(SendError::Failed("the app closed the connection without answering".into())),
        Ok(Err(e)) => Err(fail(e)),
        Err(_) => Err(SendError::Failed("the app didn't answer in 30 s".into())),
    }
}

#[cfg(unix)]
async fn connect(dir: &Path) -> Result<tokio::net::UnixStream, SendError> {
    use std::io::ErrorKind;
    match tokio::net::UnixStream::connect(endpoint(dir)).await {
        // Another user's socket where this user's app would be (a shared /tmp).
        Ok(s) if !same_user(&s, dir) => Err(SendError::Failed(format!("{} isn't this user's DataBrain", endpoint(dir).display()))),
        Ok(s) => Ok(s),
        Err(e) if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::ConnectionRefused) => Err(SendError::NotRunning),
        Err(e) => Err(SendError::Failed(e.to_string())),
    }
}

#[cfg(windows)]
async fn connect(dir: &Path) -> Result<tokio::net::windows::named_pipe::NamedPipeClient, SendError> {
    use tokio::net::windows::named_pipe::ClientOptions;
    let Some(name) = pipe_name(dir) else { return Err(SendError::NotRunning) };
    for _ in 0..50 {
        match ClientOptions::new().open(&name) {
            Ok(c) => return Ok(c),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(SendError::NotRunning),
            Err(e) if e.raw_os_error() == Some(PIPE_BUSY) => tokio::time::sleep(Duration::from_millis(100)).await,
            Err(e) => return Err(SendError::Failed(e.to_string())),
        }
    }
    Err(SendError::Failed("the app is busy".into()))
}

/// Start the app (this program without arguments) apart from the terminal:
/// closing the terminal or Ctrl-C there doesn't quit it. macOS opens the
/// bundle it is in, so it starts as an app does from the Finder.
pub fn start_app() -> std::io::Result<()> {
    use std::process::{Command, Stdio};
    let exe = std::env::current_exe()?;
    // `open` starts the app without this environment: with another data
    // folder, the app is started as a program so it uses the same one.
    #[cfg(target_os = "macos")]
    if std::env::var_os("DATABRAIN_DATA_DIR").is_none() && std::env::var_os("DATABRAIN_WORKSPACE").is_none() {
        // …/DataBrain.app/Contents/MacOS/databrain
        let bundle = exe.parent().and_then(Path::parent).and_then(Path::parent).filter(|b| b.extension().is_some_and(|e| e == "app"));
        if let Some(bundle) = bundle {
            let status = Command::new("/usr/bin/open").arg(bundle).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status()?;
            return if status.success() { Ok(()) } else { Err(std::io::Error::other(format!("open {} failed", bundle.display()))) };
        }
    }
    let mut cmd = Command::new(exe);
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP
        cmd.creation_flags(0x0000_0008 | 0x0000_0200);
    }
    cmd.spawn().map(|_| ())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn req(conn: Option<&str>) -> OpenRequest {
        OpenRequest { connection: conn.map(str::to_string), items: vec![OpenItem::Data { path: "/tmp/a.csv".into() }], params: HashMap::new(), run: false }
    }

    #[tokio::test]
    async fn a_request_reaches_the_app_and_its_answer_comes_back() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(send(dir.path(), &req(None)).await, Err(SendError::NotRunning));
        let got = Arc::new(Mutex::new(Vec::new()));
        let seen = got.clone();
        let handle: Handler = Arc::new(move |r: OpenRequest| {
            if r.connection.as_deref() == Some("nope") {
                return Err("no connection named \"nope\"".into());
            }
            seen.lock().unwrap().push(r);
            Ok(())
        });
        tokio::spawn(serve(dir.path().to_path_buf(), handle));
        let mut reply = Err(SendError::NotRunning);
        for _ in 0..100 {
            reply = send(dir.path(), &req(Some("Sales"))).await;
            if reply != Err(SendError::NotRunning) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(reply, Ok(Reply { error: None }));
        assert_eq!(*got.lock().unwrap(), vec![req(Some("Sales"))]);
        assert_eq!(send(dir.path(), &req(Some("nope"))).await, Ok(Reply { error: Some("no connection named \"nope\"".into()) }));
        // Only its user may use the socket.
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(endpoint(dir.path())).unwrap().permissions().mode() & 0o777, 0o600);
    }

    #[tokio::test]
    async fn a_left_socket_is_replaced_and_a_live_one_kept() {
        let dir = tempfile::tempdir().unwrap();
        // A socket file nobody listens on (an app that crashed).
        drop(std::os::unix::net::UnixListener::bind(endpoint(dir.path())).unwrap());
        let handle: Handler = Arc::new(|_| Ok(()));
        tokio::spawn(serve(dir.path().to_path_buf(), handle.clone()));
        let mut reply = Err(SendError::NotRunning);
        for _ in 0..100 {
            reply = send(dir.path(), &req(None)).await;
            if reply.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(reply, Ok(Reply { error: None }));
        // A second app leaves the first one's socket alone.
        serve(dir.path().to_path_buf(), handle).await;
        assert_eq!(send(dir.path(), &req(None)).await, Ok(Reply { error: None }));
    }

    #[tokio::test]
    async fn a_data_folder_too_deep_for_a_socket_gets_a_short_one() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("a".repeat(60)).join("b".repeat(60));
        std::fs::create_dir_all(&dir).unwrap();
        let path = endpoint(&dir);
        assert!(path.as_os_str().len() <= 100 && !path.starts_with(&dir), "{}", path.display());
        assert_eq!(endpoint(&dir), path, "the same for the app and dbr");
        assert_ne!(endpoint(&dir.join("c")), path);
        tokio::spawn(serve(dir.clone(), Arc::new(|_| Ok(()))));
        let mut reply = Err(SendError::NotRunning);
        for _ in 0..100 {
            reply = send(&dir, &req(None)).await;
            if reply.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(reply, Ok(Reply { error: None }));
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn garbage_gets_an_error_not_a_hang() {
        let dir = tempfile::tempdir().unwrap();
        tokio::spawn(serve(dir.path().to_path_buf(), Arc::new(|_| Ok(()))));
        tokio::time::sleep(Duration::from_millis(100)).await;
        let s = tokio::net::UnixStream::connect(endpoint(dir.path())).await.unwrap();
        let reply = exchange(s, "{\"nope\":1}\n".into()).await.unwrap();
        assert!(reply.error.unwrap().starts_with("not a dbr request"));
    }
}
