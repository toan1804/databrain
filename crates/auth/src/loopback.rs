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
}

impl Loopback {
    /// Bind to `127.0.0.1:port` (`port = 0` picks a free port).
    pub async fn bind(host: &str, port: u16, path: &str) -> Result<Self, AuthError> {
        let listener = TcpListener::bind(("127.0.0.1", port)).await.map_err(|e| {
            AuthError::Flow(format!(
                "cannot listen on 127.0.0.1:{port} for the sign-in redirect: {e}"
            ))
        })?;
        Ok(Self {
            listener,
            host: host.to_string(),
            path: if path.starts_with('/') {
                path.to_string()
            } else {
                format!("/{path}")
            },
        })
    }

    pub fn port(&self) -> u16 {
        self.listener.local_addr().map(|a| a.port()).unwrap_or(0)
    }

    pub fn redirect_uri(&self) -> String {
        let path = if self.path == "/" {
            ""
        } else {
            self.path.as_str()
        };
        format!("http://{}:{}{}", self.host, self.port(), path)
    }

    /// Wait for the browser to hit the redirect path. Returns query (GET) or
    /// form body (POST) parameters. Requests with a wrong `state` are rejected
    /// and ignored so a stray/malicious request cannot abort the flow.
    pub async fn wait(
        self,
        expected_state: Option<&str>,
        cancel: &CancellationToken,
        timeout: Duration,
    ) -> Result<HashMap<String, String>, AuthError> {
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
            match handle(stream, &self.path, expected_state).await {
                Some(params) => return Ok(params),
                None => continue,
            }
        }
    }
}

async fn handle(
    mut stream: TcpStream,
    path: &str,
    expected_state: Option<&str>,
) -> Option<HashMap<String, String>> {
    let mut buf = Vec::with_capacity(2048);
    let mut tmp = [0u8; 2048];
    let header_end = loop {
        let n = tokio::time::timeout(Duration::from_secs(10), stream.read(&mut tmp))
            .await
            .ok()?
            .ok()?;
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
    let content_length: usize = lines
        .filter_map(|l| l.split_once(':'))
        .find(|(k, _)| k.trim().eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.trim().parse().ok())
        .unwrap_or(0)
        .min(MAX_REQUEST);

    let (req_path, query) = target.split_once('?').unwrap_or((target, ""));
    if method == "OPTIONS" {
        // CORS preflight (Snowflake SSO page may POST via fetch).
        let _ = stream
            .write_all(b"HTTP/1.1 204 No Content\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Allow-Methods: GET, POST\r\nAccess-Control-Allow-Headers: *\r\nContent-Length: 0\r\n\r\n")
            .await;
        return None;
    }
    if req_path != path {
        respond(&mut stream, 404, "Not found").await;
        return None;
    }
    let mut params: HashMap<String, String> = url::form_urlencoded::parse(query.as_bytes())
        .into_owned()
        .collect();
    if method == "POST" && content_length > 0 {
        let mut body = buf[header_end + 4..].to_vec();
        while body.len() < content_length {
            let n = tokio::time::timeout(Duration::from_secs(10), stream.read(&mut tmp))
                .await
                .ok()?
                .ok()?;
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
            respond(
                &mut stream,
                400,
                "Invalid sign-in response (state mismatch).",
            )
            .await;
            return None;
        }
    }
    if params.contains_key("error") {
        let msg = params
            .get("error_description")
            .or(params.get("error"))
            .cloned()
            .unwrap_or_default();
        respond(&mut stream, 400, &format!("Sign-in failed: {msg}")).await;
    } else {
        respond(
            &mut stream,
            200,
            "You're signed in to DataBrain. You can close this tab and return to the app.",
        )
        .await;
    }
    Some(params)
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

async fn respond(stream: &mut TcpStream, status: u16, message: &str) {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        _ => "Not Found",
    };
    let body = format!(
        "<!doctype html><html><head><meta charset=utf-8><title>DataBrain</title>\
         <style>body{{font-family:system-ui,sans-serif;background:#0e0f13;color:#e6e7eb;display:flex;\
         align-items:center;justify-content:center;height:100vh;margin:0}}div{{max-width:420px;text-align:center}}\
         h1{{font-size:18px}}</style></head><body><div><h1>DataBrain</h1><p>{}</p></div></body></html>",
        html_escape(message)
    );
    let resp = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\n\
         Access-Control-Allow-Origin: *\r\nConnection: close\r\nCache-Control: no-store\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(resp.as_bytes()).await;
    let _ = stream.shutdown().await;
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn get(port: u16, target: &str) -> String {
        let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        s.write_all(format!("GET {target} HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes())
            .await
            .unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).await.unwrap();
        out
    }

    #[tokio::test]
    async fn receives_code_and_rejects_bad_state() {
        let lb = Loopback::bind("127.0.0.1", 0, "/callback").await.unwrap();
        let port = lb.port();
        assert_eq!(
            lb.redirect_uri(),
            format!("http://127.0.0.1:{port}/callback")
        );
        let cancel = CancellationToken::new();
        let waiter =
            tokio::spawn(async move { lb.wait(Some("s1"), &cancel, Duration::from_secs(5)).await });
        assert!(get(port, "/other").await.starts_with("HTTP/1.1 404"));
        assert!(
            get(port, "/callback?code=x&state=evil")
                .await
                .starts_with("HTTP/1.1 400")
        );
        assert!(
            get(port, "/callback?code=abc&state=s1")
                .await
                .starts_with("HTTP/1.1 200")
        );
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
