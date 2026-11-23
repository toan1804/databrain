//! `dbr` without `-i` hands what to open to the running app: data files as
//! results, SQL and `.sql` files in tabs that run them. The app listens on a
//! socket in its data folder that only its user may use (a named pipe on
//! Windows, which other users may read but not write); `dbr` writes one
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
}

/// What the UI is handed: the request, its connection found (`None`: the
/// local DuckDB, "Local files").
#[derive(Debug, Clone, PartialEq, Serialize, ts_rs::TS)]
#[ts(export)]
pub struct DbrOpen {
    pub connection_id: Option<String>,
    pub items: Vec<OpenItem>,
    pub params: HashMap<String, ParamValue>,
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
    Ok(DbrOpen { connection_id, items: req.items, params: req.params })
}

/// Where the app listens for the app data folder `dir`: `dbr.sock` in it,
/// or, when that path is too long for a socket (about 104 bytes on macOS),
/// a socket named after the folder in the user's runtime folder
/// (`XDG_RUNTIME_DIR`) or the temporary folder. Each side checks that the
/// other is the folder's user ([`same_user`]), which a shared `/tmp` needs.
#[cfg(unix)]
pub fn endpoint(dir: &Path) -> PathBuf {
    let p = dir.join("dbr.sock");
    if p.as_os_str().len() <= 100 {
        return p;
    }
    let base = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).filter(|d| d.is_dir()).unwrap_or_else(std::env::temp_dir);
    // FNV-1a: the same name in the app and in dbr, whatever Rust built them.
    let hash = dir.as_os_str().as_encoded_bytes().iter().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| (h ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3));
    base.join(format!("databrain-{hash:016x}.sock"))
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

/// Where the app listens for the app data folder `dir`: a pipe named after
/// it, so a second data folder (`DATABRAIN_DATA_DIR`) has its own.
#[cfg(windows)]
pub fn endpoint(dir: &Path) -> PathBuf {
    let tag: String = dir.to_string_lossy().to_lowercase().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect();
    let tag: String = tag.chars().rev().take(180).collect::<Vec<_>>().into_iter().rev().collect();
    PathBuf::from(format!(r"\\.\pipe\databrain-{tag}"))
}

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
    if tokio::net::UnixStream::connect(&path).await.is_ok() {
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
        let Ok((stream, _)) = listener.accept().await else { continue };
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
    use tokio::net::windows::named_pipe::ServerOptions;
    let name = endpoint(&dir);
    let mut server = match ServerOptions::new().first_pipe_instance(true).reject_remote_clients(true).create(&name) {
        Ok(s) => s,
        Err(e) => {
            tracing::info!(error = %e, "dbr can't reach this app (another DataBrain answers it?)");
            return;
        }
    };
    loop {
        if server.connect().await.is_err() {
            continue;
        }
        let next = match ServerOptions::new().reject_remote_clients(true).create(&name) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "dbr pipe");
                return;
            }
        };
        let conn = std::mem::replace(&mut server, next);
        let handle = handle.clone();
        tokio::spawn(async move { answer(conn, &*handle).await });
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
    // ERROR_PIPE_BUSY: every instance taken for a moment.
    const PIPE_BUSY: i32 = 231;
    for _ in 0..50 {
        match ClientOptions::new().open(endpoint(dir)) {
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
    #[cfg(target_os = "macos")]
    {
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
        OpenRequest { connection: conn.map(str::to_string), items: vec![OpenItem::Data { path: "/tmp/a.csv".into() }], params: HashMap::new() }
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
