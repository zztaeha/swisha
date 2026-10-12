#![cfg(feature = "http")]

// The internal API over a Unix socket, whose permission bits are its whole authentication: bound
// as the binary binds it, and served as the binary serves it.

mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::Duration;

use common::{MockSwish, PHONE, SECRET, SSN};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

// A private folder of its own. Short, since a socket path is capped near 104 bytes and the
// system temporary folder alone can take half of that.
fn folder() -> PathBuf {
    let dir = PathBuf::from(format!("/tmp/swisha-{}", &swisha::swish::random_payout_uuid()[..12]));
    swisha::util::fs::create_private_dir(&dir).expect("socket folder");
    dir
}

fn mode(path: &std::path::Path) -> u32 {
    std::fs::metadata(path).expect("metadata").permissions().mode() & 0o777
}

// Bound under the test's own umask, 022, which would leave a new socket 0755 and so refused: only
// the mode swisha sets makes it 0660, and the payout route answers over it.
#[tokio::test]
async fn the_socket_is_bound_0660_and_serves_the_payout_route() {
    let swish = MockSwish::new().accepts().resolves_to("PAID");
    let mut config = common::config();
    config.swish_base_url = swish.clone().start().await;
    let state = common::state_with(config).await;
    let dir = folder();
    let path = dir.join("s");

    let listener = swisha::http::bind_socket(&path).expect("bind");
    assert_eq!(mode(&path), 0o660, "the group may connect, nobody else");

    let callback = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("callback listener");
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let serving = tokio::spawn(swisha::http::serve(
        swisha::http::InternalSocket::Unix(listener),
        callback,
        state,
        async {
            let _ = stopped.await;
        },
        Duration::from_secs(1),
        swisha::http::Limits::default(),
    ));

    let body = serde_json::json!({
        "reference": "SOCK-1", "payee_alias": PHONE, "payee_ssn": SSN, "amount": 100.0,
    })
    .to_string();
    let request = format!(
        "POST /swish/payout HTTP/1.1\r\nHost: swisha\r\nContent-Type: application/json\r\n\
         x-api-secret: {SECRET}\r\n{}: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        common::KEY_HEADER,
        common::key(common::PAYER),
        body.len()
    );
    let mut stream = tokio::net::UnixStream::connect(&path).await.expect("connect");
    stream.write_all(request.as_bytes()).await.expect("send");
    let mut answer = String::new();
    stream.read_to_string(&mut answer).await.expect("answer");
    assert!(answer.starts_with("HTTP/1.1 202"), "{answer}");
    assert_eq!(swish.posted().len(), 1, "the payout reached Swish over the socket");

    let _ = stop.send(());
    serving.await.expect("join").expect("served until stopped");
    let _ = std::fs::remove_dir_all(dir);
}

// Only a stale socket may be replaced: anything else at the path is left as it was.
#[tokio::test]
async fn anything_but_a_stale_socket_at_the_path_is_left_alone() {
    let dir = folder();
    let path = dir.join("s");
    std::fs::write(&path, "not a socket").expect("write");
    let error = swisha::http::bind_socket(&path).expect_err("a regular file").to_string();
    assert!(error.contains("is not a socket"), "{error}");
    assert_eq!(std::fs::read_to_string(&path).expect("read"), "not a socket");

    std::fs::remove_file(&path).expect("remove");
    drop(std::os::unix::net::UnixListener::bind(&path).expect("a socket an unclean stop left"));
    swisha::http::bind_socket(&path).expect("a stale socket is replaced");
    assert_eq!(mode(&path), 0o660);
    // Bound in a folder of its own and moved into place: the folder goes with it.
    let left: Vec<_> = std::fs::read_dir(&dir).expect("read").map(|e| e.expect("entry").file_name()).collect();
    assert_eq!(left, ["s"], "nothing but the socket is left beside it");

    let error = swisha::http::bind_socket(&dir.join("missing").join("s")).expect_err("no folder").to_string();
    assert!(error.contains("does not exist"), "{error}");
    let _ = std::fs::remove_dir_all(dir);
}
