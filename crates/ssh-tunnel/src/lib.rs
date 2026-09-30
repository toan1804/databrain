//! SSH local port forwarding for database connections behind a bastion.
//!
//! [`Tunnel::open`] authenticates to the SSH server, listens on
//! `127.0.0.1:<random>` and forwards every accepted connection to
//! `target_host:target_port` through a `direct-tcpip` channel. Connectors
//! then connect to the local port. The tunnel closes when dropped.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use databrain_connector_core::SshAuth;
use russh::client::{self, Handle};
use russh::keys::{HashAlg, PrivateKeyWithHashAlg, PublicKeyOrCertificate};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

#[derive(Debug, thiserror::Error)]
pub enum TunnelError {
    #[error("SSH connection to {0} failed: {1}")]
    Connect(String, String),
    #[error("SSH host key mismatch for {host}: expected {expected}, got {actual}. The server key changed; if this is expected, clear the saved fingerprint in the connection's SSH settings.")]
    HostKeyMismatch { host: String, expected: String, actual: String },
    #[error("SSH authentication failed for {0}")]
    AuthFailed(String),
    #[error("{0}")]
    Other(String),
}

/// Secret material for SSH auth.
pub enum SshSecret {
    None,
    Password(String),
    KeyPassphrase(String),
}

pub struct TunnelSpec<'a> {
    pub ssh_host: &'a str,
    pub ssh_port: u16,
    pub user: &'a str,
    pub auth: &'a SshAuth,
    pub secret: SshSecret,
    /// Expected `SHA256:...` fingerprint; `None` accepts and records the key.
    pub expected_fingerprint: Option<&'a str>,
    pub target_host: &'a str,
    pub target_port: u16,
}

struct Verifier {
    expected: Option<String>,
    seen: Arc<Mutex<Option<String>>>,
}

impl client::Handler for Verifier {
    type Error = russh::Error;

    async fn check_server_key(&mut self, key: &PublicKeyOrCertificate) -> Result<bool, Self::Error> {
        let fp = match key {
            PublicKeyOrCertificate::PublicKey { key, .. } => key.fingerprint(HashAlg::Sha256).to_string(),
            PublicKeyOrCertificate::Certificate(c) => c.public_key().fingerprint(HashAlg::Sha256).to_string(),
        };
        *self.seen.lock().expect("poisoned") = Some(fp.clone());
        Ok(self.expected.as_deref().is_none_or(|e| e == fp))
    }
}

pub struct Tunnel {
    local_port: u16,
    fingerprint: String,
    cancel: CancellationToken,
    _session: Arc<Handle<Verifier>>,
}

impl Tunnel {
    pub fn local_port(&self) -> u16 {
        self.local_port
    }

    /// Server host key fingerprint (to pin on first use).
    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    pub async fn open(spec: TunnelSpec<'_>) -> Result<Tunnel, TunnelError> {
        let addr = format!("{}:{}", spec.ssh_host, spec.ssh_port);
        let config = Arc::new(client::Config {
            inactivity_timeout: None,
            keepalive_interval: Some(Duration::from_secs(30)),
            ..Default::default()
        });
        let seen = Arc::new(Mutex::new(None));
        let handler = Verifier {
            expected: spec.expected_fingerprint.map(str::to_string),
            seen: seen.clone(),
        };
        let connect = client::connect(config, (spec.ssh_host, spec.ssh_port), handler);
        let mut session = match tokio::time::timeout(Duration::from_secs(20), connect).await {
            Err(_) => return Err(TunnelError::Connect(addr, "timed out".into())),
            Ok(Err(e)) => {
                let actual = seen.lock().expect("poisoned").clone();
                if let (Some(expected), Some(actual)) = (spec.expected_fingerprint, actual) {
                    if expected != actual {
                        return Err(TunnelError::HostKeyMismatch { host: addr, expected: expected.into(), actual });
                    }
                }
                return Err(TunnelError::Connect(addr, e.to_string()));
            }
            Ok(Ok(s)) => s,
        };
        let fingerprint = seen.lock().expect("poisoned").clone().unwrap_or_default();

        let ok = match (spec.auth, &spec.secret) {
            (SshAuth::Password, SshSecret::Password(p)) => session
                .authenticate_password(spec.user, p.clone())
                .await
                .map_err(|e| TunnelError::Other(e.to_string()))?
                .success(),
            (SshAuth::Password, _) => {
                return Err(TunnelError::Other("SSH password is not set".into()));
            }
            (SshAuth::Key { path }, secret) => {
                let pass = match secret {
                    SshSecret::KeyPassphrase(p) if !p.is_empty() => Some(p.as_str()),
                    _ => None,
                };
                let path = expand_home(path);
                let key = russh::keys::load_secret_key(&path, pass)
                    .map_err(|e| TunnelError::Other(format!("cannot load SSH key {path}: {e}")))?;
                let hash = session
                    .best_supported_rsa_hash()
                    .await
                    .map_err(|e| TunnelError::Other(e.to_string()))?
                    .flatten();
                session
                    .authenticate_publickey(spec.user, PrivateKeyWithHashAlg::new(Arc::new(key), hash))
                    .await
                    .map_err(|e| TunnelError::Other(e.to_string()))?
                    .success()
            }
            (SshAuth::Agent, _) => agent_auth(&mut session, spec.user).await?,
        };
        if !ok {
            return Err(TunnelError::AuthFailed(format!("{}@{}", spec.user, addr)));
        }

        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .map_err(|e| TunnelError::Other(format!("cannot open local tunnel port: {e}")))?;
        let local_port = listener.local_addr().map_err(|e| TunnelError::Other(e.to_string()))?.port();
        let session = Arc::new(session);
        let cancel = CancellationToken::new();
        let (target_host, target_port) = (spec.target_host.to_string(), spec.target_port);
        let s = session.clone();
        let c = cancel.clone();
        tokio::spawn(async move {
            loop {
                let (mut sock, peer) = tokio::select! {
                    r = listener.accept() => match r { Ok(x) => x, Err(_) => continue },
                    _ = c.cancelled() => break,
                };
                let s = s.clone();
                let (h, p) = (target_host.clone(), target_port);
                let c = c.clone();
                tokio::spawn(async move {
                    let ch = match s
                        .channel_open_direct_tcpip(h, p as u32, peer.ip().to_string(), peer.port() as u32)
                        .await
                    {
                        Ok(ch) => ch,
                        Err(_) => return,
                    };
                    let mut stream = ch.into_stream();
                    tokio::select! {
                        _ = tokio::io::copy_bidirectional(&mut sock, &mut stream) => {}
                        _ = c.cancelled() => {}
                    }
                });
            }
        });
        Ok(Tunnel { local_port, fingerprint, cancel, _session: session })
    }
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

#[cfg(unix)]
async fn agent_auth(session: &mut Handle<Verifier>, user: &str) -> Result<bool, TunnelError> {
    let mut agent = russh::keys::agent::client::AgentClient::connect_env()
        .await
        .map_err(|e| TunnelError::Other(format!("SSH agent not available: {e}")))?;
    let ids = agent
        .request_identities()
        .await
        .map_err(|e| TunnelError::Other(e.to_string()))?;
    let hash = session
        .best_supported_rsa_hash()
        .await
        .map_err(|e| TunnelError::Other(e.to_string()))?
        .flatten();
    for id in ids {
        let key = id.public_key().into_owned();
        if let Ok(r) = session.authenticate_publickey_with(user, key, hash, &mut agent).await {
            if r.success() {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

#[cfg(not(unix))]
async fn agent_auth(_session: &mut Handle<Verifier>, _user: &str) -> Result<bool, TunnelError> {
    Err(TunnelError::Other("SSH agent authentication is only supported on macOS/Linux".into()))
}

fn expand_home(p: &str) -> String {
    if let Some(rest) = p.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
            return format!("{}/{rest}", home.to_string_lossy());
        }
    }
    p.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_home() {
        let h = std::env::var("HOME").unwrap_or_default();
        assert_eq!(expand_home("~/.ssh/id_ed25519"), format!("{h}/.ssh/id_ed25519"));
        assert_eq!(expand_home("/abs"), "/abs");
    }

    /// In-process SSH server: password auth + direct-tcpip forwarding.
    mod server {
        use russh::Channel;
        use russh::server::{self, Auth, ChannelOpenHandle, Msg, Session};

        #[derive(Clone)]
        pub struct Srv;

        impl server::Server for Srv {
            type Handler = Srv;
            fn new_client(&mut self, _: Option<std::net::SocketAddr>) -> Srv {
                Srv
            }
        }

        impl server::Handler for Srv {
            type Error = russh::Error;

            async fn auth_password(&mut self, user: &str, password: &str) -> Result<Auth, Self::Error> {
                Ok(if user == "alice" && password == "pw" { Auth::Accept } else { Auth::reject() })
            }

            #[allow(clippy::too_many_arguments)]
            async fn channel_open_direct_tcpip(
                &mut self,
                channel: Channel<Msg>,
                host: &str,
                port: u32,
                _oa: &str,
                _op: u32,
                reply: ChannelOpenHandle,
                _s: &mut Session,
            ) -> Result<(), Self::Error> {
                let addr = format!("{host}:{port}");
                reply.accept().await;
                tokio::spawn(async move {
                    if let Ok(mut tcp) = tokio::net::TcpStream::connect(addr).await {
                        let mut ch = channel.into_stream();
                        let _ = tokio::io::copy_bidirectional(&mut tcp, &mut ch).await;
                    }
                });
                Ok(())
            }
        }
    }

    #[tokio::test]
    async fn tunnel_end_to_end_with_host_key_pinning() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        // Echo server as the "database".
        let echo = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let echo_port = echo.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let (mut s, _) = echo.accept().await.unwrap();
                tokio::spawn(async move {
                    let mut buf = [0u8; 64];
                    while let Ok(n) = s.read(&mut buf).await {
                        if n == 0 || s.write_all(&buf[..n]).await.is_err() {
                            break;
                        }
                    }
                });
            }
        });
        // SSH server.
        let key = russh::keys::PrivateKey::random(&mut rand::rng(), russh::keys::Algorithm::Ed25519).unwrap();
        let expected_fp = key.public_key().fingerprint(HashAlg::Sha256).to_string();
        let cfg = Arc::new(russh::server::Config { keys: vec![key], ..Default::default() });
        let ssh = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let ssh_port = ssh.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let (sock, _) = ssh.accept().await.unwrap();
                let cfg = cfg.clone();
                tokio::spawn(async move {
                    if let Ok(s) = russh::server::run_stream(cfg, sock, server::Srv).await {
                        let _ = s.await;
                    }
                });
            }
        });

        let open = |fp: Option<String>, pw: &str| {
            let pw = pw.to_string();
            async move {
                Tunnel::open(TunnelSpec {
                    ssh_host: "127.0.0.1",
                    ssh_port,
                    user: "alice",
                    auth: &SshAuth::Password,
                    secret: SshSecret::Password(pw),
                    expected_fingerprint: fp.as_deref(),
                    target_host: "127.0.0.1",
                    target_port: echo_port,
                })
                .await
            }
        };

        let t = open(None, "pw").await.unwrap();
        assert_eq!(t.fingerprint(), expected_fp);
        let mut c = tokio::net::TcpStream::connect(("127.0.0.1", t.local_port())).await.unwrap();
        c.write_all(b"ping").await.unwrap();
        let mut buf = [0u8; 4];
        c.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"ping");

        assert!(open(Some(expected_fp.clone()), "pw").await.is_ok());
        assert!(matches!(open(Some("SHA256:wrong".into()), "pw").await, Err(TunnelError::HostKeyMismatch { .. })));
        assert!(matches!(open(None, "bad").await, Err(TunnelError::AuthFailed(_))));
    }

    #[tokio::test]
    async fn unreachable_host_fails_fast() {
        let spec = TunnelSpec {
            ssh_host: "127.0.0.1",
            ssh_port: 1,
            user: "u",
            auth: &SshAuth::Password,
            secret: SshSecret::Password("x".into()),
            expected_fingerprint: None,
            target_host: "db",
            target_port: 5432,
        };
        assert!(matches!(Tunnel::open(spec).await, Err(TunnelError::Connect(..))));
    }

    /// Live test: DATABRAIN_SSH_HOST, _USER, _PASSWORD; forwards to the SSH port itself.
    #[tokio::test]
    async fn live_tunnel() {
        let (Ok(host), Ok(user), Ok(pw)) = (
            std::env::var("DATABRAIN_SSH_HOST"),
            std::env::var("DATABRAIN_SSH_USER"),
            std::env::var("DATABRAIN_SSH_PASSWORD"),
        ) else {
            eprintln!("skipping: DATABRAIN_SSH_* not set");
            return;
        };
        let t = Tunnel::open(TunnelSpec {
            ssh_host: &host,
            ssh_port: 22,
            user: &user,
            auth: &SshAuth::Password,
            secret: SshSecret::Password(pw),
            expected_fingerprint: None,
            target_host: "127.0.0.1",
            target_port: 22,
        })
        .await
        .unwrap();
        use tokio::io::AsyncReadExt;
        let mut s = tokio::net::TcpStream::connect(("127.0.0.1", t.local_port())).await.unwrap();
        let mut buf = [0u8; 4];
        s.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"SSH-");
    }
}
