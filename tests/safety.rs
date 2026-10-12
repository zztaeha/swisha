#![cfg(feature = "http")]

// Safety under innocent failure rather than attack: crashes, races, partial answers and events
// arriving out of order. The double-payout guardrail is the center of it, because every other
// mistake here costs a phone call and that one costs money.

mod common;

use common::{MockSwish, PAYER, PHONE, SECRET, SSN};
use serde_json::{json, Value};
use swisha::domain::payout::EventRecord;
use swisha::state::SharedState;
use swisha::store::PayoutStore;

struct Rig {
    internal: String,
    callback: String,
    state: SharedState,
    swish: MockSwish,
}

async fn rig_with(swish: MockSwish) -> Rig {
    let url = swish.clone().start().await;
    let mut config = common::config();
    config.swish_base_url = url;
    let state = common::state_with(config).await;
    Rig {
        internal: common::serve(swisha::http::internal_router().with_state(state.clone())).await,
        callback: common::serve(swisha::http::callback_router().with_state(state.clone())).await,
        state,
        swish,
    }
}

async fn rig() -> Rig {
    rig_with(MockSwish::new().accepts().resolves_to("PAID")).await
}

fn body(reference: &str) -> Value {
    json!({ "reference": reference, "payee_alias": PHONE, "payee_ssn": SSN, "amount": 100.0 })
}

async fn pay(r: &Rig, payload: Value) -> (u16, String) {
    let response = reqwest::Client::new()
        .post(format!("{}/swish/payout", r.internal))
        .header("x-api-secret", SECRET)
        .header(common::KEY_HEADER, common::key(common::PAYER))
        .json(&payload)
        .send()
        .await
        .expect("request");
    let code = response.status().as_u16();
    (code, response.text().await.unwrap_or_default())
}

async fn callback(r: &Rig, reference: &str, uuid: &str, status: &str) -> u16 {
    let echoed = common::callback_identifier(&r.state, PAYER, reference).await;
    reqwest::Client::new()
        .post(format!("{}/swish/callback", r.callback))
        .header("callbackIdentifier", echoed)
        .json(&json!({
            "payerAlias": common::PAYER_NUMBER,
            "payerPaymentReference": reference,
            "payoutInstructionUUID": uuid,
            "status": status,
        }))
        .send()
        .await
        .expect("request")
        .status()
        .as_u16()
}

async fn status_of(r: &Rig, reference: &str) -> Option<String> {
    r.state.store.snapshot(PAYER, reference).await.expect("store").and_then(|s| s.status)
}

// The double-payout guardrail

// A caller that hangs up while Swish is still answering does not cut the send short. Swish's 201
// lands on the trail, so its saying later that it has no record reads as the contradiction it is.
#[tokio::test]
async fn safety_a_caller_that_hangs_up_mid_send_leaves_swishs_acceptance_on_the_trail() {
    let r = rig_with(MockSwish::new().accepts().answers_after(std::time::Duration::from_millis(1500))).await;
    let left = reqwest::Client::builder()
        .timeout(std::time::Duration::from_millis(300))
        .build()
        .expect("client")
        .post(format!("{}/swish/payout", r.internal))
        .header("x-api-secret", SECRET)
        .header(common::KEY_HEADER, common::key(common::PAYER))
        .json(&body("H-2"))
        .send()
        .await;
    assert!(left.is_err(), "the caller hung up before Swish answered");

    let since = std::time::Instant::now();
    loop {
        let trail: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT event FROM {} WHERE reference = 'H-2' ORDER BY id",
            r.state.config.table_events
        )))
        .fetch_all(r.state.store.pool())
        .await
        .expect("events");
        if trail.iter().any(|event| event == "ACCEPTED") {
            break;
        }
        assert!(since.elapsed() < std::time::Duration::from_secs(5), "the send stopped with its caller: {trail:?}");
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

#[tokio::test]
async fn safety_every_locked_status_refuses_a_repeat() {
    for status in ["PAID", "PENDING", "DEBITED", "NEEDS_REVIEW"] {
        let r = rig().await;
        pay(&r, body("G-2")).await;
        common::stage(&r.state, PAYER, "G-2", status).await;

        let (code, _) = pay(&r, body("G-2")).await;
        assert_eq!(code, 409, "a payout in {status} must not be resubmitted");
        assert_eq!(r.swish.posted().len(), 1, "{status} allowed a second instruction");
    }
}

// A refused duplicate must leave every stored field exactly as it was, or a second request could
// rewrite the amount or the recipient of a payout already in flight.
#[tokio::test]
async fn safety_a_refused_duplicate_rewrites_nothing() {
    let r = rig().await;
    let (_, first) = pay(&r, body("G-3")).await;
    let original_uuid = serde_json::from_str::<Value>(&first).expect("json")["swish_ref"]
        .as_str()
        .expect("uuid")
        .to_string();

    let mut hostile = body("G-3");
    hostile["amount"] = json!(49_000.0);
    hostile["payee_alias"] = json!("0709999999");
    hostile["payee_ssn"] = json!("194008230049");
    assert_eq!(pay(&r, hostile).await.0, 409);

    let snapshot = r.state.store.snapshot(PAYER, "G-3").await.expect("store").expect("row");
    assert_eq!(snapshot.swish_ref.as_deref(), Some(original_uuid.as_str()));
    let sent = common::payload(&r.swish.posted()[0]);
    assert_eq!(sent["amount"], "100.00");
    assert_eq!(sent["payeeAlias"], "46701234567");
}

#[tokio::test]
async fn safety_a_burst_from_one_caller_yields_one_instruction() {
    let r = rig().await;
    let mut tasks = Vec::new();
    for _ in 0..16 {
        let url = r.internal.clone();
        tasks.push(tokio::spawn(async move {
            reqwest::Client::new()
                .post(format!("{url}/swish/payout"))
                .header("x-api-secret", SECRET)
                .header(common::KEY_HEADER, common::key(common::PAYER))
                .json(&body("G-4"))
                .send()
                .await
                .expect("request")
                .status()
                .as_u16()
        }));
    }
    let mut accepted = 0;
    for t in tasks {
        if t.await.expect("join") == 202 {
            accepted += 1;
        }
    }
    assert_eq!(accepted, 1);
    assert_eq!(r.swish.posted().len(), 1);
}

// Two copies of swisha on one database, as behind a load balancer: two states sharing nothing but
// the database. Both claiming the same reference at once must still produce one payout.
#[tokio::test]
async fn safety_two_services_sharing_a_store_still_pay_once() {
    let swish = MockSwish::new().accepts().resolves_to("PAID");
    let url = swish.clone().start().await;
    let mut config = common::config();
    config.swish_base_url = url;
    let (first, pool) = common::state_and_pool(config.clone()).await;
    let payers = vec![common::payer_state(&common::payer(PAYER, common::PAYER_NUMBER))];
    let second = common::state_over(pool, config, payers).await;

    let a = common::serve(swisha::http::internal_router().with_state(first)).await;
    let b = common::serve(swisha::http::internal_router().with_state(second)).await;

    let mut tasks = Vec::new();
    for base in [a, b] {
        for _ in 0..6 {
            let base = base.clone();
            tasks.push(tokio::spawn(async move {
                reqwest::Client::new()
                    .post(format!("{base}/swish/payout"))
                    .header("x-api-secret", SECRET)
                    .header(common::KEY_HEADER, common::key(common::PAYER))
                    .json(&body("G-5"))
                    .send()
                    .await
                    .expect("request")
                    .status()
                    .as_u16()
            }));
        }
    }
    let mut accepted = 0;
    for t in tasks {
        if t.await.expect("join") == 202 {
            accepted += 1;
        }
    }
    assert_eq!(accepted, 1, "two services must not both win the same reference");
    assert_eq!(swish.posted().len(), 1);
}

// A failed payout still spends its reference. Whether Swish reported it failed or refused it, it is
// paid again under a new reference, never this one.
#[tokio::test]
async fn safety_a_failed_payout_does_not_silently_resubmit() {
    for status in ["ERROR", "DECLINED"] {
        let r = rig().await;
        pay(&r, body("G-6")).await;
        common::stage(&r.state, PAYER, "G-6", status).await;

        let (code, _) = pay(&r, body("G-6")).await;
        assert_eq!(code, 409, "a payout in {status} must be refused");
        assert_eq!(
            r.swish.posted().len(),
            1,
            "a payout in {status} produced a second instruction"
        );
    }
}

// Terminal safety

// The money has left: nothing moves PAID, and DEBITED moves only to Swish's PAID or ERROR.
#[tokio::test]
async fn safety_a_settled_payout_is_never_moved_back() {
    for (terminal, attempts) in [
        ("PAID", &["ERROR", "DEBITED", "DECLINED", "PENDING", "CREATED", "NEEDS_REVIEW"][..]),
        ("DEBITED", &["DECLINED", "PENDING", "CREATED", "NEEDS_REVIEW"][..]),
    ] {
        let r = rig().await;
        pay(&r, body("T-1")).await;
        common::stage(&r.state, PAYER, "T-1", terminal).await;

        for attempt in attempts {
            let answer = EventRecord {
                payer: PAYER.into(),
                reference: "T-1".into(),
                swish_ref: None,
                event: "CALLBACK".into(),
                status: Some((*attempt).into()),
                amount: None,
                payee_alias: None,
                error_code: None,
                error_message: None,
                ip: None,
                actor: None,
                note: None,
            };
            assert!(!r.state.store.record_status(attempt, &answer, true).await.expect("store"));
            assert_eq!(
                status_of(&r, "T-1").await.as_deref(),
                Some(terminal),
                "{attempt} moved a {terminal} payout"
            );
        }
    }
}

// NEEDS_REVIEW is deliberately not terminal: swisha has stopped chasing, but a genuine late
// answer from Swish must still be able to settle the row.
#[tokio::test]
async fn safety_a_reviewed_payout_can_still_be_settled_by_swish() {
    let r = rig().await;
    let (_, first) = pay(&r, body("T-2")).await;
    let uuid = serde_json::from_str::<Value>(&first).expect("json")["swish_ref"]
        .as_str()
        .expect("uuid")
        .to_string();
    common::stage(&r.state, PAYER, "T-2", "NEEDS_REVIEW").await;

    callback(&r, "T-2", &uuid, "PAID").await;
    assert_eq!(status_of(&r, "T-2").await.as_deref(), Some("PAID"));
}

// Callback safety

#[tokio::test]
async fn safety_a_repeated_callback_is_idempotent() {
    let r = rig().await;
    let (_, first) = pay(&r, body("K-1")).await;
    let uuid = serde_json::from_str::<Value>(&first).expect("json")["swish_ref"]
        .as_str()
        .expect("uuid")
        .to_string();

    for _ in 0..5 {
        assert_eq!(callback(&r, "K-1", &uuid, "DEBITED").await, 200);
    }
    assert_eq!(status_of(&r, "K-1").await.as_deref(), Some("DEBITED"));
    assert_eq!(r.swish.posted().len(), 1, "a callback never causes an instruction");
}

// With the matching UUID this is Swish talking, so it is answered 200. A status outside the
// lifecycle must still never reach the column, and the reference stays spent whatever it holds.
#[tokio::test]
async fn safety_a_callback_status_swisha_does_not_know_cannot_reopen_a_reference() {
    // ERROR is not terminal, so only the callback's own check stands between it and the column.
    let r = rig_with(MockSwish::new().accepts().resolves_to("ERROR")).await;
    let (_, first) = pay(&r, body("K-4")).await;
    let uuid = serde_json::from_str::<Value>(&first).expect("json")["swish_ref"]
        .as_str()
        .expect("uuid")
        .to_string();
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    assert_eq!(status_of(&r, "K-4").await.as_deref(), Some("ERROR"));

    let echoed = common::callback_identifier(&r.state, PAYER, "K-4").await;
    for strange in [json!("REFUNDED"), json!("paid"), json!("NEEDS_REVIEW"), json!(null)] {
        let code = reqwest::Client::new()
            .post(format!("{}/swish/callback", r.callback))
            .header("callbackIdentifier", &echoed)
            .json(&json!({
                "payerAlias": common::PAYER_NUMBER,
                "payerPaymentReference": "K-4",
                "payoutInstructionUUID": uuid,
                "status": strange,
            }))
            .send()
            .await
            .expect("request")
            .status()
            .as_u16();
        assert_eq!(code, 200, "{strange} is acknowledged so Swish stops redelivering");
    }

    let mut repeat = body("K-4");
    repeat["amount"] = json!(49_000.0);
    assert_eq!(pay(&r, repeat).await.0, 409, "the reference is still spent");
    assert_eq!(r.swish.posted().len(), 1, "a second instruction would be a second debit");
    assert_eq!(status_of(&r, "K-4").await.as_deref(), Some("ERROR"), "nothing strange was written");
}

#[tokio::test]
async fn safety_a_callback_answers_ok_so_swish_stops_retrying() {
    let r = rig().await;
    // Unknown reference, mismatched UUID and an unexpected status all answer 200: none is for
    // Swish to fix, and anything else would ask it to send the callback again.
    assert_eq!(callback(&r, "NOPE", "00000000000000000000000000000000", "PAID").await, 200);
    pay(&r, body("K-3")).await;
    assert_eq!(callback(&r, "K-3", "00000000000000000000000000000000", "PAID").await, 200);
    assert_eq!(callback(&r, "K-3", "00000000000000000000000000000000", "WAT").await, 200);
}

// Reconcile and sweep

#[tokio::test]
async fn safety_reconciling_twice_changes_nothing_the_second_time() {
    use swisha::swish::reconcile::reconcile;
    let r = rig_with(MockSwish::new().accepts().resolves_to("DEBITED")).await;
    let (_, first) = pay(&r, body("R-1")).await;
    let uuid = serde_json::from_str::<Value>(&first).expect("json")["swish_ref"]
        .as_str()
        .expect("uuid")
        .to_string();
    common::stage(&r.state, PAYER, "R-1", "PENDING").await;

    reconcile(PAYER.into(), "R-1".into(), false, r.state.clone()).await;
    let after_first = status_of(&r, "R-1").await;
    reconcile(PAYER.into(), "R-1".into(), false, r.state.clone()).await;

    assert_eq!(status_of(&r, "R-1").await, after_first);
    assert_eq!(
        r.state.store.snapshot(PAYER, "R-1").await.expect("store").expect("row").swish_ref.as_deref(),
        Some(uuid.as_str()),
        "reconciling must never mint a new instruction UUID"
    );
    assert_eq!(r.swish.posted().len(), 1);
}

// Money correctness

#[tokio::test]
async fn safety_the_instruction_carries_the_merchant_number_not_the_payee() {
    let r = rig().await;
    pay(&r, body("M-ALIAS")).await;
    let sent = common::payload(&r.swish.posted()[0]);
    assert_eq!(sent["payerAlias"], "1234679304", "the payer is the merchant");
    assert_eq!(sent["payeeAlias"], "46701234567", "the payee is the recipient");
    assert_eq!(sent["currency"], "SEK");
    assert_eq!(sent["payoutType"], "PAYOUT");
}

// Audit trail

#[tokio::test]
async fn safety_every_accepted_payout_leaves_a_trail() {
    let r = rig().await;
    assert_eq!(pay(&r, body("A-1")).await.0, 202);

    let snapshot = r.state.store.snapshot(PAYER, "A-1").await.expect("store").expect("row");
    type Initiated = (Option<f64>, Option<String>, Option<String>, Option<String>);
    let initiated: Option<Initiated> =
        sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT amount::FLOAT8, payee_alias, ip, swish_ref FROM {} WHERE reference = 'A-1' AND event = 'INITIATED'",
            r.state.config.table_events
        )))
        .fetch_optional(r.state.store.pool())
        .await
        .expect("events");
    let (amount, payee, ip, swish_ref) = initiated.expect("the trail opens with INITIATED by the time of the 202");
    assert_eq!(amount, Some(100.0));
    assert_eq!(payee.as_deref(), Some("46701234567"), "the recipient, as sent");
    assert_eq!(ip.as_deref(), Some("127.0.0.1"), "who asked: the address it connected from");
    assert_eq!(swish_ref, snapshot.swish_ref, "the instruction it opened");
}

#[tokio::test]
async fn safety_the_status_endpoint_tells_the_truth_at_every_stage() {
    let r = rig().await;
    let (_, first) = pay(&r, body("A-3")).await;
    let uuid = serde_json::from_str::<Value>(&first).expect("json")["swish_ref"]
        .as_str()
        .expect("uuid")
        .to_string();

    for stage in ["PENDING", "DEBITED"] {
        common::stage(&r.state, PAYER, "A-3", stage).await;
        let seen: Value = reqwest::Client::new()
            .get(format!("{}/swish/status/A-3", r.internal))
            .header("x-api-secret", SECRET)
            .header(common::KEY_HEADER, common::key(common::PAYER))
            .send()
            .await
            .expect("request")
            .json()
            .await
            .expect("json");
        assert_eq!(seen["status"], stage);
        assert_eq!(seen["swish_ref"], uuid);
    }
}

// Storage contract

#[tokio::test]
async fn safety_a_fresh_reference_is_unaffected_by_its_neighbors() {
    let r = rig().await;
    for i in 0..5 {
        assert_eq!(pay(&r, body(&format!("N-{i}"))).await.0, 202);
    }
    assert_eq!(r.swish.posted().len(), 5, "five references, five instructions");

    let mut uuids: Vec<String> = Vec::new();
    for i in 0..5 {
        let snapshot = r.state.store.snapshot(PAYER, &format!("N-{i}")).await.expect("store").expect("row");
        uuids.push(snapshot.swish_ref.expect("uuid"));
    }
    uuids.sort();
    uuids.dedup();
    assert_eq!(uuids.len(), 5, "every payout carries its own instruction UUID");
}

#[tokio::test]
async fn safety_instruction_uuids_do_not_repeat() {
    let r = rig().await;
    let mut seen = std::collections::HashSet::new();
    for i in 0..40 {
        let (_, response) = pay(&r, body(&format!("U-{i}"))).await;
        let uuid = serde_json::from_str::<Value>(&response).expect("json")["swish_ref"]
            .as_str()
            .expect("uuid")
            .to_string();
        assert_eq!(uuid.len(), 32);
        assert!(seen.insert(uuid.clone()), "{uuid} was issued twice");
    }
}

#[tokio::test]
async fn safety_state_survives_a_restart_because_it_is_not_in_memory() {
    let r = rig().await;
    pay(&r, body("P-1")).await;
    let before = r.state.store.snapshot(PAYER, "P-1").await.expect("store").expect("row");

    // A restarted process: new state, nothing carried over in memory, the same database.
    let restarted = common::state_over(
        r.state.store.pool().clone(),
        r.state.config.clone(),
        vec![common::payer_state(&common::payer(PAYER, common::PAYER_NUMBER))],
    )
    .await;
    let fresh = Rig {
        internal: common::serve(swisha::http::internal_router().with_state(restarted.clone())).await,
        callback: common::serve(swisha::http::callback_router().with_state(restarted.clone())).await,
        state: restarted,
        swish: r.swish.clone(),
    };
    let seen: Value = reqwest::Client::new()
        .get(format!("{}/swish/status/P-1", fresh.internal))
        .header("x-api-secret", SECRET)
        .header(common::KEY_HEADER, common::key(common::PAYER))
        .send()
        .await
        .expect("request")
        .json()
        .await
        .expect("json");

    assert_eq!(seen["swish_ref"], before.swish_ref.expect("uuid"));
    assert_eq!(pay(&fresh, body("P-1")).await.0, 409, "the guard survives a restart too");
    assert_eq!(r.swish.posted().len(), 1, "Swish heard of P-1 once");
}

// The poll gives up after eight unanswered questions. An ERROR callback that landed meanwhile has
// to stand, and be the only status announced.
#[tokio::test]
async fn safety_a_poll_that_gives_up_never_replaces_an_answer() {
    let r = rig_with(MockSwish::new().accepts().resolves_to("CREATED")).await;
    let mut announced = r.state.stream.subscribe(PAYER).expect("room for a stream");
    assert_eq!(pay(&r, body("LATE-1")).await.0, 202);
    let uuid = r.state.store.snapshot(PAYER, "LATE-1").await.expect("store").and_then(|s| s.swish_ref).expect("uuid");

    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    assert_eq!(callback(&r, "LATE-1", &uuid, "ERROR").await, 200);
    // Eight questions: half a second, then seven more two seconds apart.
    tokio::time::sleep(std::time::Duration::from_secs(16)).await;
    assert_eq!(status_of(&r, "LATE-1").await.as_deref(), Some("ERROR"));

    let mut statuses = Vec::new();
    while let Ok(event) = announced.try_recv() {
        if event.channel.ends_with(":updates") && event.reference == "LATE-1" {
            let payload: Value = serde_json::from_str(&event.payload).expect("json");
            statuses.push(payload["status"].as_str().unwrap_or_default().to_string());
        }
    }
    assert_eq!(statuses, ["ERROR"], "only the answer is announced");
}

// Swish reports DEBITED, then PAID, as separate answers. Whether the poll, the callback or the
// sweep hears DEBITED, the audit trail must not claim a payment Swish has not confirmed.
#[tokio::test]
async fn safety_the_trail_says_paid_only_when_swish_does() {
    let events = |r: &Rig, reference: &'static str| {
        let (pool, table) = (r.state.store.pool().clone(), r.state.config.table_events.clone());
        async move {
            sqlx::query_scalar::<_, String>(sqlx::AssertSqlSafe(format!(
                "SELECT event FROM {table} WHERE reference = $1 ORDER BY id"
            )))
            .bind(reference)
            .fetch_all(&pool)
            .await
            .expect("events")
        }
    };

    // The poll.
    let r = rig_with(MockSwish::new().accepts().resolves_to("DEBITED")).await;
    pay(&r, body("DEB-1")).await;
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    assert_eq!(status_of(&r, "DEB-1").await.as_deref(), Some("DEBITED"));
    assert!(!events(&r, "DEB-1").await.contains(&"PAID".to_string()), "{:?}", events(&r, "DEB-1").await);

    // The sweep, asking about a payout the poll never settled.
    swisha::store::PayoutStore::claim(&r.state.store, &swisha::domain::payout::NewPayout {
        payer: PAYER,
        reference: "DEB-3",
        payer_alias: common::PAYER_NUMBER,
        payee_alias: PHONE,
        payee_ssn: Some(SSN),
        amount: 100.0,
        message: "DEB-3",
        swish_ref: "AAAA0000000000000000000000000003",
        callback_identifier: None,
    })
    .await
    .expect("claim");
    swisha::swish::reconcile::reconcile(PAYER.into(), "DEB-3".into(), false, r.state.clone()).await;
    assert_eq!(status_of(&r, "DEB-3").await.as_deref(), Some("DEBITED"));
    assert!(!events(&r, "DEB-3").await.contains(&"PAID".to_string()), "{:?}", events(&r, "DEB-3").await);

    // The callback, with a Swish that has no record yet, so the poll writes nothing.
    let r = rig_with(MockSwish::new().accepts()).await;
    pay(&r, body("DEB-2")).await;
    let uuid = r.state.store.snapshot(PAYER, "DEB-2").await.expect("store").and_then(|s| s.swish_ref).expect("uuid");
    assert_eq!(callback(&r, "DEB-2", &uuid, "DEBITED").await, 200);
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    assert_eq!(status_of(&r, "DEB-2").await.as_deref(), Some("DEBITED"));
    assert!(!events(&r, "DEB-2").await.contains(&"PAID".to_string()), "{:?}", events(&r, "DEB-2").await);
}

// A stored status this version does not know is still a spent reference. The store never rewrites
// the row; the route must not treat the leftover as its own and submit again.
#[tokio::test]
async fn safety_a_status_swisha_does_not_know_is_still_a_spent_reference() {
    let r = rig().await;
    assert_eq!(pay(&r, body("ODD-1")).await.0, 202);
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE {} SET status = 'MANUAL' WHERE reference = 'ODD-1'",
        r.state.config.table_payouts
    )))
    .execute(r.state.store.pool())
    .await
    .expect("hand edit");

    let (code, text) = pay(&r, body("ODD-1")).await;
    assert_eq!(code, 409, "{text}");
    assert!(text.contains("MANUAL"), "the answer names the status it found: {text}");
    assert_eq!(r.swish.posted().len(), 1, "no second instruction");
    assert_eq!(status_of(&r, "ODD-1").await.as_deref(), Some("MANUAL"), "and the row is untouched");
}

// Swish refused with a 500 and TA01, then paid the instruction anyway. The payout is PAID, and
// nothing about it may still read "check whether it went through before sending it again".
#[tokio::test]
async fn safety_a_paid_payout_carries_no_old_failure_text() {
    let r = rig_with(MockSwish::new().rejects(500, r#"[{"errorCode":"TA01","errorMessage":"Temporary technical error"}]"#)).await;
    let (code, _) = pay(&r, body("TA-1")).await;
    assert_eq!(code, 503);
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert_eq!(status_of(&r, "TA-1").await.as_deref(), Some("PENDING"), "Swish may hold it");

    let uuid = r.state.store.snapshot(PAYER, "TA-1").await.expect("store").and_then(|s| s.swish_ref).expect("uuid");
    assert_eq!(callback(&r, "TA-1", &uuid, "PAID").await, 200);
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let seen: Value = reqwest::Client::new()
        .get(format!("{}/swish/status/TA-1", r.internal))
        .header("x-api-secret", SECRET)
        .header(common::KEY_HEADER, common::key(common::PAYER))
        .send()
        .await
        .expect("request")
        .json()
        .await
        .expect("json");
    assert_eq!(seen["status"], "PAID");
    assert!(seen["error_code"].is_null() && seen["error_message"].is_null() && seen["error_category"].is_null(), "{seen}");
}

// Swish retries a callback it could not deliver, so DEBITED can arrive again after a late ERROR.
// It must not make a payout whose money came back look paid.
#[tokio::test]
async fn safety_a_late_error_callback_stands_against_a_repeated_debited() {
    let r = rig_with(MockSwish::new().accepts()).await;
    assert_eq!(pay(&r, body("LATE-2")).await.0, 202);
    let uuid = r.state.store.snapshot(PAYER, "LATE-2").await.expect("store").and_then(|s| s.swish_ref).expect("uuid");

    assert_eq!(callback(&r, "LATE-2", &uuid, "DEBITED").await, 200);
    assert_eq!(callback(&r, "LATE-2", &uuid, "ERROR").await, 200);
    assert_eq!(status_of(&r, "LATE-2").await.as_deref(), Some("ERROR"), "the late failure is recorded");
    assert_eq!(callback(&r, "LATE-2", &uuid, "DEBITED").await, 200);
    assert_eq!(status_of(&r, "LATE-2").await.as_deref(), Some("ERROR"), "and a repeated DEBITED never undoes it");
    assert_eq!(r.swish.posted().len(), 1);
}

// DEBITED is settled: the money has left. A repeat is told so, not "in progress".
#[tokio::test]
async fn safety_a_repeat_of_a_debited_payout_is_told_it_was_debited() {
    let r = rig_with(MockSwish::new().accepts().resolves_to("DEBITED")).await;
    assert_eq!(pay(&r, body("DEB-9")).await.0, 202);
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    let (code, text) = pay(&r, body("DEB-9")).await;
    assert_eq!(code, 409);
    assert!(text.contains("debited") && !text.contains("in progress"), "{text}");
}

// A callback the store refuses announces what the status endpoint would answer. A late PENDING
// after Swish's ERROR with RF07 must not replace the bank advice with a generic text.
#[tokio::test]
async fn safety_a_refused_callback_announces_the_error_it_left_in_place() {
    let r = rig_with(MockSwish::new().accepts()).await;
    assert_eq!(pay(&r, body("ANN-1")).await.0, 202);
    let uuid = r.state.store.snapshot(PAYER, "ANN-1").await.expect("store").and_then(|s| s.swish_ref).expect("uuid");
    let echoed = common::callback_identifier(&r.state, PAYER, "ANN-1").await;
    let send = |status: &'static str, code: Option<&'static str>| {
        let (url, uuid, echoed) = (format!("{}/swish/callback", r.callback), uuid.clone(), echoed.clone());
        async move {
            let mut body = json!({ "payerAlias": common::PAYER_NUMBER, "payerPaymentReference": "ANN-1", "payoutInstructionUUID": uuid, "status": status });
            if let Some(code) = code {
                body["errorCode"] = json!(code);
            }
            reqwest::Client::new().post(url).header("callbackIdentifier", echoed).json(&body).send().await.expect("request").status().as_u16()
        }
    };
    let mut announced = r.state.stream.subscribe(PAYER).expect("room for a stream");
    assert_eq!(send("ERROR", Some("RF07")).await, 200);
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert_eq!(send("PENDING", None).await, 200);
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let mut seen = Vec::new();
    while let Ok(event) = announced.try_recv() {
        if event.channel.ends_with(":updates") {
            seen.push(serde_json::from_str::<Value>(&event.payload).expect("json"));
        }
    }
    assert_eq!(seen.len(), 2, "{seen:?}");
    assert_eq!(seen[0]["error_code"], "RF07", "the callback's own code is announced with it");
    assert_eq!(seen[1]["status"], "ERROR");
    assert_eq!(seen[1]["error_code"], "RF07", "{:?}", seen[1]);
}

// A callback says DEBITED while the POST waits, then the POST comes back 500. swisha's "no answer
// yet" must not replace Swish's DEBITED nor be announced; the caller gets the payout as it stands.
#[tokio::test]
async fn safety_a_late_failed_send_never_overwrites_the_callback_that_answered_first() {
    let r = rig_with(
        MockSwish::new()
            .rejects(500, "upstream exploded")
            .answers_after(std::time::Duration::from_millis(800)),
    )
    .await;
    let mut announced = r.state.stream.subscribe(PAYER).expect("room for a stream");
    let base = r.internal.clone();
    let send = tokio::spawn(async move {
        let response = reqwest::Client::new()
            .post(format!("{base}/swish/payout"))
            .header("x-api-secret", SECRET)
            .header(common::KEY_HEADER, common::key(common::PAYER))
            .json(&body("RACE-1"))
            .send()
            .await
            .expect("request");
        (response.status().as_u16(), response.json::<Value>().await.expect("json"))
    });

    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let uuid = r.state.store.snapshot(PAYER, "RACE-1").await.expect("store").and_then(|s| s.swish_ref).expect("uuid");
    assert_eq!(callback(&r, "RACE-1", &uuid, "DEBITED").await, 200);

    let (code, answer) = send.await.expect("join");
    assert_eq!(code, 202, "Swish holds it, as its callback proves: {answer}");
    assert_eq!(answer["status"], "DEBITED", "the answer is the payout as it stands: {answer}");
    assert!(answer["error"].is_null(), "no unknown outcome beside Swish's own answer: {answer}");
    assert_eq!(status_of(&r, "RACE-1").await.as_deref(), Some("DEBITED"));
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let mut statuses = Vec::new();
    while let Ok(event) = announced.try_recv() {
        if event.channel.ends_with(":updates") {
            let payload: Value = serde_json::from_str(&event.payload).expect("json");
            statuses.push(payload["status"].as_str().unwrap_or_default().to_string());
        }
    }
    assert_eq!(statuses, ["DEBITED"], "only Swish's answer is announced, once");
}

// Redo, and a payout paid after it

async fn redo(r: &Rig, reference: &str, new: &str) -> (u16, Value) {
    let response = reqwest::Client::new()
        .post(format!("{}/swish/payout/{PAYER}/{reference}/redo", r.internal))
        .header("x-api-secret", SECRET)
        .header(common::KEY_HEADER, common::key(common::PAYER))
        .json(&json!({ "reference": new, "by": "Anna" }))
        .send()
        .await
        .expect("request");
    let code = response.status().as_u16();
    (code, response.json().await.unwrap_or_default())
}

// A payout Swish holds under `uuid`, at `status` as its callbacks left it.
async fn held(r: &Rig, reference: &str, uuid: &str, status: &str) {
    let new = swisha::domain::payout::NewPayout {
        payer: PAYER,
        reference,
        payer_alias: common::PAYER_NUMBER,
        payee_alias: "46701234567",
        payee_ssn: Some(SSN),
        amount: 100.0,
        message: "safety",
        swish_ref: uuid,
        callback_identifier: Some("CB0000000000000000000000000000CB"),
    };
    r.state.store.claim(&new).await.expect("claim");
    if status != "CREATED" {
        assert!(common::stage(&r.state, PAYER, reference, status).await);
    }
}

async fn error_lines(r: &Rig) -> Vec<String> {
    sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT message FROM {} WHERE level = 'error' ORDER BY id",
        r.state.config.table_logs
    )))
    .fetch_all(r.state.store.pool())
    .await
    .expect("logs")
}

// The error lines once a callback's announcement has run: it runs after the callback is answered,
// so it is waited for, then given a moment more for any second line.
async fn announced_error_lines(r: &Rig) -> Vec<String> {
    let since = std::time::Instant::now();
    while error_lines(r).await.is_empty() && since.elapsed() < std::time::Duration::from_secs(3) {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    error_lines(r).await
}

async fn view(r: &Rig, reference: &str) -> Value {
    swisha::events::current_payload(PAYER, reference, &r.state).await.expect("store").expect("row")
}

// Whoever holds a payout's callbackIdentifier can fake Swish's ERROR on it. The redo asks Swish
// first, so a payout Swish paid is never paid again however its row reads.
#[tokio::test]
async fn safety_a_faked_failure_never_leads_to_a_second_payment() {
    let r = rig_with(MockSwish::new().accepts()).await;
    let uuid = "FA0000000000000000000000000000FA";
    held(&r, "FAKE-1", uuid, "DEBITED").await;
    r.swish.clone().resolves_for(uuid, "PAID");
    assert_eq!(callback(&r, "FAKE-1", uuid, "ERROR").await, 200);
    assert_eq!(status_of(&r, "FAKE-1").await.as_deref(), Some("ERROR"), "the fake landed");

    let (code, answer) = redo(&r, "FAKE-1", "FAKE-1-B").await;
    assert_eq!(code, 409, "{answer}");
    assert!(r.swish.posted().is_empty(), "nothing was sent to Swish");
    assert_eq!(status_of(&r, "FAKE-1").await.as_deref(), Some("PAID"), "Swish's word replaced the fake");
}

// Swish reports a redone payout paid after all. Nothing takes the second payment back, so it has
// to be impossible to miss: an error line in the service log, and a review on both payouts.
#[tokio::test]
async fn safety_a_payout_paid_after_its_redo_cannot_be_missed() {
    let r = rig_with(MockSwish::new().accepts()).await;
    let uuid = "PT0000000000000000000000000000PT";
    held(&r, "TWICE-1", uuid, "ERROR").await;
    r.swish.clone().resolves_for(uuid, "ERROR");
    assert_eq!(redo(&r, "TWICE-1", "TWICE-1-B").await.0, 202);
    assert!(common::stage(&r.state, PAYER, "TWICE-1-B", "ERROR").await);
    assert!(error_lines(&r).await.is_empty());

    assert_eq!(callback(&r, "TWICE-1", uuid, "PAID").await, 200);
    let lines = announced_error_lines(&r).await;
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].contains("TWICE-1") && lines[0].contains("TWICE-1-B"), "{}", lines[0]);

    let paid = view(&r, "TWICE-1").await;
    assert_eq!(paid["review"], swisha::domain::review::PAID_AFTER_REDO.english, "{paid}");
    let redone = view(&r, "TWICE-1-B").await;
    assert_eq!(redone["review"], swisha::domain::review::EARLIER_PAID.english, "{redone}");
    assert_eq!(redone["actions"], json!([]), "a failed redo is not redone again: {redone}");
}

// Swish reports a payout failed after a person confirmed it paid. The confirmation stands, but the
// contradiction must not pass in silence: an error line, and a review saying what to check.
#[tokio::test]
async fn safety_a_failure_after_a_confirmation_cannot_be_missed() {
    let r = rig().await;
    let uuid = "CF0000000000000000000000000000CF";
    held(&r, "CONF-1", uuid, "NEEDS_REVIEW").await;
    let confirmed = reqwest::Client::new()
        .post(format!("{}/swish/payout/{PAYER}/CONF-1/confirm", r.internal))
        .header("x-api-secret", SECRET)
        .header(common::KEY_HEADER, common::key(common::PAYER))
        .json(&json!({ "by": "Anna", "note": "Bank statement shows it arrived" }))
        .send()
        .await
        .expect("request");
    assert_eq!(confirmed.status().as_u16(), 200);
    assert!(view(&r, "CONF-1").await["review"].is_null(), "nothing to check once confirmed");

    assert_eq!(callback(&r, "CONF-1", uuid, "ERROR").await, 200);
    assert_eq!(status_of(&r, "CONF-1").await.as_deref(), Some("PAID"), "the confirmation stands");
    let lines = error_lines(&r).await;
    assert_eq!(lines.len(), 1, "{lines:?}");
    let line = &lines[0];
    assert!(line.contains("CONF-1") && line.contains("ERROR") && line.contains("Anna"), "{line}");
    let paid = view(&r, "CONF-1").await;
    assert_eq!(paid["review"], swisha::domain::review::CONTRADICTED.english, "{paid}");
}

// The same, found by a check rather than a callback: every road a PAID takes raises it.
#[tokio::test]
async fn safety_a_payout_a_check_finds_paid_after_its_redo_cannot_be_missed() {
    let r = rig_with(MockSwish::new().accepts()).await;
    let uuid = "PC0000000000000000000000000000PC";
    held(&r, "TWICE-3", uuid, "ERROR").await;
    r.swish.clone().resolves_for(uuid, "ERROR");
    assert_eq!(redo(&r, "TWICE-3", "TWICE-3-B").await.0, 202);
    assert!(error_lines(&r).await.is_empty());

    r.swish.clone().resolves_for(uuid, "PAID");
    swisha::swish::reconcile::reconcile(PAYER.into(), "TWICE-3".into(), false, r.state.clone()).await;
    let lines = error_lines(&r).await;
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].contains("TWICE-3") && lines[0].contains("TWICE-3-B"), "{}", lines[0]);
}

// The same, with the redo committing while the PAID waits on its lock. Read in the PAID's own
// statement, the redo would be missed.
#[tokio::test]
async fn safety_a_payout_paid_while_it_is_redone_cannot_be_missed() {
    let r = rig_with(MockSwish::new().accepts()).await;
    let uuid = "PW0000000000000000000000000000PW";
    held(&r, "TWICE-2", uuid, "ERROR").await;
    let table = r.state.config.table_payouts.clone();
    let mut redoing = r.state.store.pool().begin().await.expect("begin");
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT status FROM {table} WHERE payer = $1 AND reference = 'TWICE-2' FOR UPDATE"
    )))
    .bind(PAYER)
    .execute(&mut *redoing)
    .await
    .expect("lock");
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "INSERT INTO {table} (payer, reference, payer_alias, payee_alias, amount, message, swish_ref, status, replaces)
         VALUES ($1, 'TWICE-2-B', '1234679304', '46701234567', 100.0, 'safety', 'PW00000000000000000000000000000B', 'CREATED', 'TWICE-2')"
    )))
    .bind(PAYER)
    .execute(&mut *redoing)
    .await
    .expect("claim the redo");

    let state = r.state.clone();
    let callback_url = r.callback.clone();
    let paid = tokio::spawn(async move {
        let echoed = common::callback_identifier(&state, PAYER, "TWICE-2").await;
        reqwest::Client::new()
            .post(format!("{callback_url}/swish/callback"))
            .header("callbackIdentifier", echoed)
            .json(&json!({
                "payerAlias": common::PAYER_NUMBER,
                "payerPaymentReference": "TWICE-2",
                "payoutInstructionUUID": "PW0000000000000000000000000000PW",
                "status": "PAID",
            }))
            .send()
            .await
            .expect("request")
            .status()
            .as_u16()
    });
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(!paid.is_finished(), "the PAID waits for the redo's lock");
    redoing.commit().await.expect("commit");

    assert_eq!(paid.await.expect("join"), 200);
    let lines = announced_error_lines(&r).await;
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].contains("TWICE-2-B"), "{}", lines[0]);
}

// The trail opens before the instruction leaves, not merely before the answer: read while Swish
// is still taking the POST, INITIATED is there, so a stop mid-send leaves the payout on record.
#[tokio::test]
async fn safety_the_trail_opens_before_the_instruction_leaves() {
    let r = rig_with(MockSwish::new().accepts().answers_after(std::time::Duration::from_secs(1))).await;
    let internal = r.internal.clone();
    let paying = tokio::spawn(async move {
        reqwest::Client::new()
            .post(format!("{internal}/swish/payout"))
            .header("x-api-secret", SECRET)
            .header(common::KEY_HEADER, common::key(PAYER))
            .json(&body("A-9"))
            .send()
            .await
            .expect("request")
            .status()
            .as_u16()
    });
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    let opened: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT event FROM {} WHERE reference = 'A-9' ORDER BY id",
        r.state.config.table_events
    )))
    .fetch_all(r.state.store.pool())
    .await
    .expect("events");
    assert_eq!(opened, ["INITIATED"], "the POST is still being answered");
    assert!(r.swish.posted().is_empty(), "and Swish has not answered it yet");
    assert_eq!(paying.await.expect("join"), 202);
}
