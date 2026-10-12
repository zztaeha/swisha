#![cfg(feature = "http")]

// What a caller can hold open. Each connection and each event stream holds a file, and a payout
// that cannot open one to Swish fails with its reference spent, so neither may grow unbounded.

mod common;

use std::time::Duration;

use common::{PAYER, SECRET};
use swisha::events::EventStream;
use swisha::http::{InternalSocket, Limits};
use swisha::state::SharedState;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

// Serves `state` as the binary does, with `limits`, until the sender is dropped.
async fn served(state: SharedState, internal: InternalSocket, limits: Limits) -> tokio::sync::oneshot::Sender<()> {
    let callback = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    serving(state, internal, callback, limits)
}

fn serving(
    state: SharedState,
    internal: InternalSocket,
    callback: TcpListener,
    limits: Limits,
) -> tokio::sync::oneshot::Sender<()> {
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let stopped = async {
        let _ = stopped.await;
    };
    tokio::spawn(swisha::http::serve(internal, callback, state, stopped, Duration::from_secs(1), limits));
    stop
}

async fn tcp() -> (InternalSocket, String) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr").to_string();
    (InternalSocket::Tcp(listener), addr)
}

// What arrives within `wait`: `None` while the connection stays open and unanswered, and an empty
// answer once it is closed.
async fn answer(stream: &mut (impl AsyncRead + Unpin), wait: Duration) -> Option<String> {
    let mut buffer = vec![0; 4096];
    match tokio::time::timeout(wait, stream.read(&mut buffer)).await {
        Err(_) => None,
        Ok(Ok(read)) => Some(String::from_utf8_lossy(&buffer[..read]).to_string()),
        Ok(Err(_)) => Some(String::new()),
    }
}

fn health() -> String {
    format!("GET /system/health HTTP/1.1\r\nHost: swisha\r\nx-api-secret: {SECRET}\r\nConnection: close\r\n\r\n")
}

// A caller without the secret, or with the secret and no key, is answered from its headers: a body
// it announces and never sends holds nothing open.
#[tokio::test]
async fn a_caller_without_the_secret_or_the_key_is_refused_before_its_body_is_read() {
    let base = common::serve(swisha::http::internal_router().with_state(common::state().await)).await;
    let addr = base.trim_start_matches("http://").to_string();
    let secret = format!("x-api-secret: {SECRET}\r\n");
    for (path, headers) in [
        ("/swish/payout", ""),
        ("/swish/payout", secret.as_str()),
        ("/swish/payout/test/R-1/redo", secret.as_str()),
        ("/swish/payout/test/R-1/confirm", secret.as_str()),
        ("/swish/payers", ""),
        ("/swish/payers/test/key", secret.as_str()),
    ] {
        let mut stream = TcpStream::connect(&addr).await.expect("connect");
        let head = format!(
            "POST {path} HTTP/1.1\r\nHost: swisha\r\nContent-Type: application/json\r\n{headers}Content-Length: 100000\r\n\r\n{{"
        );
        stream.write_all(head.as_bytes()).await.expect("send");
        let got = answer(&mut stream, Duration::from_secs(2)).await;
        assert!(got.as_deref().is_some_and(|a| a.starts_with("HTTP/1.1 401")), "{path} {headers:?}: {got:?}");
    }
}

// A connection that never finishes its headers, or never sends one, is closed once the header
// timeout passes, over a port and over the socket alike.
#[tokio::test]
async fn a_connection_that_never_finishes_its_headers_is_closed() {
    let limits = Limits { header_read_timeout: Duration::from_millis(200), ..Limits::default() };
    let (internal, addr) = tcp().await;
    let _stop = served(common::state().await, internal, limits).await;
    let mut partial = TcpStream::connect(&addr).await.expect("connect");
    partial.write_all(b"GET /system/health HTTP/1.1\r\nHost: swisha\r\n").await.expect("send");
    let mut silent = TcpStream::connect(&addr).await.expect("connect");
    assert!(answer(&mut partial, Duration::from_secs(3)).await.is_some(), "a partial header held its connection");
    assert!(answer(&mut silent, Duration::from_secs(3)).await.is_some(), "a silent connection was held");

    let dir = std::path::PathBuf::from(format!("/tmp/swisha-{}", &swisha::swish::random_payout_uuid()[..12]));
    swisha::util::fs::create_private_dir(&dir).expect("socket folder");
    let path = dir.join("s");
    let listener = swisha::http::bind_socket(&path).expect("bind");
    let _stop = served(common::state().await, InternalSocket::Unix(listener), limits).await;
    let mut partial = tokio::net::UnixStream::connect(&path).await.expect("connect");
    partial.write_all(b"GET /system/health HTTP/1.1\r\nHost: swisha\r\n").await.expect("send");
    assert!(answer(&mut partial, Duration::from_secs(3)).await.is_some(), "a partial header held the socket");
    let _ = std::fs::remove_dir_all(dir);
}

// A body that never finishes is cut off at the body deadline, on both listeners, so senders that
// stop midway cannot hold every connection the caps allow.
#[tokio::test]
async fn a_body_that_never_finishes_is_cut_off() {
    let limits = Limits {
        body_read_timeout: Duration::from_millis(300),
        internal_connections: 2,
        callback_connections: 2,
        ..Limits::default()
    };
    let (internal, internal_addr) = tcp().await;
    let callback = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let callback_addr = callback.local_addr().expect("addr").to_string();
    let _stop = serving(common::state().await, internal, callback, limits);
    let secret = format!("x-api-secret: {SECRET}\r\n");
    for (addr, path, headers) in [(&internal_addr, "/swish/payers", secret.as_str()), (&callback_addr, "/swish/callback", "")] {
        let mut stalled = Vec::new();
        for _ in 0..2 {
            let mut stream = TcpStream::connect(addr).await.expect("connect");
            let head = format!(
                "POST {path} HTTP/1.1\r\nHost: swisha\r\nContent-Type: application/json\r\n{headers}Content-Length: 1000\r\n\r\n{{"
            );
            stream.write_all(head.as_bytes()).await.expect("send");
            stalled.push(stream);
        }
        for stream in &mut stalled {
            assert!(answer(stream, Duration::from_secs(3)).await.is_some(), "{path}: a stalled body held its connection");
        }
        let mut next = TcpStream::connect(addr).await.expect("connect");
        next.write_all(b"GET /nothing HTTP/1.1\r\nHost: swisha\r\nConnection: close\r\n\r\n").await.expect("send");
        let got = answer(&mut next, Duration::from_secs(3)).await;
        assert!(got.as_deref().is_some_and(|a| a.starts_with("HTTP/1.1 404")), "{path}: {got:?}");
    }
}

// In production the callback refuses a caller outside Swish's addresses from its headers alone: a
// body it announces and never sends holds nothing open.
#[tokio::test]
async fn the_callback_refuses_a_caller_outside_swish_before_its_body_is_read() {
    let mut config = common::config();
    config.swish_env = "production".into();
    let state = common::state_with(config).await;
    let base = common::serve(swisha::http::callback_router().with_state(state)).await;
    let mut stream = TcpStream::connect(base.trim_start_matches("http://")).await.expect("connect");
    let head = "POST /swish/callback HTTP/1.1\r\nHost: swisha\r\nContent-Type: application/json\r\nContent-Length: 1000\r\n\r\n{";
    stream.write_all(head.as_bytes()).await.expect("send");
    let got = answer(&mut stream, Duration::from_secs(2)).await;
    assert!(got.as_deref().is_some_and(|a| a.starts_with("HTTP/1.1 403")), "{got:?}");
}

// A request's head over 16 KB is refused with 431 rather than buffered, so an unfinished one cannot
// hold more than that.
#[tokio::test]
async fn a_head_over_16_kb_is_refused() {
    let (internal, addr) = tcp().await;
    let _stop = served(common::state().await, internal, Limits::default()).await;
    let mut stream = TcpStream::connect(&addr).await.expect("connect");
    let filler = "x".repeat(20 * 1024);
    let head = format!("GET /system/health HTTP/1.1\r\nHost: swisha\r\nx-api-secret: {SECRET}\r\nx-filler: {filler}\r\n\r\n");
    let _ = stream.write_all(head.as_bytes()).await;
    let got = answer(&mut stream, Duration::from_secs(3)).await;
    assert!(got.as_deref().is_some_and(|a| a.starts_with("HTTP/1.1 431")), "{got:?}");
}

// Past the cap a connection waits in the kernel's queue until one held open closes.
#[tokio::test]
async fn a_connection_past_the_cap_waits_for_one_to_close() {
    let limits = Limits { internal_connections: 2, ..Limits::default() };
    let (internal, addr) = tcp().await;
    let _stop = served(common::state().await, internal, limits).await;
    let first = TcpStream::connect(&addr).await.expect("connect");
    let _second = TcpStream::connect(&addr).await.expect("connect");
    tokio::time::sleep(Duration::from_millis(200)).await;

    let mut third = TcpStream::connect(&addr).await.expect("the kernel queues it");
    third.write_all(health().as_bytes()).await.expect("send");
    assert_eq!(answer(&mut third, Duration::from_millis(500)).await, None, "answered past the cap");
    drop(first);
    let got = answer(&mut third, Duration::from_secs(3)).await;
    assert!(got.as_deref().is_some_and(|a| a.starts_with("HTTP/1.1 200")), "{got:?}");
}

// The callback listener is the one the internet reaches, and holds to limits of its own: a header
// sent too slowly is closed, and past its cap a connection waits for one to close.
#[tokio::test]
async fn the_callback_listener_closes_a_connection_that_never_finishes_its_headers() {
    let limits = Limits { header_read_timeout: Duration::from_millis(200), ..Limits::default() };
    let callback = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = callback.local_addr().expect("addr").to_string();
    let _stop = serving(common::state().await, tcp().await.0, callback, limits);
    let mut partial = TcpStream::connect(&addr).await.expect("connect");
    partial.write_all(b"POST /swish/callback HTTP/1.1\r\nHost: swisha\r\n").await.expect("send");
    assert!(answer(&mut partial, Duration::from_secs(3)).await.is_some(), "a partial header held its connection");
}

#[tokio::test]
async fn the_callback_listener_holds_no_connection_past_its_cap() {
    let limits = Limits { callback_connections: 2, ..Limits::default() };
    let callback = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = callback.local_addr().expect("addr").to_string();
    let _stop = serving(common::state().await, tcp().await.0, callback, limits);
    let first = TcpStream::connect(&addr).await.expect("connect");
    let _second = TcpStream::connect(&addr).await.expect("connect");
    tokio::time::sleep(Duration::from_millis(200)).await;

    let mut third = TcpStream::connect(&addr).await.expect("the kernel queues it");
    third.write_all(b"GET /nothing HTTP/1.1\r\nHost: swisha\r\nConnection: close\r\n\r\n").await.expect("send");
    assert_eq!(answer(&mut third, Duration::from_millis(500)).await, None, "answered past the cap");
    drop(first);
    let got = answer(&mut third, Duration::from_secs(3)).await;
    assert!(got.as_deref().is_some_and(|a| a.starts_with("HTTP/1.1 404")), "{got:?}");
}

// One number's key cannot hold every stream, and every number together cannot hold the files a
// payout needs. Past either cap a new stream is refused with 429, saying which.
#[tokio::test]
async fn event_streams_are_capped_per_number_and_in_all() {
    let numbers = vec![common::payer(PAYER, common::PAYER_NUMBER), common::payer("gbg", "1239990011")];
    let state = common::state_with_stream(common::config(), numbers, EventStream::with_caps(128, 2, 3)).await;
    let base = common::serve(swisha::http::internal_router().with_state(state.clone())).await;
    let open = |name: &str| {
        let request = reqwest::Client::new()
            .get(format!("{base}/events"))
            .header("x-api-secret", SECRET)
            .header(common::KEY_HEADER, common::key(name));
        async move { request.send().await.expect("answered") }
    };

    let first = open(PAYER).await;
    let _second = open(PAYER).await;
    assert_eq!((first.status().as_u16(), _second.status().as_u16()), (200, 200));
    let refused = open(PAYER).await;
    assert_eq!(refused.status().as_u16(), 429);
    let said = refused.text().await.expect("body");
    assert!(said.contains("this number"), "{said}");

    let other = open("gbg").await;
    assert_eq!(other.status().as_u16(), 200, "another number still has room");
    let refused = open("gbg").await;
    assert_eq!(refused.status().as_u16(), 429);
    let said = refused.text().await.expect("body");
    assert!(said.contains("every number"), "{said}");
    assert_eq!(state.stream.subscriber_count(), 3);
}
