//! Opening a connection: TCP, SQL Browser and login.

use crate::error::map_err;
use databrain_auth::Credential;
use databrain_auth::CredentialSource;
use databrain_connector_core::ConnectorError;
use databrain_connector_core::Result;
use secrecy::ExposeSecret;
use std::sync::Arc;
use std::time::Duration;
use tiberius::AuthMethod as TdsAuth;
use tiberius::Client;
use tiberius::Config;
use tiberius::SqlBrowser;
use tokio::net::TcpStream;
use tokio_util::compat::Compat;
use tokio_util::compat::TokioAsyncWriteCompatExt;

pub(crate) type Conn = Client<Compat<TcpStream>>;

pub(crate) async fn open(c: &Config, use_browser: bool) -> Result<Conn> {
    let fut = async {
        let tcp = if use_browser {
            TcpStream::connect_named(c).await.map_err(map_err)?
        } else {
            TcpStream::connect(c.get_addr()).await.map_err(|e| ConnectorError::connection(e.to_string()))?
        };
        tcp.set_nodelay(true).ok();
        Client::connect(c.clone(), tcp.compat_write()).await.map_err(|e| {
            let mut e = map_err(e);
            e.kind = databrain_connector_core::ErrorKind::Connection;
            e
        })
    };
    tokio::time::timeout(Duration::from_secs(20), fut).await.map_err(|_| ConnectorError::connection("connection timed out"))?
}

/// How a session (re)connects: the server settings, and where the login
/// comes from. The credential is asked for at each connection, not kept: an
/// Entra token expires after about an hour, and the source refreshes it.
pub(crate) struct Opener {
    /// Everything but the login (set per connection).
    pub(crate) config: Config,
    pub(crate) use_browser: bool,
    pub(crate) creds: Arc<dyn CredentialSource>,
}

impl Opener {
    /// A new connection with a credential asked for now. A token the server
    /// refuses (expired or revoked: login failed) is invalidated and asked
    /// for once more; a password isn't tried twice (a second failed login
    /// counts against lockout policies).
    pub(crate) async fn open(&self) -> Result<Conn> {
        self.open_by(|c| async move { open(&c, self.use_browser).await }).await
    }

    /// [`Self::open`] with `connect` making the connection from a config
    /// with the login set (tests give their own).
    pub(crate) async fn open_by<T, F, Fut>(&self, connect: F) -> Result<T>
    where
        F: Fn(Config) -> Fut,
        Fut: std::future::Future<Output = Result<T>>,
    {
        let with = |cred: Credential| {
            let mut c = self.config.clone();
            tds_auth(cred).map(|a| {
                c.authentication(a);
                c
            })
        };
        let cred = self.creds.get().await?;
        let token = matches!(cred, Credential::Bearer { .. });
        match connect(with(cred)?).await {
            Err(e) if token && login_refused(&e) => {
                self.creds.invalidate().await;
                connect(with(self.creds.get().await?)?).await
            }
            r => r,
        }
    }
}

/// The server refused the login (error 18456: a wrong password, an expired
/// or revoked token).
pub(crate) fn login_refused(e: &ConnectorError) -> bool {
    e.code.as_deref().is_some_and(|c| c == "18456" || c.starts_with("18456 "))
}

/// The login for a credential.
pub(crate) fn tds_auth(cred: Credential) -> Result<TdsAuth> {
    Ok(match cred {
        #[cfg(windows)]
        Credential::Password { user, password } if user.contains('\\') => TdsAuth::windows(&user, password.expose_secret()),
        #[cfg(not(windows))]
        Credential::Password { user, .. } if user.contains('\\') => {
            return Err(ConnectorError::config("Windows (DOMAIN\\user) logins are only supported on Windows; use a SQL login or Entra ID"));
        }
        Credential::Password { user, password } => TdsAuth::sql_server(user, password.expose_secret()),
        Credential::Bearer { token, .. } => TdsAuth::aad_token(token.expose_secret()),
        _ => return Err(ConnectorError::config("unsupported authentication method for SQL Server")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use databrain_auth::{AuthError, AuthStatus};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Hands out `token-1`, `token-2`… (or a password), counting asks and invalidations.
    struct Counting {
        token: bool,
        gets: AtomicUsize,
        invalidated: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl CredentialSource for Counting {
        async fn get(&self) -> std::result::Result<Credential, AuthError> {
            let n = self.gets.fetch_add(1, Ordering::SeqCst) + 1;
            Ok(if self.token {
                Credential::Bearer { token: format!("token-{n}").into(), user: None }
            } else {
                Credential::Password { user: "sa".into(), password: "pw".to_string().into() }
            })
        }
        async fn invalidate(&self) {
            self.invalidated.fetch_add(1, Ordering::SeqCst);
        }
        fn status(&self) -> AuthStatus {
            AuthStatus::default()
        }
    }

    fn opener(token: bool) -> (Opener, Arc<Counting>) {
        let creds = Arc::new(Counting { token, gets: AtomicUsize::new(0), invalidated: AtomicUsize::new(0) });
        (Opener { config: Config::new(), use_browser: false, creds: creds.clone() }, creds)
    }

    fn refused() -> ConnectorError {
        ConnectorError::connection("Login failed for user '<token-identified principal>'.").with_code("18456 (line 1)")
    }

    /// Each connection asks for the credential: a reconnect an hour later
    /// gets a fresh Entra token, not the one the session started with.
    #[tokio::test]
    async fn each_connection_asks_for_the_credential() {
        let (o, creds) = opener(true);
        for _ in 0..3 {
            tokio::time::timeout(Duration::from_secs(5), o.open_by(|_c| async { Ok(()) })).await.unwrap().unwrap();
        }
        assert_eq!(creds.gets.load(Ordering::SeqCst), 3);
        assert_eq!(creds.invalidated.load(Ordering::SeqCst), 0);
    }

    /// A token the server refuses is invalidated and asked for once more;
    /// a refused password isn't tried again, and other errors aren't retried.
    #[tokio::test]
    async fn a_refused_token_is_fetched_again_a_refused_password_is_not_retried() {
        let (o, creds) = opener(true);
        let tries = AtomicUsize::new(0);
        let r = o.open_by(|_c| {
            let n = tries.fetch_add(1, Ordering::SeqCst);
            async move { if n == 0 { Err(refused()) } else { Ok(()) } }
        });
        tokio::time::timeout(Duration::from_secs(5), r).await.unwrap().unwrap();
        assert_eq!((tries.load(Ordering::SeqCst), creds.gets.load(Ordering::SeqCst), creds.invalidated.load(Ordering::SeqCst)), (2, 2, 1));

        let (o, creds) = opener(false);
        let tries = AtomicUsize::new(0);
        let r = o.open_by(|_c| {
            tries.fetch_add(1, Ordering::SeqCst);
            async { Err::<(), _>(refused()) }
        });
        assert!(login_refused(&tokio::time::timeout(Duration::from_secs(5), r).await.unwrap().unwrap_err()));
        assert_eq!((tries.load(Ordering::SeqCst), creds.invalidated.load(Ordering::SeqCst)), (1, 0), "one login attempt with a password");

        let (o, creds) = opener(true);
        let tries = AtomicUsize::new(0);
        let r = o.open_by(|_c| {
            tries.fetch_add(1, Ordering::SeqCst);
            async { Err::<(), _>(ConnectorError::connection("connection refused")) }
        });
        assert!(tokio::time::timeout(Duration::from_secs(5), r).await.unwrap().is_err());
        assert_eq!((tries.load(Ordering::SeqCst), creds.invalidated.load(Ordering::SeqCst)), (1, 0), "not a login refusal: not retried");
    }

    #[test]
    fn login_refusal_is_error_18456() {
        assert!(login_refused(&refused()));
        assert!(!login_refused(&ConnectorError::query("Invalid object name 't'.").with_code("208 (line 1)")));
        assert!(!login_refused(&ConnectorError::query("x").with_code("184560 (line 1)")));
    }
}
