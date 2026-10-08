//! The loopback bridge that makes a dev server accept requests arriving under the tailnet name.
//!
//! Dev servers such as Vite refuse a `Host` that is not local. The bridge listens on a loopback
//! port, rewrites the request head so it looks local and forwards the connection to the dev
//! server. It serves one request per connection (`Connection: close`) so it never has to frame an
//! HTTP body; an upgrade such as a WebSocket keeps its headers and its bytes flow both ways.

use std::time::Duration;

use subtle::ConstantTimeEq;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::timeout;

const CHALLENGE_HEADER: &str = "x-conductor-remote-bridge";
const MAX_HEAD: usize = 64 * 1024;
const HEAD_TIMEOUT: Duration = Duration::from_secs(10);
const CHALLENGE_TIMEOUT: Duration = Duration::from_millis(500);

/// A running bridge; dropping it or calling `close` stops accepting and ends its connections' tasks.
pub struct Bridge {
    port: u16,
    token: String,
    task: JoinHandle<()>,
}

impl Bridge {
    /// Listens on `127.0.0.1:0` and forwards to `127.0.0.1:<target_port>`.
    pub async fn open(target_port: u16) -> std::io::Result<Bridge> {
        let mut secret = [0u8; 32];
        getrandom::fill(&mut secret).map_err(|e| std::io::Error::other(e.to_string()))?;
        let token = hex(&secret);
        let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
        let port = listener.local_addr()?.port();
        let task = tokio::spawn(accept_loop(listener, target_port, token.clone()));
        Ok(Bridge { port, token, task })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// The secret that proves a bridge is this one (32 random bytes, hex).
    pub fn token(&self) -> &str {
        &self.token
    }

    pub async fn close(mut self) {
        self.task.abort();
        // The accept task owns the listener and the connection tasks; once it is gone they are too.
        let _ = (&mut self.task).await;
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Whether the bridge on `port` answers the challenge for `token` (within 500 ms).
pub async fn bridge_matches(port: u16, token: &str) -> bool {
    if token.is_empty() || token.bytes().any(|b| !b.is_ascii_graphic()) {
        return false;
    }
    let ask = async {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).await.ok()?;
        let request = format!(
            "GET / HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n{CHALLENGE_HEADER}: {token}\r\nConnection: close\r\n\r\n"
        );
        stream.write_all(request.as_bytes()).await.ok()?;
        let mut answer = Vec::new();
        // Read to the end: the bridge closes after its answer, anything else just times out.
        stream.read_to_end(&mut answer).await.ok()?;
        Some(answer.starts_with(b"HTTP/1.1 204"))
    };
    matches!(timeout(CHALLENGE_TIMEOUT, ask).await, Ok(Some(true)))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

async fn accept_loop(listener: TcpListener, target_port: u16, token: String) {
    // Dropping the set (when this task is aborted) aborts every connection task.
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    connections.spawn(serve(stream, target_port, token.clone()));
                }
                Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
            },
            Some(_) = connections.join_next(), if !connections.is_empty() => {}
        }
    }
}

enum Head {
    /// The request head and whatever followed it in the buffer.
    Request(Vec<u8>, Vec<u8>),
    /// Too large, too slow, or the peer went away.
    Refused,
}

async fn read_head(stream: &mut TcpStream) -> Head {
    let read = async {
        let mut buf: Vec<u8> = Vec::new();
        let mut chunk = [0u8; 8192];
        let mut scanned = 0;
        loop {
            if let Some(end) = head_end(&buf, scanned) {
                let rest = buf.split_off(end);
                return Head::Request(buf, rest);
            }
            if buf.len() >= MAX_HEAD {
                return Head::Refused;
            }
            // A '\n' near the end may still gain its successor, so rescan from a bit before.
            scanned = buf.len().saturating_sub(2);
            match stream.read(&mut chunk).await {
                Ok(0) | Err(_) => return Head::Refused,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
        }
    };
    match timeout(HEAD_TIMEOUT, read).await {
        Ok(Head::Request(head, rest)) if head.len() <= MAX_HEAD => Head::Request(head, rest),
        _ => Head::Refused,
    }
}

/// The offset just past the first empty line, looking only at line ends from `from` on.
fn head_end(buf: &[u8], from: usize) -> Option<usize> {
    let mut i = from;
    while i < buf.len() {
        if buf[i] == b'\n' && i > 0 {
            match (buf.get(i + 1), buf.get(i + 2)) {
                (Some(b'\n'), _) => return Some(i + 2),
                (Some(b'\r'), Some(b'\n')) => return Some(i + 3),
                _ => {}
            }
        }
        i += 1;
    }
    None
}

async fn serve(mut client: TcpStream, target_port: u16, token: String) {
    let Head::Request(head, rest) = read_head(&mut client).await else {
        return;
    };
    let lines = split_lines(&head);
    match challenge(&lines, &token) {
        Some(true) => {
            let _ = client
                .write_all(b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n")
                .await;
            let _ = client.shutdown().await;
            return;
        }
        Some(false) => {
            let _ = client.write_all(&status(403, "Forbidden")).await;
            let _ = client.shutdown().await;
            return;
        }
        None => {}
    }
    let Some(mut upstream) = connect(target_port).await else {
        let _ = client.write_all(&status(502, "Bad Gateway")).await;
        let _ = client.shutdown().await;
        return;
    };
    let forwarded = rewrite(&lines, target_port);
    if upstream.write_all(&forwarded).await.is_err() || upstream.write_all(&rest).await.is_err() {
        let _ = client.write_all(&status(502, "Bad Gateway")).await;
        let _ = client.shutdown().await;
        return;
    }
    let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
}

fn status(code: u16, reason: &str) -> Vec<u8> {
    format!("HTTP/1.1 {code} {reason}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
        .into_bytes()
}

async fn connect(port: u16) -> Option<TcpStream> {
    if let Ok(stream) = TcpStream::connect(("127.0.0.1", port)).await {
        return Some(stream);
    }
    TcpStream::connect(("::1", port)).await.ok()
}

/// The head cut into lines, each with its terminator.
fn split_lines(head: &[u8]) -> Vec<&[u8]> {
    head.split_inclusive(|&b| b == b'\n').collect()
}

/// A header line's name and trimmed value; `None` for a line that is not `name: value`.
fn header(line: &[u8]) -> Option<(&[u8], &[u8])> {
    let colon = line.iter().position(|&b| b == b':')?;
    let (name, value) = (&line[..colon], &line[colon + 1..]);
    Some((name.trim_ascii(), value.trim_ascii()))
}

fn terminator(line: &[u8]) -> &'static [u8] {
    if line.ends_with(b"\r\n") {
        b"\r\n"
    } else {
        b"\n"
    }
}

/// `Some(true)` when the head answers the challenge for `token`, `Some(false)` when it carries the
/// challenge header with another value, `None` for an ordinary request.
fn challenge(lines: &[&[u8]], token: &str) -> Option<bool> {
    let mut present = false;
    for line in lines.iter().skip(1) {
        let Some((name, value)) = header(line) else {
            continue;
        };
        if name.eq_ignore_ascii_case(CHALLENGE_HEADER.as_bytes()) {
            present = true;
            if bool::from(value.ct_eq(token.as_bytes())) {
                return Some(true);
            }
        }
    }
    present.then_some(false)
}

/// The head with `Host`, `X-Forwarded-Host`, `Origin` and `Connection` made fit for the dev server.
fn rewrite(lines: &[&[u8]], target_port: u16) -> Vec<u8> {
    let named = |line: &[u8], wanted: &str| {
        header(line).is_some_and(|(name, _)| name.eq_ignore_ascii_case(wanted.as_bytes()))
    };
    let body = &lines[1..];
    let upgrade = body.iter().any(|line| named(line, "upgrade"));
    let has_forwarded = body.iter().any(|line| named(line, "x-forwarded-host"));
    let original_host = body
        .iter()
        .find(|line| named(line, "host"))
        .and_then(|line| header(line))
        .map(|(_, value)| value);

    let mut forwarded_added = has_forwarded;
    let mut out = Vec::new();
    out.extend_from_slice(lines[0]);
    // The last line is the empty one that ends the head.
    let (headers, blank) = body.split_at(body.len().saturating_sub(1));
    for line in headers {
        let end = terminator(line);
        if named(line, "host") {
            out.extend_from_slice(format!("Host: 127.0.0.1:{target_port}").as_bytes());
            out.extend_from_slice(end);
            if !forwarded_added {
                if let Some(original) = original_host {
                    out.extend_from_slice(b"X-Forwarded-Host: ");
                    out.extend_from_slice(original);
                    out.extend_from_slice(end);
                    forwarded_added = true;
                }
            }
        } else if named(line, "origin") {
            out.extend_from_slice(format!("Origin: http://127.0.0.1:{target_port}").as_bytes());
            out.extend_from_slice(end);
        } else if !upgrade
            && (named(line, "connection")
                || named(line, "keep-alive")
                || named(line, "proxy-connection"))
        {
            continue;
        } else {
            out.extend_from_slice(line);
        }
    }
    if !upgrade {
        out.extend_from_slice(b"Connection: close\r\n");
    }
    for line in blank {
        out.extend_from_slice(line);
    }
    out
}
