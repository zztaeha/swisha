#![cfg(feature = "http")]

// swisha forbids unsafe, so a use-after-free or an overflow is not reachable. What is reachable
// is growth: a map that never prunes, a channel that never drops, a request that allocates in
// proportion to what a caller sent. These tests are about staying bounded.

use sqlx::AssertSqlSafe;

mod common;

use common::{MockSwish, PAYER, PHONE, SECRET, SSN};
use serde_json::{json, Value};
use swisha::events::{EventStream, StreamEvent};
use swisha::state::SharedState;
use swisha::store::PayoutStore;

struct Rig {
    base: String,
    state: SharedState,
    pool: sqlx::PgPool,
    swish: MockSwish,
}

async fn rig_with(swish: MockSwish) -> Rig {
    let url = swish.clone().start().await;
    let mut config = common::config();
    config.swish_base_url = url;
    let (state, pool) = common::state_and_pool(config).await;
    let base = common::serve(swisha::http::internal_router().with_state(state.clone())).await;
    Rig { base, state, pool, swish }
}

async fn rig() -> Rig {
    rig_with(MockSwish::new().accepts().resolves_to("PAID")).await
}

fn body(reference: &str) -> Value {
    json!({ "reference": reference, "payee_alias": PHONE, "payee_ssn": SSN, "amount": 100.0 })
}

async fn pay(base: &str, payload: Value) -> u16 {
    reqwest::Client::new()
        .post(format!("{base}/swish/payout"))
        .header("x-api-secret", SECRET)
        .header(common::KEY_HEADER, common::key(common::PAYER))
        .json(&payload)
        .send()
        .await
        .expect("request")
        .status()
        .as_u16()
}

// The event channel

// A bounded ring buffer is what stops a slow reader turning into unbounded memory. The oldest
// events are dropped instead, which is why recovery is the status endpoint and not the stream.
#[test]
fn a_slow_subscriber_loses_events_rather_than_growing_memory() {
    let stream = EventStream::new(8);
    let mut rx = stream.subscribe("sthlm").expect("room for a stream");

    for i in 0..100 {
        stream.publish_for_test(StreamEvent {
            payer: "sthlm".into(),
            channel: "swisha:sthlm:events".into(),
            reference: "INV-1".into(),
            payload: format!("event {i}"),
        });
    }

    match rx.try_recv() {
        Err(tokio::sync::broadcast::error::TryRecvError::Lagged(missed)) => {
            assert!(missed >= 90, "a small buffer should have dropped most of them, missed {missed}");
        }
        other => panic!("a lagging subscriber should be told it lagged, got {other:?}"),
    }
}

#[test]
fn a_dropped_subscriber_is_released() {
    let stream = EventStream::default();
    assert_eq!(stream.subscriber_count(), 0);
    {
        let _a = stream.subscribe("sthlm").expect("room for a stream");
        let _b = stream.subscribe("sthlm").expect("room for a stream");
        assert_eq!(stream.subscriber_count(), 2);
    }
    assert_eq!(stream.subscriber_count(), 0, "receivers must not outlive their scope");
}

#[test]
fn publishing_with_nobody_listening_is_not_an_error() {
    let stream = EventStream::new(4);
    for i in 0..10_000 {
        stream.publish_for_test(StreamEvent {
            payer: "sthlm".into(),
            channel: "swisha:sthlm:events".into(),
            reference: "INV-1".into(),
            payload: format!("{i}"),
        });
    }
    assert_eq!(stream.subscriber_count(), 0);
}

// Allocation is proportional to input

#[test]
fn hex_allocates_exactly_two_characters_per_byte() {
    for size in [0usize, 1, 16, 1_024] {
        let rendered = swisha::util::hex::upper(&vec![0xabu8; size]);
        assert_eq!(rendered.len(), size * 2);
        assert_eq!(rendered.capacity(), size * 2, "no slack was reserved for {size} bytes");
    }
}

#[test]
fn base64_output_is_bounded_by_its_input() {
    for size in [0usize, 1, 2, 3, 1_000] {
        let encoded = swisha::util::base64::encode(&vec![0u8; size]);
        assert_eq!(encoded.len(), size.div_ceil(3) * 4, "base64 is 4 characters per 3 bytes");
        assert_eq!(swisha::util::base64::decode(&encoded).expect("round trip").len(), size);
    }
}

#[test]
fn normalizing_never_grows_its_input() {
    use swisha::domain::validate::{normalize_phone, normalize_ssn};
    for raw in ["", "0701234567", &"1".repeat(10_000), "+46 70 123 45 67"] {
        assert!(normalize_ssn(raw).len() <= raw.len(), "ssn normalization only removes");
        // A leading zero becomes the two-character country code, so this one may grow by one.
        assert!(normalize_phone(raw).len() <= raw.len() + 1);
    }
}

#[test]
fn a_status_set_renders_to_a_fixed_size() {
    use swisha::domain::status::{sql_list, TERMINAL, UNCONFIRMED};
    for set in [TERMINAL, UNCONFIRMED] {
        let rendered = sql_list(set);
        assert!(rendered.len() < 200, "a guard list is bounded by the enum, not by input");
        assert_eq!(rendered.matches('\'').count(), set.len() * 2);
    }
}

// Input cannot make swisha allocate without limit

#[tokio::test]
async fn an_oversized_body_is_refused_by_the_framework() {
    let r = rig().await;
    let (base, swish) = (r.base.clone(), r.swish.clone());
    let huge = format!(
        r#"{{"reference":"MEM-1","payee_alias":"{PHONE}","payee_ssn":"{SSN}","amount":100.0,"message":"{}"}}"#,
        "x".repeat(4 * 1024 * 1024)
    );
    // Two shapes count as refusal, and which one happens is a race: the server can answer with a
    // status, or hang up as soon as it decides not to read 4MB it will discard. Insisting on the
    // first made this test flaky. What matters is that the body is never accepted.
    let result = reqwest::Client::new()
        .post(format!("{base}/swish/payout"))
        .header("x-api-secret", SECRET)
        .header(common::KEY_HEADER, common::key(common::PAYER))
        .header("content-type", "application/json")
        .body(huge)
        .send()
        .await;

    match result {
        Ok(response) => {
            let code = response.status().as_u16();
            assert!(code == 413 || code == 400 || code == 422, "a 4MB body produced {code}");
        }
        Err(e) => assert!(
            !e.is_timeout(),
            "the connection closing is a refusal; hanging is not: {e}"
        ),
    }
    assert!(swish.posted().is_empty(), "nothing was sent to Swish");
}

#[tokio::test]
async fn an_enormous_reference_is_refused_before_anything_is_stored() {
    let r = rig().await;
    let (base, swish) = (r.base.clone(), r.swish.clone());
    for size in [1_000usize, 100_000] {
        let reference = "x".repeat(size);
        assert_eq!(pay(&base, body(&reference)).await, 400, "{size} characters");
        assert!(r.state.store.snapshot(PAYER, &reference).await.expect("store").is_none());
    }
    assert!(swish.posted().is_empty());
}

// serde_json bounds its own recursion, so a nesting bomb is refused rather than exhausting the
// stack. Without that bound this request would abort the process, not return an error.
#[tokio::test]
async fn a_deeply_nested_body_does_not_exhaust_the_stack() {
    let r = rig().await;
    let base = r.base.clone();
    let bomb = format!("{}{}", "[".repeat(50_000), "]".repeat(50_000));
    let code = reqwest::Client::new()
        .post(format!("{base}/swish/payout"))
        .header("x-api-secret", SECRET)
        .header(common::KEY_HEADER, common::key(common::PAYER))
        .header("content-type", "application/json")
        .body(bomb)
        .send()
        .await
        .expect("the process survived")
        .status()
        .as_u16();
    assert!(code == 400 || code == 422 || code == 413, "produced {code}");
}

// Nothing accumulates over repetition

#[tokio::test]
async fn refused_duplicates_do_not_accumulate_rows() {
    let r = rig().await;
    let (base, swish) = (r.base.clone(), r.swish.clone());
    assert_eq!(pay(&base, body("MEM-3")).await, 202);
    for _ in 0..200 {
        assert_eq!(pay(&base, body("MEM-3")).await, 409);
    }
    let rows: i64 = sqlx::query_scalar(AssertSqlSafe(format!("SELECT count(*) FROM {}", r.state.config.table_payouts)))
        .fetch_one(&r.pool)
        .await
        .expect("count");
    assert_eq!(rows, 1, "200 refusals wrote 200 rows");
    assert_eq!(swish.posted().len(), 1);
}

#[tokio::test]
async fn many_payouts_grow_the_store_one_row_each() {
    let r = rig().await;
    let base = r.base.clone();
    for i in 0..50 {
        assert_eq!(pay(&base, body(&format!("MEM-4-{i}"))).await, 202);
    }
    let rows: i64 = sqlx::query_scalar(AssertSqlSafe(format!("SELECT count(*) FROM {}", r.state.config.table_payouts)))
        .fetch_one(&r.pool)
        .await
        .expect("count");
    assert_eq!(rows, 50, "one payout, one row");
}

#[tokio::test]
async fn repeated_callbacks_never_create_rows() {
    let r = rig().await;
    let callback = common::serve(swisha::http::callback_router().with_state(r.state.clone())).await;

    for i in 0..200 {
        reqwest::Client::new()
            .post(format!("{callback}/swish/callback"))
            .json(&json!({
                "payerAlias": common::PAYER_NUMBER,
                "payerPaymentReference": format!("GHOST-{i}"),
                "payoutInstructionUUID": "00000000000000000000000000000000",
                "status": "PAID",
            }))
            .send()
            .await
            .expect("request");
    }
    let rows: i64 = sqlx::query_scalar(AssertSqlSafe(format!("SELECT count(*) FROM {}", r.state.config.table_payouts)))
        .fetch_one(&r.pool)
        .await
        .expect("count");
    assert_eq!(rows, 0, "a callback must never bring a payout into existence");
}

// The poll runs in a spawned task after the request is answered. If those tasks never finished,
// a busy service would accumulate them for as long as it ran.
#[tokio::test]
async fn the_background_poll_tasks_finish() {
    let r = rig().await;
    let base = r.base.clone();
    for i in 0..20 {
        pay(&base, body(&format!("MEM-5-{i}"))).await;
    }
    tokio::time::sleep(std::time::Duration::from_millis(1_500)).await;

    let mut settled = 0;
    for i in 0..20 {
        if let Some(status) = r
            .state
            .store
            .snapshot(PAYER, &format!("MEM-5-{i}"))
            .await
            .expect("store")
            .and_then(|s| s.status)
            && status != "CREATED"
        {
            settled += 1;
        }
    }
    assert!(settled >= 18, "spawned polls should have run to completion, {settled}/20 did");
}

#[tokio::test]
async fn a_rejected_payout_leaves_one_row_not_many() {
    let r = rig_with(MockSwish::new().rejects(422, r#"[{"errorCode":"PA02"}]"#)).await;
    let base = r.base.clone();

    pay(&base, body("MEM-6")).await;
    for _ in 0..50 {
        pay(&base, body("MEM-6")).await;
    }
    let rows: i64 = sqlx::query_scalar(AssertSqlSafe(format!("SELECT count(*) FROM {}", r.state.config.table_payouts)))
        .fetch_one(&r.pool)
        .await
        .expect("count");
    assert_eq!(rows, 1);
}

#[tokio::test]
async fn the_event_log_records_one_line_per_thing_that_happened() {
    let r = rig().await;
    let base = r.base.clone();
    pay(&base, body("MEM-7")).await;
    tokio::time::sleep(std::time::Duration::from_millis(600)).await;

    let events: i64 = sqlx::query_scalar(AssertSqlSafe(format!(
        "SELECT count(*) FROM {} WHERE reference = 'MEM-7'",
        r.state.config.table_events
    )))
        .fetch_one(&r.pool)
        .await
        .expect("count");
    assert!(events >= 1, "a payout should leave a trail");
    assert!(events < 20, "but not an unbounded one: {events}");
}

// The error table is static data. A lookup hands back borrowed strings, so explaining a failure
// costs nothing per call however often a caller asks.
#[test]
fn explaining_an_error_allocates_nothing_per_lookup() {
    use swisha::domain::errors::{describe, Language, TABLE};
    let first = describe(Some("RF07"));
    for _ in 0..100_000 {
        let again = describe(Some("RF07"));
        assert_eq!(again.english.as_ptr(), first.english.as_ptr(), "the same static string");
    }
    // And the table itself is fixed, so the whole surface is bounded by the enum.
    assert_eq!(TABLE.len(), 27);
    assert!(!first.message(Language::Swedish).is_empty());
}

// One key, shared. Reading the number per payout must not parse or copy its 2048-bit RSA key
// again: every request reads the one loaded number, and with it the one key.
#[tokio::test]
async fn the_signing_key_is_shared_rather_than_copied() {
    let r = rig().await;
    let payer = r.state.payer(PAYER).expect("the test payer is loaded");
    let again = r.state.payer(PAYER).expect("the test payer is loaded");
    assert!(std::sync::Arc::ptr_eq(&payer, &again), "each request reads the one loaded number");
    assert!(
        std::sync::Arc::ptr_eq(&payer.signing_key, &again.signing_key),
        "and signs with its one key"
    );

    // Eight tasks sign concurrently off the one key.
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let key = payer.signing_key.clone();
            tokio::spawn(async move { swisha::swish::sign::sign_payload("payload", &key).is_ok() })
        })
        .collect();
    for h in handles {
        assert!(h.await.expect("join"), "every task signed");
    }
}
