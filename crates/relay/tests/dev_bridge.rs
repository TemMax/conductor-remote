//! The dev bridge: the rewritten head, the upgrade pass-through, bodies, the challenge, the
//! fallback to `[::1]` and the refusals.

use std::time::Duration;

use conductor_remote::dev::bridge::{bridge_matches, Bridge};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::time::timeout;

const OK: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello";

/// Reads a request head; returns it (with its empty line) and what followed it.
async fn read_head(stream: &mut TcpStream) -> (String, Vec<u8>) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        if let Some(at) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let rest = buf.split_off(at + 4);
            return (String::from_utf8(buf).unwrap(), rest);
        }
        let n = stream.read(&mut chunk).await.unwrap();
        assert!(n > 0, "the connection ended inside a head");
        buf.extend_from_slice(&chunk[..n]);
    }
}

/// A server that records each request head and answers `OK`.
async fn recording_server() -> (u16, mpsc::UnboundedReceiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            let tx = tx.clone();
            tokio::spawn(async move {
                let (head, _) = read_head(&mut stream).await;
                tx.send(head).unwrap();
                stream.write_all(OK).await.unwrap();
            });
        }
    });
    (port, rx)
}

async fn unused_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    listener.local_addr().unwrap().port()
}

/// Sends `request` to the bridge and reads until it closes.
async fn round_trip(port: u16, request: &[u8]) -> Vec<u8> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    stream.write_all(request).await.unwrap();
    let mut answer = Vec::new();
    timeout(Duration::from_secs(5), stream.read_to_end(&mut answer))
        .await
        .expect("the bridge closed the connection")
        .unwrap();
    answer
}

fn lines(head: &str) -> Vec<&str> {
    head.split("\r\n").filter(|l| !l.is_empty()).collect()
}

#[tokio::test]
async fn a_get_reaches_the_server_with_a_local_host_and_the_response_comes_back() {
    let (target, mut heads) = recording_server().await;
    let bridge = Bridge::open(target).await.unwrap();

    let answer = round_trip(
        bridge.port(),
        b"GET /app?x=1 HTTP/1.1\r\nHost: mac.tail1234.ts.net\r\nAccept: text/html\r\n\r\n",
    )
    .await;

    assert_eq!(answer, OK);
    let head = heads.recv().await.unwrap();
    assert_eq!(
        lines(&head),
        [
            "GET /app?x=1 HTTP/1.1".to_owned(),
            format!("Host: 127.0.0.1:{target}"),
            "X-Forwarded-Host: mac.tail1234.ts.net".to_owned(),
            "Accept: text/html".to_owned(),
            "Connection: close".to_owned(),
        ]
    );
    assert!(head.ends_with("\r\n\r\n"));
}

#[tokio::test]
async fn an_existing_x_forwarded_host_is_kept() {
    let (target, mut heads) = recording_server().await;
    let bridge = Bridge::open(target).await.unwrap();

    round_trip(
        bridge.port(),
        b"GET / HTTP/1.1\r\nhost: mac.ts.net\r\nx-forwarded-host: earlier.example\r\n\r\n",
    )
    .await;

    let head = heads.recv().await.unwrap();
    let forwarded: Vec<_> = lines(&head)
        .into_iter()
        .filter(|l| l.to_ascii_lowercase().starts_with("x-forwarded-host"))
        .collect();
    assert_eq!(forwarded, ["x-forwarded-host: earlier.example"]);
    assert!(lines(&head).contains(&format!("Host: 127.0.0.1:{target}").as_str()));
}

#[tokio::test]
async fn keep_alive_headers_give_way_to_connection_close() {
    let (target, mut heads) = recording_server().await;
    let bridge = Bridge::open(target).await.unwrap();

    round_trip(
        bridge.port(),
        b"GET / HTTP/1.1\r\nHost: mac.ts.net\r\nconnection: keep-alive\r\nKeep-Alive: timeout=5\r\nProxy-Connection: keep-alive\r\nAccept: */*\r\n\r\n",
    )
    .await;

    let head = heads.recv().await.unwrap();
    let ours: Vec<_> = lines(&head)
        .into_iter()
        .filter(|l| {
            let l = l.to_ascii_lowercase();
            l.starts_with("connection") || l.starts_with("keep-alive") || l.starts_with("proxy")
        })
        .collect();
    assert_eq!(ours, ["Connection: close"]);
    assert!(lines(&head).contains(&"Accept: */*"));
}

#[tokio::test]
async fn an_origin_is_rewritten() {
    let (target, mut heads) = recording_server().await;
    let bridge = Bridge::open(target).await.unwrap();

    round_trip(
        bridge.port(),
        b"GET / HTTP/1.1\r\nHost: mac.ts.net\r\nORIGIN: https://mac.ts.net\r\n\r\n",
    )
    .await;

    let head = heads.recv().await.unwrap();
    assert!(lines(&head).contains(&format!("Origin: http://127.0.0.1:{target}").as_str()));
    assert!(!head.contains("mac.ts.net\r\n\r\n") && !head.contains("https://"));
}

#[tokio::test]
async fn an_upgrade_keeps_its_headers_and_bytes_flow_both_ways() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let target = listener.local_addr().unwrap().port();
    let (tx, mut heads) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let (head, rest) = read_head(&mut stream).await;
        tx.send(head).unwrap();
        stream
            .write_all(b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n")
            .await
            .unwrap();
        // Echo what came with the head, then whatever follows.
        stream.write_all(&rest).await.unwrap();
        let mut chunk = [0u8; 64];
        loop {
            let n = stream.read(&mut chunk).await.unwrap();
            if n == 0 {
                break;
            }
            stream.write_all(&chunk[..n]).await.unwrap();
        }
    });
    let bridge = Bridge::open(target).await.unwrap();

    let mut client = TcpStream::connect(("127.0.0.1", bridge.port()))
        .await
        .unwrap();
    client
        .write_all(b"GET /ws HTTP/1.1\r\nHost: mac.ts.net\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\r\nearly")
        .await
        .unwrap();

    let head = heads.recv().await.unwrap();
    assert!(lines(&head).contains(&"Connection: Upgrade"));
    assert!(lines(&head).contains(&"Upgrade: websocket"));
    assert!(!head.contains("Connection: close"));
    assert!(lines(&head).contains(&format!("Host: 127.0.0.1:{target}").as_str()));

    let (answer, mut early) = read_head(&mut client).await;
    assert!(answer.starts_with("HTTP/1.1 101"));
    // The echo of the bytes sent with the head may have arrived together with the 101.
    while early.len() < 5 {
        let mut chunk = [0u8; 16];
        let n = timeout(Duration::from_secs(5), client.read(&mut chunk))
            .await
            .unwrap()
            .unwrap();
        assert!(n > 0);
        early.extend_from_slice(&chunk[..n]);
    }
    assert_eq!(early, b"early");

    client.write_all(b"ping").await.unwrap();
    let mut echoed = [0u8; 4];
    timeout(Duration::from_secs(5), client.read_exact(&mut echoed))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&echoed, b"ping");
}

#[tokio::test]
async fn a_request_body_arrives_complete() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let target = listener.local_addr().unwrap().port();
    let (tx, mut bodies) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let (_, mut body) = read_head(&mut stream).await;
        while body.len() < 10 {
            let mut chunk = [0u8; 64];
            let n = stream.read(&mut chunk).await.unwrap();
            assert!(n > 0);
            body.extend_from_slice(&chunk[..n]);
        }
        tx.send(body).unwrap();
        stream.write_all(OK).await.unwrap();
    });
    let bridge = Bridge::open(target).await.unwrap();

    let mut client = TcpStream::connect(("127.0.0.1", bridge.port()))
        .await
        .unwrap();
    client
        .write_all(b"POST /save HTTP/1.1\r\nHost: mac.ts.net\r\nContent-Length: 10\r\n\r\n0123")
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    client.write_all(b"456789").await.unwrap();

    let mut answer = Vec::new();
    timeout(Duration::from_secs(5), client.read_to_end(&mut answer))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(answer, OK);
    assert_eq!(bodies.recv().await.unwrap(), b"0123456789");
}

#[tokio::test]
async fn the_challenge_is_answered_for_the_token_only() {
    let (target, mut heads) = recording_server().await;
    let bridge = Bridge::open(target).await.unwrap();
    assert_eq!(bridge.token().len(), 64);
    assert!(bridge.token().bytes().all(|b| b.is_ascii_hexdigit()));

    assert!(bridge_matches(bridge.port(), bridge.token()).await);
    assert!(!bridge_matches(bridge.port(), &"0".repeat(64)).await);

    let answer = round_trip(
        bridge.port(),
        format!(
            "GET / HTTP/1.1\r\nHost: x\r\nX-Conductor-Remote-Bridge: {}\r\n\r\n",
            bridge.token()
        )
        .as_bytes(),
    )
    .await;
    let answer = String::from_utf8(answer).unwrap();
    assert!(answer.starts_with("HTTP/1.1 204 No Content\r\n"));
    assert!(answer.contains("Connection: close"));

    let answer = round_trip(
        bridge.port(),
        b"GET / HTTP/1.1\r\nHost: x\r\nx-conductor-remote-bridge: nope\r\n\r\n",
    )
    .await;
    assert!(String::from_utf8(answer)
        .unwrap()
        .starts_with("HTTP/1.1 403"));

    // Neither challenge reached the server.
    assert!(heads.try_recv().is_err());
}

#[tokio::test]
async fn bridge_matches_is_false_for_a_closed_port_and_after_close() {
    let (target, _heads) = recording_server().await;
    let bridge = Bridge::open(target).await.unwrap();
    let (port, token) = (bridge.port(), bridge.token().to_owned());
    assert!(bridge_matches(port, &token).await);

    bridge.close().await;
    assert!(!bridge_matches(port, &token).await);
    assert!(!bridge_matches(unused_port().await, &token).await);
}

#[tokio::test]
async fn dropping_the_bridge_ends_its_connections() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let target = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let _ = read_head(&mut stream).await;
        // Hold the connection open without answering.
        tokio::time::sleep(Duration::from_secs(60)).await;
    });
    let bridge = Bridge::open(target).await.unwrap();
    let mut client = TcpStream::connect(("127.0.0.1", bridge.port()))
        .await
        .unwrap();
    client
        .write_all(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n")
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;

    drop(bridge);

    let mut rest = Vec::new();
    let ended = timeout(Duration::from_secs(5), client.read_to_end(&mut rest)).await;
    assert!(ended.is_ok(), "the connection outlived the bridge");
}

#[tokio::test]
async fn an_unreachable_target_gives_502() {
    let bridge = Bridge::open(unused_port().await).await.unwrap();

    let answer = round_trip(bridge.port(), b"GET / HTTP/1.1\r\nHost: x\r\n\r\n").await;

    let answer = String::from_utf8(answer).unwrap();
    assert!(answer.starts_with("HTTP/1.1 502 Bad Gateway\r\n"));
    assert!(answer.contains("Connection: close"));
}

#[tokio::test]
async fn a_target_listening_only_on_ipv6_loopback_is_reached() {
    let Ok(listener) = TcpListener::bind("[::1]:0").await else {
        eprintln!("skipped: no IPv6 loopback");
        return;
    };
    let target = listener.local_addr().unwrap().port();
    let (tx, mut heads) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let (head, _) = read_head(&mut stream).await;
        tx.send(head).unwrap();
        stream.write_all(OK).await.unwrap();
    });
    let bridge = Bridge::open(target).await.unwrap();

    let answer = round_trip(bridge.port(), b"GET / HTTP/1.1\r\nHost: mac.ts.net\r\n\r\n").await;

    assert_eq!(answer, OK);
    let head = heads.recv().await.unwrap();
    assert!(lines(&head).contains(&format!("Host: 127.0.0.1:{target}").as_str()));
}

#[tokio::test]
async fn an_oversized_head_closes_the_connection() {
    let (target, mut heads) = recording_server().await;
    let bridge = Bridge::open(target).await.unwrap();

    let mut client = TcpStream::connect(("127.0.0.1", bridge.port()))
        .await
        .unwrap();
    let mut request = b"GET / HTTP/1.1\r\nHost: x\r\n".to_vec();
    while request.len() < 70 * 1024 {
        request.extend_from_slice(b"X-Filler: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\r\n");
    }
    // The bridge may close before everything is written.
    let _ = client.write_all(&request).await;

    let mut answer = Vec::new();
    let _ = timeout(Duration::from_secs(5), client.read_to_end(&mut answer))
        .await
        .expect("the bridge closed the connection");
    assert!(answer.is_empty());
    assert!(heads.try_recv().is_err());
}
