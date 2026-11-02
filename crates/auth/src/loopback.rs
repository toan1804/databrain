//! Single-use HTTP listener on the loopback interface that receives OAuth /
//! SSO redirects from the system browser (RFC 8252 §7.3).

use std::collections::HashMap;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;

use crate::AuthError;

const MAX_REQUEST: usize = 64 * 1024;

pub struct Loopback {
    listener: TcpListener,
    /// Host name used in the redirect URI (`127.0.0.1` or `localhost`).
    host: String,
    path: String,
    /// Web origins that may call the listener from a page (CORS): exact
    /// (`https://x.example.com`) or a domain suffix (`.snowflakecomputing.com`,
    /// https only). Empty: only plain browser redirects (no `Origin`).
    origins: Vec<String>,
}

impl Loopback {
    /// Bind to `127.0.0.1:port` (`port = 0` picks a free port).
    pub async fn bind(host: &str, port: u16, path: &str) -> Result<Self, AuthError> {
        let listener = TcpListener::bind(("127.0.0.1", port))
            .await
            .map_err(|e| AuthError::Flow(format!("cannot listen on 127.0.0.1:{port} for the sign-in redirect: {e}")))?;
        Ok(Self { listener, host: host.to_string(), path: if path.starts_with('/') { path.to_string() } else { format!("/{path}") }, origins: vec![] })
    }

    /// Let pages of these origins call the listener (see `origins`).
    pub fn allow_origins(mut self, origins: &[&str]) -> Self {
        self.origins = origins.iter().map(|o| o.to_string()).collect();
        self
    }

    pub fn port(&self) -> u16 {
        self.listener.local_addr().map(|a| a.port()).unwrap_or(0)
    }

    pub fn redirect_uri(&self) -> String {
        let path = if self.path == "/" { "" } else { self.path.as_str() };
        format!("http://{}:{}{}", self.host, self.port(), path)
    }

    /// Wait for the browser to hit the redirect path. Returns query (GET) or
    /// form body (POST) parameters. Requests with a wrong `state` are rejected
    /// and ignored so a stray/malicious request cannot abort the flow.
    pub async fn wait(self, expected_state: Option<&str>, cancel: &CancellationToken, timeout: Duration) -> Result<HashMap<String, String>, AuthError> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let accept = tokio::select! {
                r = self.listener.accept() => r,
                _ = cancel.cancelled() => return Err(AuthError::Cancelled),
                _ = tokio::time::sleep_until(deadline) => {
                    return Err(AuthError::Flow("timed out waiting for the browser sign-in".into()))
                }
            };
            let Ok((stream, _)) = accept else { continue };
            let port = self.port();
            match handle(stream, &self.path, expected_state, port, &self.origins).await {
                Some(params) => return Ok(params),
                None => continue,
            }
        }
    }
}

/// `Host` names this listener (a page on another name that resolves to
/// 127.0.0.1, DNS rebinding, is refused).
fn host_ok(host: Option<&str>, port: u16) -> bool {
    host.is_some_and(|h| [format!("127.0.0.1:{port}"), format!("localhost:{port}"), format!("[::1]:{port}")].iter().any(|ok| h.eq_ignore_ascii_case(ok)))
}

/// The `Origin` a page called from, when the flow allows it.
fn allowed_origin<'a>(origin: &'a str, allowed: &[String]) -> Option<&'a str> {
    allowed
        .iter()
        .any(|a| {
            if let Some(suffix) = a.strip_prefix('.') {
                origin
                    .strip_prefix("https://")
                    .is_some_and(|h| h.len() > suffix.len() && h.to_ascii_lowercase().ends_with(&format!(".{}", suffix.to_ascii_lowercase())))
            } else {
                origin.eq_ignore_ascii_case(a)
            }
        })
        .then_some(origin)
}

async fn handle(mut stream: TcpStream, path: &str, expected_state: Option<&str>, port: u16, origins: &[String]) -> Option<HashMap<String, String>> {
    let mut buf = Vec::with_capacity(2048);
    let mut tmp = [0u8; 2048];
    let header_end = loop {
        let n = tokio::time::timeout(Duration::from_secs(10), stream.read(&mut tmp)).await.ok()?.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(p) = find(&buf, b"\r\n\r\n") {
            break p;
        }
        if buf.len() > MAX_REQUEST {
            return None;
        }
    };
    let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let mut lines = head.split("\r\n");
    let request_line = lines.next()?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?.to_ascii_uppercase();
    let target = parts.next()?;
    let headers: Vec<(String, String)> = lines.filter_map(|l| l.split_once(':')).map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string())).collect();
    let header = |name: &str| headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str());
    let content_length: usize = header("content-length").and_then(|v| v.parse().ok()).unwrap_or(0).min(MAX_REQUEST);

    // Only this listener's own name, and pages of the origins the flow allows.
    if !host_ok(header("host"), port) {
        respond(&mut stream, 403, "Forbidden", None).await;
        return None;
    }
    let cors = match header("origin") {
        None => None,
        Some(o) => match allowed_origin(o, origins) {
            Some(o) => Some(o.to_string()),
            None => {
                respond(&mut stream, 403, "Forbidden", None).await;
                return None;
            }
        },
    };
    let (req_path, query) = target.split_once('?').unwrap_or((target, ""));
    if method == "OPTIONS" {
        // CORS preflight from an allowed page (Snowflake's SSO page).
        let allow = cors.as_deref().map(|o| format!("Access-Control-Allow-Origin: {o}\r\nVary: Origin\r\n")).unwrap_or_default();
        let _ = stream
            .write_all(format!("HTTP/1.1 204 No Content\r\n{allow}Access-Control-Allow-Methods: GET, POST\r\nAccess-Control-Allow-Headers: Content-Type\r\nContent-Length: 0\r\n\r\n").as_bytes())
            .await;
        return None;
    }
    if req_path != path {
        respond(&mut stream, 404, "Not found", cors.as_deref()).await;
        return None;
    }
    let mut params: HashMap<String, String> = url::form_urlencoded::parse(query.as_bytes()).into_owned().collect();
    if method == "POST" && content_length > 0 {
        let mut body = buf[header_end + 4..].to_vec();
        while body.len() < content_length {
            let n = tokio::time::timeout(Duration::from_secs(10), stream.read(&mut tmp)).await.ok()?.ok()?;
            if n == 0 {
                break;
            }
            body.extend_from_slice(&tmp[..n]);
        }
        body.truncate(content_length);
        params.extend(url::form_urlencoded::parse(&body).into_owned());
    }
    if let Some(expected) = expected_state {
        if params.get("state").map(String::as_str) != Some(expected) {
            respond(&mut stream, 400, "Invalid sign-in response (state mismatch).", cors.as_deref()).await;
            return None;
        }
    }
    if params.contains_key("error") {
        let msg = params.get("error_description").or(params.get("error")).cloned().unwrap_or_default();
        respond(&mut stream, 400, &format!("Sign-in failed: {msg}"), cors.as_deref()).await;
    } else {
        respond(&mut stream, 200, "You're signed in to DataBrain. You can close this tab and return to the app.", cors.as_deref()).await;
    }
    Some(params)
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// `cors`: the allowed page origin to name in the response (never `*`).
async fn respond(stream: &mut TcpStream, status: u16, message: &str, cors: Option<&str>) {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        _ => "Not Found",
    };
    let allow = cors.map(|o| format!("Access-Control-Allow-Origin: {o}\r\nVary: Origin\r\n")).unwrap_or_default();
    let body = format!(
        "<!doctype html><html><head><meta charset=utf-8><title>DataBrain</title>\
         <style>body{{font-family:system-ui,sans-serif;background:#0e0f13;color:#e6e7eb;display:flex;\
         align-items:center;justify-content:center;height:100vh;margin:0}}div{{max-width:420px;text-align:center}}\
         h1{{font-size:18px}}</style></head><body><div><h1>DataBrain</h1><p>{}</p></div></body></html>",
        html_escape(message)
    );
    let resp = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\n\
         {allow}Connection: close\r\nCache-Control: no-store\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(resp.as_bytes()).await;
    let _ = stream.shutdown().await;
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn send(port: u16, method: &str, target: &str, headers: &str) -> String {
        let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        s.write_all(format!("{method} {target} HTTP/1.1\r\n{headers}\r\n").as_bytes()).await.unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).await.unwrap();
        out
    }

    async fn get(port: u16, target: &str) -> String {
        send(port, "GET", target, &format!("Host: 127.0.0.1:{port}\r\n")).await
    }

    /// Only this listener's own Host; pages only from allowed origins, named
    /// in CORS (never `*`).
    #[tokio::test]
    async fn checks_host_and_origin() {
        let lb = Loopback::bind("localhost", 0, "/").await.unwrap().allow_origins(&[".snowflakecomputing.com"]);
        let port = lb.port();
        let cancel = CancellationToken::new();
        let waiter = tokio::spawn(async move { lb.wait(None, &cancel, Duration::from_secs(5)).await });
        // DNS rebinding: another name pointing at 127.0.0.1.
        assert!(send(port, "GET", "/?token=evil", &format!("Host: evil.example:{port}\r\n")).await.starts_with("HTTP/1.1 403"));
        assert!(send(port, "GET", "/?token=evil", "").await.starts_with("HTTP/1.1 403"), "no Host");
        // A page of another site.
        let r = send(port, "OPTIONS", "/", &format!("Host: localhost:{port}\r\nOrigin: https://evil.example\r\n")).await;
        assert!(r.starts_with("HTTP/1.1 403") && !r.contains("Access-Control-Allow-Origin"), "{r}");
        let r = send(port, "OPTIONS", "/", &format!("Host: localhost:{port}\r\nOrigin: https://snowflakecomputing.com.evil.example\r\n")).await;
        assert!(r.starts_with("HTTP/1.1 403"), "suffix tricks: {r}");
        // Snowflake's page: preflight and call, its origin named.
        let ok = "https://acme.eu-west-1.snowflakecomputing.com";
        let r = send(port, "OPTIONS", "/", &format!("Host: localhost:{port}\r\nOrigin: {ok}\r\n")).await;
        assert!(r.starts_with("HTTP/1.1 204") && r.contains(&format!("Access-Control-Allow-Origin: {ok}")) && !r.contains("Origin: *"), "{r}");
        let r = send(port, "GET", "/?token=t1", &format!("Host: localhost:{port}\r\nOrigin: {ok}\r\n")).await;
        assert!(r.starts_with("HTTP/1.1 200") && r.contains(&format!("Access-Control-Allow-Origin: {ok}")), "{r}");
        assert_eq!(waiter.await.unwrap().unwrap()["token"], "t1");
    }

    /// A flow without allowed origins takes only plain browser redirects.
    #[tokio::test]
    async fn no_origin_allowed_by_default() {
        let lb = Loopback::bind("127.0.0.1", 0, "/cb").await.unwrap();
        let port = lb.port();
        let cancel = CancellationToken::new();
        let waiter = tokio::spawn(async move { lb.wait(Some("s"), &cancel, Duration::from_secs(5)).await });
        let r = send(port, "GET", "/cb?code=x&state=s", &format!("Host: 127.0.0.1:{port}\r\nOrigin: https://any.example\r\n")).await;
        assert!(r.starts_with("HTTP/1.1 403"), "{r}");
        assert!(!get(port, "/cb?code=c&state=s").await.contains("Access-Control-Allow-Origin"));
        assert_eq!(waiter.await.unwrap().unwrap()["code"], "c");
    }

    #[tokio::test]
    async fn receives_code_and_rejects_bad_state() {
        let lb = Loopback::bind("127.0.0.1", 0, "/callback").await.unwrap();
        let port = lb.port();
        assert_eq!(lb.redirect_uri(), format!("http://127.0.0.1:{port}/callback"));
        let cancel = CancellationToken::new();
        let waiter = tokio::spawn(async move { lb.wait(Some("s1"), &cancel, Duration::from_secs(5)).await });
        assert!(get(port, "/other").await.starts_with("HTTP/1.1 404"));
        assert!(get(port, "/callback?code=x&state=evil").await.starts_with("HTTP/1.1 400"));
        assert!(get(port, "/callback?code=abc&state=s1").await.starts_with("HTTP/1.1 200"));
        let params = waiter.await.unwrap().unwrap();
        assert_eq!(params["code"], "abc");
    }

    #[tokio::test]
    async fn cancel_stops_waiting() {
        let lb = Loopback::bind("localhost", 0, "/").await.unwrap();
        let cancel = CancellationToken::new();
        cancel.cancel();
        let r = lb.wait(None, &cancel, Duration::from_secs(5)).await;
        assert!(matches!(r, Err(AuthError::Cancelled)));
    }
}
