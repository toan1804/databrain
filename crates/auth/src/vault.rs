//! Credential storage backends.
//!
//! - [`KeychainStore`](crate::KeychainStore): the OS keychain (one item per
//!   secret; macOS may ask for the login password per item and per app build).
//! - [`VaultStore`]: a local encrypted file in the app-data folder, like
//!   DBeaver's credentials file. AES-256-GCM with a random 256-bit key kept
//!   in a separate key file (owner-only permissions). No prompts; anyone who
//!   can read both files as this OS user can decrypt them.
//! - [`SwitchableStore`]: the store the app uses; switches between the two
//!   and migrates secrets.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use aes_gcm::aead::{Aead, Generate, Key, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use parking_lot_like::Mutex;
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};

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

/// Decrypted entries with the file's modification time when they were read.
type Cached = Option<(Option<std::time::SystemTime>, BTreeMap<String, String>)>;

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

    fn key(&self, create: bool) -> Result<Option<Key<Aes256Gcm>>, AuthError> {
        match std::fs::read_to_string(&self.key_path) {
            Ok(s) => {
                let raw = B64.decode(s.trim()).map_err(|_| Self::err("vault.key is damaged"))?;
                let key = Key::<Aes256Gcm>::try_from(raw.as_slice()).map_err(|_| Self::err("vault.key has the wrong size"))?;
                Ok(Some(key))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if !create {
                    return Ok(None);
                }
                let key = Key::<Aes256Gcm>::generate();
                write_private(&self.key_path, B64.encode(key.as_slice()).as_bytes()).map_err(|e| Self::err(format!("cannot create the key file: {e}")))?;
                Ok(Some(key))
            }
            Err(e) => Err(Self::err(format!("cannot read the key file: {e}"))),
        }
    }

    fn load(&self) -> Result<BTreeMap<String, String>, AuthError> {
        let text = match std::fs::read_to_string(&self.path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
            Err(e) => return Err(Self::err(format!("cannot read: {e}"))),
        };
        let f: VaultFile = serde_json::from_str(&text).map_err(|_| Self::err("vault.json is damaged"))?;
        let key = self.key(false)?.ok_or_else(|| Self::err("vault.key is missing, so saved passwords cannot be decrypted"))?;
        let nonce_raw = B64.decode(&f.nonce).map_err(|_| Self::err("vault.json is damaged"))?;
        let nonce = Nonce::try_from(nonce_raw.as_slice()).map_err(|_| Self::err("vault.json is damaged"))?;
        let data = B64.decode(&f.data).map_err(|_| Self::err("vault.json is damaged"))?;
        let plain = Aes256Gcm::new(&key)
            .decrypt(&nonce, data.as_slice())
            .map_err(|_| Self::err("cannot decrypt (vault.key does not match vault.json)"))?;
        serde_json::from_slice(&plain).map_err(|_| Self::err("vault.json is damaged"))
    }

    fn save(&self, map: &BTreeMap<String, String>) -> Result<(), AuthError> {
        let key = self.key(true)?.expect("created");
        let nonce = Nonce::generate();
        let plain = serde_json::to_vec(map).map_err(Self::err)?;
        let data = Aes256Gcm::new(&key).encrypt(&nonce, plain.as_slice()).map_err(|_| Self::err("encryption failed"))?;
        let f = VaultFile { v: 1, nonce: B64.encode(nonce.as_slice()), data: B64.encode(data) };
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
    fn current(&self, g: &mut Cached) -> Result<BTreeMap<String, String>, AuthError> {
        let m = self.mtime();
        if g.as_ref().is_none_or(|(t, _)| *t != m) {
            *g = Some((m, self.load()?));
        }
        Ok(g.as_ref().expect("loaded").1.clone())
    }

    fn with<R>(&self, f: impl FnOnce(&BTreeMap<String, String>) -> R) -> Result<R, AuthError> {
        let mut g = self.cache.lock();
        let map = self.current(&mut g)?;
        Ok(f(&map))
    }

    fn update(&self, f: impl FnOnce(&mut BTreeMap<String, String>) -> bool) -> Result<(), AuthError> {
        let mut g = self.cache.lock();
        let mut map = self.current(&mut g)?;
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
        self.with(|m| m.get(&r.0).map(|v| SecretString::from(v.clone())))
    }
    fn set(&self, r: &SecretRef, value: &SecretString) -> Result<(), AuthError> {
        self.update(|m| {
            m.insert(r.0.clone(), value.expose_secret().to_string());
            true
        })
    }
    fn delete(&self, r: &SecretRef) -> Result<(), AuthError> {
        self.update(|m| m.remove(&r.0).is_some())
    }
}

/// Create/overwrite a file readable only by the current user (Unix 0600).
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
    f.write_all(bytes)?;
    f.sync_all()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

// ------------------------------------------------------------------ switchable

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum StoreKind {
    /// macOS Keychain / Windows Credential Manager / Secret Service.
    #[default]
    Keychain,
    /// Encrypted file in the app-data folder (no prompts).
    Vault,
}

/// Outcome of [`SwitchableStore::switch`].
#[derive(Debug, Clone, Serialize)]
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
        // Wrong key: clear error, no garbage.
        std::fs::write(dir.path().join("vault.key"), B64.encode([7u8; 32])).unwrap();
        let e = VaultStore::new(dir.path()).get(&r("a")).unwrap_err().to_string();
        assert!(e.contains("cannot decrypt"), "{e}");
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
