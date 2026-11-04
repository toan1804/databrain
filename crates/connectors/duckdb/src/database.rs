use std::sync::{Arc, Mutex};

use databrain_connector_core::{ConnectorError, Result};

use crate::error::map_err;

/// A connection, with the file database it belongs to: whatever still holds
/// the connection (a running query's task too) keeps the file open, and the
/// file closes after its last connection.
pub(crate) struct FileConn {
    pub(crate) conn: duckdb::Connection,
    pub(crate) _db: Option<Arc<FileDb>>,
}

impl std::ops::Deref for FileConn {
    type Target = duckdb::Connection;
    fn deref(&self) -> &duckdb::Connection {
        &self.conn
    }
}

impl std::ops::DerefMut for FileConn {
    fn deref_mut(&mut self) -> &mut duckdb::Connection {
        &mut self.conn
    }
}

pub(crate) type Loaded = Mutex<std::collections::HashMap<String, String>>;

/// One opened database file, shared by every session on it in this process.
/// DuckDB allows a file to be opened by one database instance at a time
/// (on Windows even within a process: a second open fails with "File is
/// already open"); sessions are connections to that one instance.
pub(crate) struct FileDb {
    /// The instance's last connection (sessions' connections close first).
    pub(crate) root: Mutex<Option<duckdb::Connection>>,
    /// The `results` catalog is per instance, so is what was loaded into it.
    pub(crate) loaded: Arc<Loaded>,
    pub(crate) key: FileKey,
}

impl Drop for FileDb {
    /// Closes the file (checkpoint, WAL removed), then leaves the registry.
    /// Until then a session opening the file waits (see [`connect_file`])
    /// instead of opening a second instance on a file that is still closing.
    fn drop(&mut self) {
        if let Some(root) = self.root.get_mut().ok().and_then(Option::take) {
            // The WAL is written into the file now: DuckDB may finish closing
            // on a thread of its own, and a new instance would otherwise read
            // a WAL that one deletes.
            if !self.key.1 {
                let _ = root.execute_batch("CHECKPOINT");
            }
            drop(root);
        }
        if let Ok(mut dbs) = FILE_DBS.lock()
            && dbs.get(&self.key).is_some_and(|w| w.strong_count() == 0)
        {
            dbs.remove(&self.key);
        }
    }
}

/// (resolved path, read-only, sandboxed).
pub(crate) type FileKey = (std::path::PathBuf, bool, bool);
pub(crate) static FILE_DBS: std::sync::LazyLock<Mutex<std::collections::HashMap<FileKey, std::sync::Weak<FileDb>>>> =
    std::sync::LazyLock::new(Default::default);

/// A new connection to the database file at `path`, opening it unless a
/// live session already has it open (same access mode). A new instance is
/// prepared with `init` before any session gets it. A sandboxed instance
/// and an open one can't share the file (one instance per file), so asking
/// for one while the other is open fails.
pub(crate) fn connect_file(
    path: &str,
    read_only: bool,
    sandbox: bool,
    init: &dyn Fn(&duckdb::Connection) -> Result<()>,
) -> Result<(duckdb::Connection, Arc<FileDb>)> {
    let p = std::path::Path::new(path);
    // The same key before and after the file exists (the first session may
    // create it): the folder is resolved (links, 8.3 names), not the file.
    let resolved = match (p.parent().filter(|d| !d.as_os_str().is_empty()), p.file_name()) {
        (Some(dir), Some(name)) => std::fs::canonicalize(dir).map(|d| d.join(name)).unwrap_or_else(|_| p.to_path_buf()),
        (None, Some(name)) => std::env::current_dir().map(|d| d.join(name)).unwrap_or_else(|_| p.to_path_buf()),
        _ => p.to_path_buf(),
    };
    let key = (resolved, read_only, sandbox);
    let closing_since = std::time::Instant::now();
    let mut dbs = loop {
        let dbs = FILE_DBS.lock().map_err(|_| ConnectorError::internal("lock poisoned"))?;
        // An instance whose last session ended is still closing the file.
        let closing = dbs.iter().any(|(k, w)| k.0 == key.0 && w.strong_count() == 0);
        if !closing {
            break dbs;
        }
        drop(dbs);
        if closing_since.elapsed() > std::time::Duration::from_secs(30) {
            return Err(ConnectorError::connection(format!("{path} is still being closed; try again in a moment.")));
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    };
    if let Some(db) = dbs.get(&key).and_then(std::sync::Weak::upgrade) {
        let conn = match db.root.lock().as_deref() {
            Ok(Some(root)) => root.try_clone().map_err(|e| ConnectorError::connection(e.to_string())),
            _ => Err(ConnectorError::internal("the database is closing")),
        };
        // `db` may be the last reference: it closes (and takes the registry) after the guard is released.
        drop(dbs);
        return conn.map(|c| (c, db));
    }
    if dbs.keys().any(|k| k.0 == key.0 && k.2 != sandbox) {
        return Err(ConnectorError::config(if sandbox {
            format!(
                "{path} is open in DataBrain (a query tab or notebook), so the assistant can't open it in its sandbox (no file or network access). Close those tabs, or let the assistant read files and the network in the connection's AI settings."
            )
        } else {
            format!("The assistant is using {path} right now; try again when it finishes.")
        }));
    }
    let root = open_file(path, read_only)?;
    init(&root)?;
    let conn = root.try_clone().map_err(|e| ConnectorError::connection(e.to_string()))?;
    let db = Arc::new(FileDb { root: Mutex::new(Some(root)), loaded: Arc::default(), key: key.clone() });
    dbs.insert(key, Arc::downgrade(&db));
    Ok((conn, db))
}

/// Open `path`, retrying for a moment while an instance that just closed it
/// (in DuckDB's own threads) still has it open or is removing its WAL.
pub(crate) fn open_file(path: &str, read_only: bool) -> Result<duckdb::Connection> {
    let start = std::time::Instant::now();
    loop {
        let c = duckdb::Config::default().access_mode(if read_only { duckdb::AccessMode::ReadOnly } else { duckdb::AccessMode::ReadWrite }).map_err(map_err)?;
        match duckdb::Connection::open_with_flags(path, c) {
            Ok(conn) => return Ok(conn),
            Err(e) => {
                let msg = e.to_string();
                let closing = msg.contains(".wal") || msg.contains("already open");
                if !closing || start.elapsed() > std::time::Duration::from_secs(2) {
                    return Err(ConnectorError::connection(msg));
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
    }
}

#[cfg(test)]
mod tests {

    use crate::test_util::*;
    use databrain_auth::AuthMethod;

    use databrain_connector_core::{ConnectionConfig, ConnectorKind};

    /// A session opening a file that another thread is closing waits for it
    /// to close instead of opening a second instance on it (WAL errors).
    #[tokio::test]
    async fn reopens_a_file_while_it_closes() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = ConnectionConfig::new(ConnectorKind::Duckdb, AuthMethod::None);
        cfg.file_path = Some(dir.path().join("lake.duckdb").to_string_lossy().into_owned());
        for i in 0..30 {
            let s = session(cfg.clone()).await;
            q(&*s, &format!("create table t{i} as select range as x from range(5000)")).await;
            std::thread::spawn(move || drop(s));
            let s2 = session(cfg.clone()).await;
            assert_eq!(q(&*s2, &format!("select count(*) from t{i}")).await.num_rows(), 1);
        }
    }
}
