//! A scripted HTTP server on localhost for the connectors' polling tests.
//! `answer(method, path, body)` returns a status and a body; status 0
//! closes the connection without answering (a network error).

use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Requests received: method, path (with its query), body.
pub type Log = Arc<Mutex<Vec<(String, String, String)>>>;

pub async fn serve(answer: impl Fn(&str, &str, &str) -> (u16, String) + Send + Sync + 'static) -> (String, Log) {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", l.local_addr().unwrap());
    let log: Log = Arc::default();
    let (lg, answer) = (log.clone(), Arc::new(answer));
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = l.accept().await else { break };
            let (lg, answer) = (lg.clone(), answer.clone());
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut tmp = [0u8; 8192];
                let (head, body) = loop {
                    let n = sock.read(&mut tmp).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                    if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&buf[..i]).to_string();
                        let len = head
                            .lines()
                            .find_map(|h| h.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap_or(0)))
                            .unwrap_or(0);
                        while buf.len() < i + 4 + len {
                            let n = sock.read(&mut tmp).await.unwrap_or(0);
                            if n == 0 {
                                break;
                            }
                            buf.extend_from_slice(&tmp[..n]);
                        }
                        break (head, String::from_utf8_lossy(&buf[i + 4..]).to_string());
                    }
                };
                let mut first = head.lines().next().unwrap_or("").split_whitespace();
                let (method, path) = (first.next().unwrap_or("").to_string(), first.next().unwrap_or("").to_string());
                lg.lock().unwrap().push((method.clone(), path.clone(), body.clone()));
                let (status, out) = answer(&method, &path, &body);
                if status == 0 {
                    return;
                }
                let head = format!("HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n", out.len());
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock.write_all(out.as_bytes()).await;
            });
        }
    });
    (origin, log)
}
