#![cfg(feature = "http")]

// A restart is how every deploy ends. Stopping must not cut off a payout already on its way to
// Swish: that payout gets its answer and its row written, and only then does the server stop.

mod common;

use std::time::Duration;

use common::{MockSwish, PAYER, PHONE, SECRET, SSN};
use serde_json::json;
use swisha::http::InternalSocket;
use swisha::store::PayoutStore;
use tokio::net::TcpListener;

#[tokio::test]
async fn a_stop_lets_the_payout_in_flight_finish_then_refuses_new_connections() {
    // Swish takes a moment to accept, long enough for the stop to arrive mid-request.
    let swish = MockSwish::new()
        .accepts()
        .resolves_to("PAID")
        .answers_after(Duration::from_millis(1500));
    let mut config = common::config();
    config.swish_base_url = swish.clone().start().await;
    let state = common::state_with(config).await;

    let internal = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let base = format!("http://{}", internal.local_addr().expect("addr"));
    let callback = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(swisha::http::serve(
        InternalSocket::Tcp(internal),
        callback,
        state.clone(),
        async {
            let _ = stopped.await;
        },
        Duration::from_secs(10),
        swisha::http::Limits::default(),
    ));

    let url = base.clone();
    let in_flight = tokio::spawn(async move {
        reqwest::Client::new()
            .post(format!("{url}/swish/payout"))
            .header("x-api-secret", SECRET)
            .header(common::KEY_HEADER, common::key(common::PAYER))
            .json(&json!({ "reference": "STOP-1", "payee_alias": PHONE, "payee_ssn": SSN, "amount": 100.0 }))
            .send()
            .await
            .expect("the request in flight is answered, not cut off")
            .status()
            .as_u16()
    });

    tokio::time::sleep(Duration::from_millis(500)).await;
    let _ = stop.send(());

    let stopped = tokio::time::timeout(Duration::from_secs(5), server).await;
    assert!(
        matches!(stopped, Ok(Ok(Ok(())))),
        "the server stops cleanly once the request is done: {stopped:?}"
    );
    // The binary exits the moment serve returns, so returning early would kill the payout in
    // flight. The mock records the instruction only once it has answered it, after its delay.
    assert_eq!(swish.posted().len(), 1, "serve returned only after the payout reached Swish");

    assert_eq!(in_flight.await.expect("join"), 202, "the payout in flight gets its answer");
    let snapshot = state.store.snapshot(PAYER, "STOP-1").await.expect("store").expect("row");
    assert!(snapshot.swish_ref.is_some(), "and its row was written");

    let after = reqwest::Client::new().get(format!("{base}/system/health")).send().await;
    assert!(after.is_err(), "nothing new is accepted once stopped");
}

// A stop waits for what a request started even when its caller has already gone: the payout it
// was sending has Swish's acceptance on its trail before serve returns and the binary exits.
#[tokio::test]
async fn a_stop_finishes_the_work_of_a_caller_that_left() {
    let swish = MockSwish::new().accepts().answers_after(Duration::from_millis(1500));
    let mut config = common::config();
    config.swish_base_url = swish.clone().start().await;
    let state = common::state_with(config).await;

    let internal = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let base = format!("http://{}", internal.local_addr().expect("addr"));
    let callback = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(swisha::http::serve(
        InternalSocket::Tcp(internal),
        callback,
        state.clone(),
        async {
            let _ = stopped.await;
        },
        Duration::from_secs(10),
        swisha::http::Limits::default(),
    ));

    let left = reqwest::Client::builder()
        .timeout(Duration::from_millis(300))
        .build()
        .expect("client")
        .post(format!("{base}/swish/payout"))
        .header("x-api-secret", SECRET)
        .header(common::KEY_HEADER, common::key(common::PAYER))
        .json(&json!({ "reference": "STOP-2", "payee_alias": PHONE, "payee_ssn": SSN, "amount": 100.0 }))
        .send()
        .await;
    assert!(left.is_err(), "the caller left before Swish answered");
    let _ = stop.send(());

    let stopped = tokio::time::timeout(Duration::from_secs(5), server).await;
    assert!(matches!(stopped, Ok(Ok(Ok(())))), "the server stops cleanly: {stopped:?}");
    let trail: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT event FROM {} WHERE reference = 'STOP-2' ORDER BY id",
        state.config.table_events
    )))
    .fetch_all(state.store.pool())
    .await
    .expect("events");
    assert!(trail.iter().any(|event| event == "ACCEPTED"), "serve returned before the send finished: {trail:?}");
}

// Every deploy restarts swisha, and the client's screen is always subscribed to /events. A
// stream that never ends would make each stop wait the whole drain, with nothing accepting.
#[tokio::test]
async fn a_stop_ends_event_streams_instead_of_waiting_for_them() {
    let state = common::state().await;
    let internal = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let base = format!("http://{}", internal.local_addr().expect("addr"));
    let callback = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(swisha::http::serve(
        InternalSocket::Tcp(internal),
        callback,
        state,
        async {
            let _ = stopped.await;
        },
        Duration::from_secs(10),
        swisha::http::Limits::default(),
    ));

    let mut stream = reqwest::Client::new()
        .get(format!("{base}/events"))
        .header("x-api-secret", SECRET)
        .header(common::KEY_HEADER, common::key(common::PAYER))
        .send()
        .await
        .expect("subscribe");
    assert_eq!(stream.status().as_u16(), 200);
    tokio::time::sleep(Duration::from_millis(200)).await;

    let started = std::time::Instant::now();
    let _ = stop.send(());
    let outcome = tokio::time::timeout(Duration::from_secs(3), server).await;
    assert!(matches!(outcome, Ok(Ok(Ok(())))), "the stop waited for a stream that never ends: {outcome:?}");
    assert!(started.elapsed() < Duration::from_secs(2), "{:?}", started.elapsed());

    let ended = tokio::time::timeout(Duration::from_secs(2), async {
        while let Ok(Some(_)) = stream.chunk().await {}
    })
    .await;
    assert!(ended.is_ok(), "the client sees its stream end");
}
