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
    /// Decrypted entries and the file's modification time when loaded
    /// (reloaded when another process, e.g. databrain-mcp, changed it).
    cache: Mutex<Cached>,
}

impl VaultStore {
    /// Vault files in `dir` (created on first write).
    pub fn new(dir: impl AsRef<Path>) -> Self {
        let dir = dir.as_ref();
        Self { path: dir.join("vault.json"), key_path: dir.join("vault.key"), cache: Mutex::new(None) }
    }

    fn err(msg: impl std::fmt::Display) -> AuthError {
        AuthError::Store(format!("credential vault: {msg}"))
    }

    fn key(&self, create: bool) -> Result<Option<RawKey>, AuthError> {
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
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if !create {
                    return Ok(None);
                }
                let mut key = RawKey::default();
                rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, key.as_mut_slice());
                let text = Zeroizing::new(B64.encode(key.as_slice()));
                write_private(&self.key_path, text.as_bytes()).map_err(|e| Self::err(format!("cannot create the key file: {e}")))?;
                Ok(Some(key))
            }
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
        // Atomic replace: write a temp file, then rename.
        let tmp = self.path.with_extension("json.tmp");
        write_private(&tmp, &json).map_err(|e| Self::err(format!("cannot write: {e}")))?;
        std::fs::rename(&tmp, &self.path).map_err(|e| Self::err(format!("cannot write: {e}")))
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

    fn update(&self, f: impl FnOnce(&mut Entries) -> bool) -> Result<(), AuthError> {
        let mut g = self.cache.lock();
        let mut map = self.current(&mut g)?.clone();
        if f(&mut map) {
            self.save(&map)?;
            *g = Some((self.mtime(), map));
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

/// Create/overwrite a file readable only by the current user: mode 0600 on
/// Unix; on Windows an explicit access list (the user and SYSTEM, nothing
/// inherited) set before the contents are written.
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    use std::io::Write;
    let mut f = opts.open(path)?;
    #[cfg(windows)]
    windows_acl::owner_only(path)?;
    f.write_all(bytes)?;
    f.sync_all()?;
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
