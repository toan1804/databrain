//! Credential storage backends.
//!
//! - [`KeychainStore`](crate::KeychainStore): the OS keychain (one item per
//!   secret; macOS may ask for the login password per item and per app build).
//! - [`VaultStore`]: a local encrypted file in the app-data folder, like
//!   DBeaver's credentials file. AES-256-GCM with a random 256-bit key kept
//!   in a separate key file. Both files are owner-only (mode 0600 on Unix, an
//!   explicit user + SYSTEM access list on Windows). Decrypted values and the
//!   key are wiped from memory when dropped. No prompts; anyone who can read
//!   both files as this OS user can decrypt them.
//! - [`SwitchableStore`]: the store the app uses; switches between the two
//!   and migrates secrets.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use aes_gcm::aead::{AeadInOut, Generate, KeyInit, Tag};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use parking_lot_like::Mutex;
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::{AuthError, KeychainStore, SecretRef, SecretStore};

mod parking_lot_like {
    /// `std::sync::Mutex` that ignores poisoning.
    pub struct Mutex<T>(std::sync::Mutex<T>);
    impl<T> Mutex<T> {
        pub fn new(v: T) -> Self {
            Self(std::sync::Mutex::new(v))
        }
        pub fn lock(&self) -> std::sync::MutexGuard<'_, T> {
            self.0.lock().unwrap_or_else(|e| e.into_inner())
        }
    }
}

/// Decrypted entries: values are wiped from memory when dropped.
type Entries = BTreeMap<String, Zeroizing<String>>;

/// Decrypted entries with the file's modification time when they were read.
type Cached = Option<(Option<std::time::SystemTime>, Entries)>;

/// The vault key, wiped from memory when dropped.
type RawKey = Zeroizing<[u8; 32]>;

const TAG_LEN: usize = 16;

#[derive(Serialize, Deserialize)]
struct VaultFile {
    v: u32,
    nonce: String,
    data: String,
}

/// Encrypted local secret file (`vault.json` + `vault.key`).
pub struct VaultStore {
    path: PathBuf,
    key_path: PathBuf,
    /// Held while a process reads, changes and writes `vault.json`.
    lock_path: PathBuf,
    /// Decrypted entries and the file's modification time when loaded
    /// (reloaded when another process, e.g. databrain-mcp, changed it).
    cache: Mutex<Cached>,
}

impl VaultStore {
    /// Vault files in `dir` (created on first write).
    pub fn new(dir: impl AsRef<Path>) -> Self {
        let dir = dir.as_ref();
        Self { path: dir.join("vault.json"), key_path: dir.join("vault.key"), lock_path: dir.join("vault.lock"), cache: Mutex::new(None) }
    }

    fn err(msg: impl std::fmt::Display) -> AuthError {
        AuthError::Store(format!("credential vault: {msg}"))
    }

    /// The vault key; with `create`, made when there is none. It is created
    /// exclusively: when another process made it first, that one is used,
    /// so two first writers never end up with different keys.
    fn key(&self, create: bool) -> Result<Option<RawKey>, AuthError> {
        if let Some(key) = self.read_key()? {
            return Ok(Some(key));
        }
        if !create {
            return Ok(None);
        }
        let mut key = RawKey::default();
        rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, key.as_mut_slice());
        let text = Zeroizing::new(B64.encode(key.as_slice()));
        match write_private(&self.key_path, text.as_bytes()) {
            Ok(()) => Ok(Some(key)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                self.read_key()?.ok_or_else(|| Self::err("vault.key disappeared while it was being created")).map(Some)
            }
            Err(e) => Err(Self::err(format!("cannot create the key file: {e}"))),
        }
    }

    fn read_key(&self) -> Result<Option<RawKey>, AuthError> {
        match std::fs::read_to_string(&self.key_path).map(Zeroizing::new) {
            Ok(s) => {
                let raw = Zeroizing::new(B64.decode(s.trim()).map_err(|_| Self::err("vault.key is damaged"))?);
                let mut key = RawKey::default();
                if raw.len() != key.len() {
                    return Err(Self::err("vault.key has the wrong size"));
                }
                key.copy_from_slice(&raw);
                Ok(Some(key))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(Self::err(format!("cannot read the key file: {e}"))),
        }
    }

    fn cipher(key: &RawKey) -> Aes256Gcm {
        Aes256Gcm::new_from_slice(key.as_slice()).expect("32-byte key")
    }

    fn load(&self) -> Result<Entries, AuthError> {
        let text = match std::fs::read_to_string(&self.path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
            Err(e) => return Err(Self::err(format!("cannot read: {e}"))),
        };
        let damaged = || Self::err("vault.json is damaged");
        let f: VaultFile = serde_json::from_str(&text).map_err(|_| damaged())?;
        let key = self.key(false)?.ok_or_else(|| Self::err("vault.key is missing, so saved passwords cannot be decrypted"))?;
        let nonce_raw = B64.decode(&f.nonce).map_err(|_| damaged())?;
        let nonce = Nonce::try_from(nonce_raw.as_slice()).map_err(|_| damaged())?;
        // Decrypted in place in a buffer that is wiped when dropped.
        let mut buf = Zeroizing::new(B64.decode(&f.data).map_err(|_| damaged())?);
        let n = buf.len().checked_sub(TAG_LEN).ok_or_else(damaged)?;
        let tag = Tag::<Aes256Gcm>::try_from(&buf[n..]).map_err(|_| damaged())?;
        Self::cipher(&key)
            .decrypt_inout_detached(&nonce, b"", buf[..n].as_mut().into(), &tag)
            .map_err(|_| Self::err("cannot decrypt (vault.key does not match vault.json)"))?;
        let map: BTreeMap<String, String> = serde_json::from_slice(&buf[..n]).map_err(|_| damaged())?;
        Ok(map.into_iter().map(|(k, v)| (k, Zeroizing::new(v))).collect())
    }

    fn save(&self, map: &Entries) -> Result<(), AuthError> {
        let key = self.key(true)?.expect("created");
        let nonce = Nonce::generate();
        // Sized for the worst case (every character escaped) so the buffer
        // never grows: growing would leave copies of the plaintext behind.
        let room = map.iter().map(|(k, v)| 6 * (k.len() + v.len()) + 6).sum::<usize>() + 2 + TAG_LEN;
        let mut buf = Zeroizing::new(Vec::with_capacity(room));
        let plain: BTreeMap<&str, &str> = map.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        serde_json::to_writer(&mut *buf, &plain).map_err(Self::err)?;
        let tag = Self::cipher(&key).encrypt_inout_detached(&nonce, b"", buf.as_mut_slice().into()).map_err(|_| Self::err("encryption failed"))?;
        buf.extend_from_slice(&tag);
        let f = VaultFile { v: 1, nonce: B64.encode(nonce.as_slice()), data: B64.encode(&*buf) };
        let json = serde_json::to_vec(&f).map_err(Self::err)?;
        // Atomic replace: write a temp file of this write's own, then rename.
        let tmp = unique_sibling(&self.path, "tmp");
        let written = write_private(&tmp, &json).and_then(|()| std::fs::rename(&tmp, &self.path));
        if written.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        written.map_err(|e| Self::err(format!("cannot write: {e}")))
    }

    fn mtime(&self) -> Option<std::time::SystemTime> {
        std::fs::metadata(&self.path).and_then(|m| m.modified()).ok()
    }

    /// Current entries (cached; reloaded if the file changed).
    fn current<'a>(&self, g: &'a mut Cached) -> Result<&'a Entries, AuthError> {
        let m = self.mtime();
        if g.as_ref().is_none_or(|(t, _)| *t != m) {
            *g = Some((m, self.load()?));
        }
        Ok(&g.as_ref().expect("loaded").1)
    }

    fn with<R>(&self, f: impl FnOnce(&Entries) -> R) -> Result<R, AuthError> {
        let mut g = self.cache.lock();
        Ok(f(self.current(&mut g)?))
    }

    /// Apply one change. Other processes (the app, databrain-mcp,
    /// databrain-jobs) write the same file, so the change is made under the
    /// vault lock to the file as it is now, never to a cached copy.
    fn update(&self, f: impl FnOnce(&mut Entries) -> bool) -> Result<(), AuthError> {
        let mut g = self.cache.lock();
        let _lock = FileLock::acquire(&self.lock_path).map_err(|e| Self::err(format!("cannot lock vault.lock: {e}")))?;
        let mtime = self.mtime();
        let mut map = self.load()?;
        if f(&mut map) {
            self.save(&map)?;
            *g = Some((self.mtime(), map));
        } else {
            *g = Some((mtime, map));
        }
        Ok(())
    }

    /// Number of stored secrets.
    pub fn len(&self) -> Result<usize, AuthError> {
        self.with(|m| m.len())
    }

    pub fn is_empty(&self) -> Result<bool, AuthError> {
        self.len().map(|n| n == 0)
    }
}

impl SecretStore for VaultStore {
    fn get(&self, r: &SecretRef) -> Result<Option<SecretString>, AuthError> {
        self.with(|m| m.get(&r.0).map(|v| SecretString::from(v.as_str().to_string())))
    }
    fn set(&self, r: &SecretRef, value: &SecretString) -> Result<(), AuthError> {
        self.update(|m| {
            m.insert(r.0.clone(), Zeroizing::new(value.expose_secret().to_string()));
            true
        })
    }
    fn delete(&self, r: &SecretRef) -> Result<(), AuthError> {
        self.update(|m| m.remove(&r.0).is_some())
    }
}

/// A lock file left this long is taken to belong to a process that died
/// while writing (a write takes milliseconds).
const STALE_LOCK: Duration = Duration::from_secs(10);
/// How long to wait for another process's write before giving up.
const LOCK_WAIT: Duration = Duration::from_secs(20);

/// Cross-process lock: a file created exclusively, removed when dropped.
/// Portable (no OS lock calls), so it works the same on Unix and Windows.
struct FileLock(PathBuf);

impl FileLock {
    fn acquire(path: &Path) -> std::io::Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let started = Instant::now();
        let mut pause = Duration::from_millis(1);
        loop {
            let mut opts = std::fs::OpenOptions::new();
            opts.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                opts.mode(0o600);
            }
            let busy = match opts.open(path) {
                Ok(mut f) => {
                    use std::io::Write;
                    let _ = writeln!(f, "{}", std::process::id());
                    return Ok(Self(path.to_path_buf()));
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => true,
                // Windows refuses to open a file that is being deleted.
                Err(e) if cfg!(windows) && e.kind() == std::io::ErrorKind::PermissionDenied => false,
                Err(e) => return Err(e),
            };
            if busy {
                let age = std::fs::metadata(path).and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok());
                if age.is_some_and(|a| a > STALE_LOCK) {
                    // Left behind: moved aside (only one waiter can) and removed.
                    let gone = unique_sibling(path, "stale");
                    if std::fs::rename(path, &gone).is_ok() {
                        let _ = std::fs::remove_file(&gone);
                    }
                    continue;
                }
            }
            if started.elapsed() > LOCK_WAIT {
                return Err(std::io::Error::other("another DataBrain process is still writing the vault"));
            }
            std::thread::sleep(pause);
            pause = (pause * 2).min(Duration::from_millis(25));
        }
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// `<file>.<pid>.<random>.<suffix>` next to `path`: unique to one write.
fn unique_sibling(path: &Path, suffix: &str) -> PathBuf {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    path.with_file_name(format!("{name}.{}.{:016x}.{suffix}", std::process::id(), rand::random::<u64>()))
}

/// Create a new file readable only by the current user: mode 0600 on Unix;
/// on Windows an explicit access list (the user and SYSTEM, nothing
/// inherited) set before the contents are written. Fails with
/// `AlreadyExists` when the file exists; a partial file is removed on error.
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let f = opts.open(path)?;
    let written = fill_private(path, f, bytes);
    if written.is_err() {
        let _ = std::fs::remove_file(path);
    }
    written
}

fn fill_private(path: &Path, mut f: std::fs::File, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    #[cfg(windows)]
    windows_acl::owner_only(path)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    drop(f);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

/// Windows access lists through the system's own tools (`whoami`, `icacls`),
/// which keeps this crate free of unsafe FFI code.
#[cfg(windows)]
mod windows_acl {
    use std::os::windows::process::CommandExt;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::sync::OnceLock;

    /// No console window for the helper processes (the app has none).
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    /// LocalSystem, which backup and the OS itself run as.
    const SYSTEM_SID: &str = "S-1-5-18";

    fn system32() -> PathBuf {
        std::env::var_os("SystemRoot").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\Windows")).join("System32")
    }

    fn other(msg: String) -> std::io::Error {
        std::io::Error::other(msg)
    }

    /// The current user's SID (`S-1-5-21-…`).
    fn user_sid() -> std::io::Result<String> {
        static SID: OnceLock<Option<String>> = OnceLock::new();
        let sid = SID.get_or_init(|| {
            let out = Command::new(system32().join("whoami.exe"))
                .args(["/user", "/fo", "csv", "/nh"])
                .stdin(Stdio::null())
                .stderr(Stdio::null())
                .creation_flags(CREATE_NO_WINDOW)
                .output()
                .ok()?;
            parse_sid(&String::from_utf8_lossy(&out.stdout))
        });
        sid.clone().ok_or_else(|| other("cannot find the current user's SID (whoami /user)".into()))
    }

    /// The SID in `whoami /user /fo csv /nh` output: `"host\user","S-1-5-21-…"`.
    pub(super) fn parse_sid(csv: &str) -> Option<String> {
        let sid = csv.trim().rsplit(',').next()?.trim().trim_matches('"');
        (sid.starts_with("S-1-") && sid.chars().all(|c| c.is_ascii_digit() || c == '-' || c == 'S')).then(|| sid.to_string())
    }

    /// Replace `path`'s access list with full control for the current user
    /// and SYSTEM only, dropping inherited entries.
    pub(super) fn owner_only(path: &Path) -> std::io::Result<()> {
        let sid = user_sid()?;
        let out = Command::new(system32().join("icacls.exe"))
            .arg(path)
            .args(["/inheritance:r", "/grant:r", &format!("*{sid}:F"), &format!("*{SYSTEM_SID}:F"), "/q"])
            .stdin(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .output()?;
        if !out.status.success() {
            return Err(other(format!("cannot restrict access to {} (icacls): {}", path.display(), String::from_utf8_lossy(&out.stderr).trim())));
        }
        Ok(())
    }
}

// ------------------------------------------------------------------ switchable

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, ts_rs::TS)]
#[ts(export)]
#[serde(rename_all = "snake_case")]
pub enum StoreKind {
    /// macOS Keychain / Windows Credential Manager / Secret Service.
    #[default]
    Keychain,
    /// Encrypted file in the app-data folder (no prompts).
    Vault,
}

/// Outcome of [`SwitchableStore::switch`].
#[derive(Debug, Clone, Serialize, ts_rs::TS)]
#[ts(export)]
pub struct MigrationReport {
    pub moved: usize,
    /// References that could not be read from the old store.
    pub failed: Vec<String>,
}

/// The app's secret store: keychain or vault, switchable at runtime.
pub struct SwitchableStore {
    kind: Mutex<StoreKind>,
    keychain: Box<dyn SecretStore>,
    vault: Box<dyn SecretStore>,
}

impl SwitchableStore {
    pub fn new(kind: StoreKind, vault_dir: impl AsRef<Path>) -> Self {
        Self::with_stores(kind, Box::new(KeychainStore), Box::new(VaultStore::new(vault_dir)))
    }

    /// Custom backends (tests).
    pub fn with_stores(kind: StoreKind, keychain: Box<dyn SecretStore>, vault: Box<dyn SecretStore>) -> Self {
        Self { kind: Mutex::new(kind), keychain, vault }
    }

    pub fn kind(&self) -> StoreKind {
        *self.kind.lock()
    }

    fn active(&self) -> &dyn SecretStore {
        match self.kind() {
            StoreKind::Keychain => self.keychain.as_ref(),
            StoreKind::Vault => self.vault.as_ref(),
        }
    }

    /// Switch to `to`, copying the given secrets from the current store and
    /// then removing them there. Secrets that cannot be read stay where they
    /// are (and are reported); the switch happens either way.
    pub fn switch(&self, to: StoreKind, refs: &[SecretRef]) -> Result<MigrationReport, AuthError> {
        let mut kind = self.kind.lock();
        if *kind == to {
            return Ok(MigrationReport { moved: 0, failed: vec![] });
        }
        let (from, dest): (&dyn SecretStore, &dyn SecretStore) = match to {
            StoreKind::Vault => (self.keychain.as_ref(), self.vault.as_ref()),
            StoreKind::Keychain => (self.vault.as_ref(), self.keychain.as_ref()),
        };
        let mut report = MigrationReport { moved: 0, failed: vec![] };
        let mut copied: HashMap<String, SecretRef> = HashMap::new();
        for r in refs {
            match from.get(r) {
                Ok(Some(v)) => {
                    dest.set(r, &v)?;
                    copied.insert(r.0.clone(), r.clone());
                    report.moved += 1;
                }
                Ok(None) => {}
                Err(_) => report.failed.push(r.0.clone()),
            }
        }
        *kind = to;
        drop(kind);
        for r in copied.values() {
            let _ = from.delete(r);
        }
        Ok(report)
    }
}

impl SecretStore for SwitchableStore {
    fn get(&self, r: &SecretRef) -> Result<Option<SecretString>, AuthError> {
        self.active().get(r)
    }
    fn set(&self, r: &SecretRef, value: &SecretString) -> Result<(), AuthError> {
        self.active().set(r, value)
    }
    fn delete(&self, r: &SecretRef) -> Result<(), AuthError> {
        self.active().delete(r)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MemoryStore;

    fn r(s: &str) -> SecretRef {
        SecretRef(s.into())
    }

    #[test]
    fn vault_round_trips_and_encrypts() {
        let dir = tempfile::tempdir().unwrap();
        let v = VaultStore::new(dir.path());
        assert!(v.get(&r("a")).unwrap().is_none());
        v.set(&r("a"), &SecretString::from("p@ss wörd".to_string())).unwrap();
        v.set(&r("b"), &SecretString::from("tok".to_string())).unwrap();
        v.delete(&r("b")).unwrap();
        // Another instance (e.g. the MCP server) sees changes and vice versa.
        let other = VaultStore::new(dir.path());
        assert_eq!(other.get(&r("a")).unwrap().unwrap().expose_secret(), "p@ss wörd");
        std::thread::sleep(std::time::Duration::from_millis(20));
        other.set(&r("c"), &SecretString::from("from mcp".to_string())).unwrap();
        assert_eq!(v.get(&r("c")).unwrap().unwrap().expose_secret(), "from mcp");
        other.delete(&r("c")).unwrap();
        // A fresh instance reads the files back.
        let v2 = VaultStore::new(dir.path());
        assert_eq!(v2.get(&r("a")).unwrap().unwrap().expose_secret(), "p@ss wörd");
        assert!(v2.get(&r("b")).unwrap().is_none());
        assert_eq!(v2.len().unwrap(), 1);
        // Not stored in clear text.
        let raw = std::fs::read_to_string(dir.path().join("vault.json")).unwrap();
        assert!(!raw.contains("p@ss") && !raw.contains("connection"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for f in ["vault.json", "vault.key"] {
                let mode = std::fs::metadata(dir.path().join(f)).unwrap().permissions().mode() & 0o777;
                assert_eq!(mode, 0o600, "{f}");
            }
        }
        #[cfg(windows)]
        for f in ["vault.json", "vault.key"] {
            let out = std::process::Command::new("icacls").arg(dir.path().join(f)).output().unwrap();
            let acl = String::from_utf8_lossy(&out.stdout).to_string();
            assert!(!acl.contains("(I)"), "{f}: nothing inherited\n{acl}");
            for broad in ["Everyone", "BUILTIN\\Users", "Authenticated Users"] {
                assert!(!acl.contains(broad), "{f}: {broad}\n{acl}");
            }
        }
        // Wrong key: clear error, no garbage.
        std::fs::write(dir.path().join("vault.key"), B64.encode([7u8; 32])).unwrap();
        let e = VaultStore::new(dir.path()).get(&r("a")).unwrap_err().to_string();
        assert!(e.contains("cannot decrypt"), "{e}");
    }

    /// Files written before the in-place encryption (ciphertext + tag from
    /// `Aead::encrypt`) still open.
    #[test]
    fn reads_vaults_written_by_earlier_versions() {
        use aes_gcm::aead::Aead;
        let dir = tempfile::tempdir().unwrap();
        let key = [9u8; 32];
        std::fs::write(dir.path().join("vault.key"), B64.encode(key)).unwrap();
        let nonce = Nonce::generate();
        let data = Aes256Gcm::new_from_slice(&key).unwrap().encrypt(&nonce, br#"{"a":"old"}"#.as_slice()).unwrap();
        let f = VaultFile { v: 1, nonce: B64.encode(nonce.as_slice()), data: B64.encode(data) };
        std::fs::write(dir.path().join("vault.json"), serde_json::to_vec(&f).unwrap()).unwrap();
        let v = VaultStore::new(dir.path());
        assert_eq!(v.get(&r("a")).unwrap().unwrap().expose_secret(), "old");
        v.set(&r("b"), &SecretString::from("new \"quoted\" \u{1F600}".to_string())).unwrap();
        let v2 = VaultStore::new(dir.path());
        assert_eq!(v2.get(&r("b")).unwrap().unwrap().expose_secret(), "new \"quoted\" \u{1F600}");
        // Too short to hold a tag: damaged, not a panic.
        std::fs::write(dir.path().join("vault.json"), serde_json::to_vec(&VaultFile { v: 1, nonce: f.nonce.clone(), data: B64.encode([1u8; 4]) }).unwrap())
            .unwrap();
        assert!(VaultStore::new(dir.path()).get(&r("a")).unwrap_err().to_string().contains("damaged"));
    }

    /// Separate instances stand in for the app, databrain-mcp and
    /// databrain-jobs (each with its own cache): writing at the same time,
    /// starting with no vault at all, none of their changes is lost and the
    /// vault stays readable (one key).
    #[test]
    fn concurrent_writers_keep_every_change() {
        let dir = tempfile::tempdir().unwrap();
        let (writers, each) = (6, 15);
        let start = std::sync::Arc::new(std::sync::Barrier::new(writers));
        let handles: Vec<_> = (0..writers)
            .map(|w| {
                let (path, start) = (dir.path().to_path_buf(), start.clone());
                std::thread::spawn(move || {
                    let v = VaultStore::new(&path);
                    start.wait();
                    for i in 0..each {
                        v.set(&r(&format!("w{w}:{i}")), &SecretString::from(format!("v{w}.{i}"))).unwrap();
                    }
                    // A delete applies only to its own entry.
                    v.delete(&r(&format!("w{w}:0"))).unwrap();
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        let v = VaultStore::new(dir.path());
        assert_eq!(v.len().unwrap(), writers * (each - 1));
        for w in 0..writers {
            assert!(v.get(&r(&format!("w{w}:0"))).unwrap().is_none());
            for i in 1..each {
                assert_eq!(v.get(&r(&format!("w{w}:{i}"))).unwrap().unwrap().expose_secret(), format!("v{w}.{i}"));
            }
        }
        // No temp or lock files left behind.
        let mut names: Vec<String> = std::fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        names.sort();
        assert_eq!(names, ["vault.json", "vault.key"]);
    }

    /// An instance that cached the map before another one wrote does not
    /// write its stale copy back.
    #[test]
    fn a_stale_cache_does_not_undo_other_writes() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (VaultStore::new(dir.path()), VaultStore::new(dir.path()));
        a.set(&r("a"), &SecretString::from("1".to_string())).unwrap();
        assert!(b.get(&r("a")).unwrap().is_some());
        // Same modification time (coarse clocks): `b` cannot tell it changed.
        let t = std::fs::metadata(dir.path().join("vault.json")).unwrap().modified().unwrap();
        a.set(&r("from_a"), &SecretString::from("2".to_string())).unwrap();
        std::fs::File::options().write(true).open(dir.path().join("vault.json")).unwrap().set_modified(t).unwrap();
        b.set(&r("from_b"), &SecretString::from("3".to_string())).unwrap();
        let fresh = VaultStore::new(dir.path());
        assert!(fresh.get(&r("from_a")).unwrap().is_some(), "a's write survived b's");
        assert!(fresh.get(&r("from_b")).unwrap().is_some());
    }

    /// A lock left by a process that died while writing is taken over; a
    /// live one is waited for.
    #[test]
    fn waits_for_the_lock_and_takes_over_a_stale_one() {
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("vault.lock");
        std::fs::write(&lock, "999999").unwrap();
        let old = std::time::SystemTime::now() - STALE_LOCK - Duration::from_secs(5);
        std::fs::File::options().write(true).open(&lock).unwrap().set_modified(old).unwrap();
        let v = VaultStore::new(dir.path());
        v.set(&r("a"), &SecretString::from("1".to_string())).unwrap();
        assert!(!lock.exists(), "released after the write");
        // Held by someone else: the write waits until it is released.
        let held = FileLock::acquire(&lock).unwrap();
        let path = dir.path().to_path_buf();
        let t = std::thread::spawn(move || VaultStore::new(&path).set(&r("b"), &SecretString::from("2".to_string())).unwrap());
        std::thread::sleep(Duration::from_millis(150));
        assert!(!t.is_finished(), "waits for the lock");
        assert!(v.get(&r("b")).unwrap().is_none());
        drop(held);
        t.join().unwrap();
        assert_eq!(v.get(&r("b")).unwrap().unwrap().expose_secret(), "2");
    }

    /// The key file is created exclusively: an existing one is used, never
    /// replaced.
    #[test]
    fn an_existing_key_is_never_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let v = VaultStore::new(dir.path());
        assert!(v.key(false).unwrap().is_none());
        std::fs::write(dir.path().join("vault.key"), B64.encode([3u8; 32])).unwrap();
        assert_eq!(*v.key(true).unwrap().unwrap(), [3u8; 32]);
        std::fs::remove_file(dir.path().join("vault.key")).unwrap();
        let made = v.key(true).unwrap().unwrap();
        assert_eq!(*v.key(true).unwrap().unwrap(), *made);
        assert!(write_private(&dir.path().join("vault.key"), b"x").unwrap_err().kind() == std::io::ErrorKind::AlreadyExists);
    }

    #[cfg(windows)]
    #[test]
    fn parses_the_user_sid() {
        assert_eq!(windows_acl::parse_sid("\"pc\\me\",\"S-1-5-21-1-2-3-1001\"\r\n").as_deref(), Some("S-1-5-21-1-2-3-1001"));
        assert_eq!(windows_acl::parse_sid("garbage"), None);
    }

    #[test]
    fn switch_migrates_secrets() {
        let dir = tempfile::tempdir().unwrap();
        let s = SwitchableStore::with_stores(StoreKind::Keychain, Box::new(MemoryStore::default()), Box::new(VaultStore::new(dir.path())));
        s.set(&r("connection:1"), &SecretString::from("pw".to_string())).unwrap();
        s.set(&r("ai:2"), &SecretString::from("key".to_string())).unwrap();
        let rep = s.switch(StoreKind::Vault, &[r("connection:1"), r("ai:2"), r("connection:none")]).unwrap();
        assert_eq!((rep.moved, rep.failed.len()), (2, 0));
        assert_eq!(s.kind(), StoreKind::Vault);
        assert_eq!(s.get(&r("connection:1")).unwrap().unwrap().expose_secret(), "pw");
        assert!(s.keychain.get(&r("connection:1")).unwrap().is_none(), "removed from the old store");
        // And back.
        let rep = s.switch(StoreKind::Keychain, &[r("connection:1"), r("ai:2")]).unwrap();
        assert_eq!(rep.moved, 2);
        assert_eq!(s.get(&r("ai:2")).unwrap().unwrap().expose_secret(), "key");
        assert!(VaultStore::new(dir.path()).is_empty().unwrap());
    }
}
