#![cfg(feature = "http")]

// Adversarial tests. Every one of these is an attempt to make swisha pay twice, pay the wrong
// person, accept an unauthenticated instruction, or leak something it should not. A test here
// passing means the attack failed.

mod common;

use common::{MockSwish, PAYER, PHONE, SECRET, SSN};
use serde_json::{json, Value};
use swisha::state::SharedState;
use swisha::store::PayoutStore;

struct Target {
    internal: String,
    callback: String,
    state: SharedState,
    swish: MockSwish,
}

// Swish accepts each payout and never gives an outcome, so nothing but a forged callback could
// move a payout's status while a test looks.
async fn target() -> Target {
    let swish = MockSwish::new().accepts();
    let url = swish.clone().start().await;
    let mut config = common::config();
    config.swish_base_url = url;
    let state = common::state_with(config).await;
    Target {
        internal: common::serve(swisha::http::internal_router().with_state(state.clone())).await,
        callback: common::serve(swisha::http::callback_router().with_state(state.clone())).await,
        state,
        swish,
    }
}

fn body(reference: &str) -> Value {
    json!({ "reference": reference, "payee_alias": PHONE, "payee_ssn": SSN, "amount": 100.0 })
}

async fn pay(t: &Target, secret: Option<(&str, &str)>, payload: Value) -> (u16, String) {
    // Always the right key, so every refusal here is the secret's.
    let mut req = reqwest::Client::new()
        .post(format!("{}/swish/payout", t.internal))
        .header(common::KEY_HEADER, common::key(PAYER))
        .json(&payload);
    if let Some((name, value)) = secret {
        req = req.header(name, value);
    }
    let r = req.send().await.expect("request");
    (r.status().as_u16(), r.text().await.unwrap_or_default())
}

async fn authed(t: &Target, payload: Value) -> (u16, String) {
    pay(t, Some(("x-api-secret", SECRET)), payload).await
}

// Authentication

#[tokio::test]
async fn attack_wrong_secret_is_refused() {
    let t = target().await;
    for wrong in ["", " ", "wrong", "0000000000000000000000000000000000"] {
        assert_eq!(
            pay(&t, Some(("x-api-secret", wrong)), body("A-2")).await.0,
            401,
            "{wrong:?} must not authenticate"
        );
    }
}

// A comparison that stops at the first differing byte lets an attacker walk the secret one
// character at a time. Neither a correct prefix nor a correct-plus-extra may be accepted.
#[tokio::test]
async fn attack_partial_secret_never_authenticates() {
    let t = target().await;
    for len in 1..SECRET.len() {
        assert_eq!(
            pay(&t, Some(("x-api-secret", &SECRET[..len])), body("A-3")).await.0,
            401,
            "a {len}-character prefix must not authenticate"
        );
    }
    assert_eq!(pay(&t, Some(("x-api-secret", &format!("{SECRET}x"))), body("A-3")).await.0, 401);
}

// Surrounding whitespace is stripped from a header value by HTTP itself, so a padded secret is
// the same secret arriving, not a bypass: the exact bytes are still required. Pinned so nobody
// later reads it as a hole, and so a change to that handling is noticed.
#[tokio::test]
async fn attack_padding_around_the_secret_is_the_same_secret() {
    let t = target().await;
    for padded in [format!(" {SECRET}"), format!("{SECRET} "), format!("{SECRET}\t")] {
        assert_ne!(
            pay(&t, Some(("x-api-secret", &padded)), body(&format!("A-4-{}", padded.len()))).await.0,
            401,
            "HTTP strips the padding, so this is the correct secret"
        );
    }
    // Padding *inside* the value is part of it, and must not authenticate.
    for forged in [
        format!("{} {}", &SECRET[..4], &SECRET[5..]),
        SECRET.replace("0123", "0 23"),
    ] {
        assert_eq!(
            pay(&t, Some(("x-api-secret", &forged)), body("A-4-x")).await.0,
            401,
            "{forged:?} is a different secret"
        );
    }
}

// A control character cannot be carried in a header value at all, so a secret containing one is
// unrepresentable rather than merely rejected.
#[tokio::test]
async fn attack_a_null_byte_cannot_be_smuggled_into_the_secret() {
    let forged = format!("{}\u{0}", &SECRET[..SECRET.len() - 1]);
    assert!(
        reqwest::header::HeaderValue::from_str(&forged).is_err(),
        "a null byte is not a valid header value"
    );
}

// HTTP header names are case-insensitive, so the guard must not be bypassable by changing case
// in either direction: a different case must neither slip past nor be rejected wrongly.
#[tokio::test]
async fn attack_header_case_does_not_change_the_outcome() {
    let t = target().await;
    for name in ["X-API-SECRET", "X-Api-Secret", "x-ApI-sEcReT"] {
        assert_ne!(
            pay(&t, Some((name, SECRET)), body(&format!("A-5-{name}"))).await.0,
            401,
            "{name} is the same header and carries the right secret"
        );
    }
    assert_eq!(pay(&t, Some(("X-API-SECRET", "nope")), body("A-5-x")).await.0, 401);
}

// Address spoofing and rate limiting

// The limiter is one map for the process, keyed by caller and number, so a test of the limit pays
// from a number of its own, with the budget it names.
async fn limited(name: &str, number: &str, per_minute: u32, trust_loopback: bool) -> (String, SharedState) {
    let swish = MockSwish::new().accepts().resolves_to("PAID");
    let mut config = common::config();
    config.swish_base_url = swish.start().await;
    config.payouts_per_minute = per_minute;
    if trust_loopback {
        config.trusted_proxies = vec!["127.0.0.1".parse().expect("loopback")];
    }
    let state = common::state_with_payers(config, vec![common::payer(name, number)]).await;
    (common::serve(swisha::http::internal_router().with_state(state.clone())).await, state)
}

async fn pay_from(base: &str, name: &str, reference: &str, forwarded: Option<String>) -> u16 {
    let mut request = reqwest::Client::new()
        .post(format!("{base}/swish/payout"))
        .header("x-api-secret", SECRET)
        .header(common::KEY_HEADER, common::key(name))
        .json(&body(reference));
    if let Some(forwarded) = forwarded {
        request = request.header("x-forwarded-for", forwarded);
    }
    request.send().await.expect("request").status().as_u16()
}

#[tokio::test]
async fn attack_forwarded_header_is_ignored_from_an_untrusted_peer() {
    let (base, _) = limited("limit-a", "1239990001", 30, false).await;
    // 31 requests, each claiming a different origin. If the claim were believed, none would be
    // limited; because it is not, they all share the loopback bucket and the last is refused.
    let mut last = 0;
    for i in 0..31 {
        last = pay_from(&base, "limit-a", &format!("S-1-{i}"), Some(format!("203.0.113.{}", i + 1))).await;
    }
    assert_eq!(last, 429, "spoofed origins must not each get their own budget");
}

// TRUSTED_PROXY names the proxy in front of the callback. On the internal API a caller that
// could set X-Forwarded-For would pick its own budget and the address the audit trail keeps.
#[tokio::test]
async fn attack_a_forwarded_address_never_picks_an_internal_callers_budget() {
    let (base, state) = limited("limit-b", "1239990002", 30, true).await;
    let mut last = 0;
    for i in 0..31 {
        last = pay_from(&base, "limit-b", &format!("S-2-{i}"), Some(format!("203.0.113.{}", i + 1))).await;
    }
    assert_eq!(last, 429, "every request came from loopback, whatever it claimed");
    let ips: Vec<Option<String>> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT ip FROM {} WHERE reference = 'S-2-0'",
        state.config.table_events
    )))
    .fetch_all(state.store.pool())
    .await
    .expect("events");
    assert!(!ips.is_empty() && ips.iter().all(|ip| ip.as_deref() == Some("127.0.0.1")), "{ips:?}");
}

// The route takes each number's budget from the configuration.
#[tokio::test]
async fn the_payout_route_spends_the_configured_budget() {
    let (base, _) = limited("limit-c", "1239990003", 3, false).await;
    let mut codes = Vec::new();
    for i in 0..4 {
        codes.push(pay_from(&base, "limit-c", &format!("S-9-{i}"), None).await);
    }
    assert_eq!(codes, [202, 202, 202, 429]);
}

// A redo is a payout, and spends the same budget: past it, 429 before anything is read or asked.
#[tokio::test]
async fn a_redo_spends_the_payout_budget() {
    let (base, _) = limited("limit-e", "1239990005", 1, false).await;
    let mut codes = Vec::new();
    for _ in 0..2 {
        let code = reqwest::Client::new()
            .post(format!("{base}/swish/payout/limit-e/R-1/redo"))
            .header("x-api-secret", SECRET)
            .header(common::KEY_HEADER, common::key("limit-e"))
            .json(&json!({ "reference": "not a reference", "by": "Anna" }))
            .send()
            .await
            .expect("request")
            .status()
            .as_u16();
        codes.push(code);
    }
    assert_eq!(codes, [400, 429], "the first spends the budget, the second finds it spent");
}

#[tokio::test]
async fn attack_the_rate_limit_actually_stops_a_flood() {
    let (base, _) = limited("limit-d", "1239990004", 30, true).await;
    let mut refused = 0;
    for i in 0..40 {
        if pay_from(&base, "limit-d", &format!("S-3-{i}"), Some("203.0.113.254".into())).await == 429 {
            refused += 1;
        }
    }
    assert_eq!(refused, 10, "a flood from one origin is cut off at its budget");
}

// SQL injection

// Every value is bound, never interpolated. If that were not so, this reference would drop the
// table it is being written into.
#[tokio::test]
async fn attack_sql_injection_in_the_reference_is_stored_as_text() {
    let t = target().await;
    let payload = "'; DROP TABLE swisha_payouts; --";

    // The route refuses it outright: Swish only takes letters, digits and hyphens.
    let (code, _) = authed(&t, body(payload)).await;
    assert_eq!(code, 400, "not a reference Swish accepts");
    assert!(t.swish.posted().is_empty());

    // The store must still treat it as text for anyone reaching it without the route, such as a
    // library caller: values are bound, never spliced into the statement.
    t.state
        .store
        .claim(&swisha::domain::payout::NewPayout {
            payer: PAYER,
            reference: payload,
            payer_alias: common::PAYER_NUMBER,
            payee_alias: "46701234567",
            payee_ssn: Some(SSN),
            amount: 100.0,
            message: "x",
            swish_ref: "AAAABBBBCCCCDDDDEEEEFFFF00003333",
            callback_identifier: None,
        })
        .await
        .expect("it is just text");
    let snapshot = t.state.store.snapshot(PAYER, payload).await.expect("store alive").expect("row");
    assert!(snapshot.swish_ref.is_some(), "the row exists, so the table does");
}

#[tokio::test]
async fn attack_sql_injection_in_other_fields_changes_nothing() {
    let t = target().await;
    for (field, value) in [
        ("message", "x'); DELETE FROM swisha_payouts WHERE ('1'='1"),
        ("reference", "1' OR '1'='1"),
        ("reference", "\\'; DROP TABLE swisha_events; --"),
    ] {
        let mut b = body("S-4");
        b[field] = json!(value);
        if field == "message" {
            b["reference"] = json!(format!("S-4-{}", value.len()));
        }
        let (code, _) = authed(&t, b).await;
        assert!(code == 202 || code == 400, "unexpected {code} for {field}");
        assert!(t.state.store.snapshot(PAYER, "S-4").await.is_ok(), "the table survived {value}");
    }
}

#[tokio::test]
async fn attack_injection_through_the_status_path_is_not_a_query() {
    let t = target().await;
    for probe in ["A'--", "1%27%20OR%20%271%27%3D%271", "..%2F..%2Fetc%2Fpasswd"] {
        let code = reqwest::Client::new()
            .get(format!("{}/swish/status/{probe}", t.internal))
            .header("x-api-secret", SECRET)
            .header(common::KEY_HEADER, common::key(common::PAYER))
            .send()
            .await
            .expect("request")
            .status()
            .as_u16();
        assert!(code == 404 || code == 400, "{probe} produced {code}");
        assert!(t.state.store.snapshot(PAYER, "S-5").await.is_ok(), "the table survived {probe}");
    }
}

// What reaches Swish

// The instruction is signed over exactly the bytes that are sent. A reference containing quotes
// or braces must not be able to restructure that JSON.
#[tokio::test]
async fn attack_json_breaking_reference_cannot_restructure_the_instruction() {
    let t = target().await;
    let hostile = r#"a","amount":"99999","x":"b"#;
    assert_eq!(authed(&t, body(hostile)).await.0, 400, "not a reference Swish accepts");
    assert!(t.swish.posted().is_empty(), "nothing reached Swish");

    // The payload builder must still escape it for anyone reaching it without the route.
    use swisha::swish::payload::{ExecutePayoutArgs, PayoutPayload};
    let args = ExecutePayoutArgs {
        reference: hostile,
        payout_uuid: "ABC123",
        payee_alias: "46701234567",
        payee_ssn: Some(SSN),
        amount: 100.0,
        message: "x",
    };
    let built = PayoutPayload::new(&args, common::PAYER_NUMBER, "00", "2026-10-07T12:00:00Z".into());
    let payload: Value = serde_json::to_value(&built).expect("json");
    assert_eq!(payload["amount"], "100.00", "the amount must be the one that was validated");
    assert_eq!(payload["payerPaymentReference"], hostile, "escaped, not interpreted");
}

#[tokio::test]
async fn attack_control_characters_never_reach_swish() {
    let t = target().await;
    for (field, value) in [
        ("reference", "A\r\nHost: evil"),
        ("reference", "A\u{0}B"),
        ("message", "A\r\nX-Injected: 1"),
        ("message", "A\u{0}B"),
    ] {
        let mut b = body("W-2");
        b[field] = json!(value);
        assert_eq!(authed(&t, b).await.0, 400, "{field} {value:?} must be refused");
    }
    assert!(t.swish.posted().is_empty(), "nothing was sent");
}

#[tokio::test]
async fn attack_the_amount_ceiling_cannot_be_stepped_over() {
    let t = target().await;
    for over in [50_000.01, 50_001.0, 1_000_000.0, f64::MAX] {
        let mut b = body("W-3");
        b["amount"] = json!(over);
        assert_eq!(authed(&t, b).await.0, 400, "{over} is above the ceiling");
    }
    assert!(t.swish.posted().is_empty());
}

#[tokio::test]
async fn attack_a_negative_amount_cannot_reverse_a_payout() {
    let t = target().await;
    for under in [-0.01, -100.0, -50_000.0, 0.0] {
        let mut b = body("W-4");
        b["amount"] = json!(under);
        assert_eq!(authed(&t, b).await.0, 400, "{under} is not a payout");
    }
    assert!(t.swish.posted().is_empty());
}

// Callback forgery

async fn forge(t: &Target, reference: &str, uuid: &str, status: &str) -> u16 {
    // The identifier is echoed as Swish would, so these tests reach the UUID and status checks.
    let echoed = common::callback_identifier(&t.state, PAYER, reference).await;
    reqwest::Client::new()
        .post(format!("{}/swish/callback", t.callback))
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

// A callback names the number that paid. One swisha does not pay from cannot belong to any
// payout here, whatever the reference and UUID say.
#[tokio::test]
async fn attack_a_callback_from_a_number_swisha_does_not_pay_from_changes_nothing() {
    let t = target().await;
    let (_, first) = authed(&t, body("C-0")).await;
    let uuid = serde_json::from_str::<Value>(&first).expect("json")["swish_ref"]
        .as_str()
        .expect("uuid")
        .to_string();
    let before = t.state.store.snapshot(PAYER, "C-0").await.expect("store").expect("row");
    let echoed = common::callback_identifier(&t.state, PAYER, "C-0").await;

    let code = reqwest::Client::new()
        .post(format!("{}/swish/callback", t.callback))
        .header("callbackIdentifier", echoed)
        .json(&json!({
            "payerAlias": "1239999999",
            "payerPaymentReference": "C-0",
            "payoutInstructionUUID": uuid,
            "status": "PAID",
        }))
        .send()
        .await
        .expect("request")
        .status()
        .as_u16();
    assert_eq!(code, 200, "acknowledged, so Swish stops redelivering");

    let after = t.state.store.snapshot(PAYER, "C-0").await.expect("store").expect("row");
    assert_eq!(after.status, before.status, "another number's callback must not settle this payout");
}

// The callback listener is the one reachable from the internet. Knowing a reference must not be
// enough to declare a payout settled: the instruction UUID has to match too.
#[tokio::test]
async fn attack_a_callback_with_the_wrong_uuid_changes_nothing() {
    let t = target().await;
    authed(&t, body("C-1")).await;
    let before = t.state.store.snapshot(PAYER, "C-1").await.expect("store").expect("row");

    forge(&t, "C-1", "00000000000000000000000000000000", "PAID").await;

    let after = t.state.store.snapshot(PAYER, "C-1").await.expect("store").expect("row");
    assert_eq!(after.status, before.status, "a guessed UUID must not settle a payout");
}

#[tokio::test]
async fn attack_a_callback_cannot_walk_a_paid_payout_backwards() {
    let t = target().await;
    let (_, response) = authed(&t, body("C-3")).await;
    let uuid = serde_json::from_str::<Value>(&response).expect("json")["swish_ref"]
        .as_str()
        .expect("uuid")
        .to_string();
    common::stage(&t.state, PAYER, "C-3", "PAID").await;

    for status in ["ERROR", "DECLINED", "PENDING", "CREATED"] {
        forge(&t, "C-3", &uuid, status).await;
        let snapshot = t.state.store.snapshot(PAYER, "C-3").await.expect("store").expect("row");
        assert_eq!(snapshot.status.as_deref(), Some("PAID"), "{status} must not move a settled payout");
    }
}

// Disclosure

#[tokio::test]
async fn attack_the_status_endpoint_reveals_nothing_for_an_unknown_reference() {
    let t = target().await;
    let response = reqwest::Client::new()
        .get(format!("{}/swish/status/NOT-A-PAYOUT", t.internal))
        .header("x-api-secret", SECRET)
        .header(common::KEY_HEADER, common::key(common::PAYER))
        .send()
        .await
        .expect("request");
    assert_eq!(response.status().as_u16(), 404);
    let body = response.text().await.expect("body");
    assert!(!body.contains("swisha_payouts"), "no schema in the answer: {body}");
    assert!(!body.contains("SELECT"), "no SQL in the answer: {body}");
}

#[tokio::test]
async fn attack_an_unauthenticated_caller_learns_nothing_from_the_refusal() {
    let t = target().await;
    let (code, body) = pay(&t, None, body("X-1")).await;
    assert_eq!(code, 401);
    assert!(!body.contains(SECRET), "the refusal must not echo the expected secret");
    assert!(body.len() < 200, "a refusal has nothing to explain: {body}");
}

// A rejection from Swish is surfaced so a person can act on the code, but it must carry only
// what Swish said, not swisha's own configuration.
#[tokio::test]
async fn attack_a_swish_rejection_leaks_no_local_configuration() {
    let swish = MockSwish::new().rejects(422, r#"[{"errorCode":"PA02"}]"#);
    let url = swish.start().await;
    let mut config = common::config();
    config.swish_base_url = url.clone();
    let state = common::state_with(config).await;
    let base = common::serve(swisha::http::internal_router().with_state(state)).await;

    let response = reqwest::Client::new()
        .post(format!("{base}/swish/payout"))
        .header("x-api-secret", SECRET)
        .header(common::KEY_HEADER, common::key(common::PAYER))
        .json(&body("X-2"))
        .send()
        .await
        .expect("request")
        .text()
        .await
        .expect("body");

    assert!(response.contains("PA02"), "the code a person needs is shown");
    assert!(!response.contains(SECRET), "but not the secret");
    assert!(!response.contains("1234679304"), "nor the merchant number");
}

// Request shape

#[tokio::test]
async fn attack_unknown_fields_cannot_smuggle_values() {
    let t = target().await;
    for smuggled in ["swish_ref", "status", "attempts", "payer_alias", "signing_key"] {
        let mut b = body("R-1");
        b[smuggled] = json!("attacker-controlled");
        let (code, _) = authed(&t, b).await;
        assert_eq!(code, 422, "{smuggled} must be refused, not ignored");
    }
    assert!(t.swish.posted().is_empty());
}

#[tokio::test]
async fn attack_a_wrong_typed_field_is_refused() {
    let t = target().await;
    for (field, value) in [
        ("amount", json!("100.00")),
        ("reference", json!(12345)),
        ("payee_alias", json!(null)),
        ("payee_ssn", json!(196408233234i64)),
    ] {
        let mut b = body("R-2");
        b[field] = value.clone();
        let (code, _) = authed(&t, b).await;
        assert_eq!(code, 422, "{field} as {value} must be refused");
    }
}

#[tokio::test]
async fn attack_a_malformed_body_never_reaches_the_handler() {
    let t = target().await;
    for raw in ["", "{", "[]", "null", r#"{"reference":"#] {
        let code = reqwest::Client::new()
            .post(format!("{}/swish/payout", t.internal))
            .header("x-api-secret", SECRET)
            .header(common::KEY_HEADER, common::key(common::PAYER))
            .header("content-type", "application/json")
            .body(raw)
            .send()
            .await
            .expect("request")
            .status()
            .as_u16();
        assert!(code == 400 || code == 422, "{raw:?} produced {code}");
    }
    assert!(t.swish.posted().is_empty());
}

// Swish echoes each instruction's callbackIdentifier as a header. Without it, or with the wrong
// one, nothing moves; in test mode, with no address allowlist, this is the proof it was Swish.
#[tokio::test]
async fn attack_a_callback_without_the_echoed_identifier_changes_nothing() {
    let t = target().await;
    let (_, first) = authed(&t, body("C-9")).await;
    let uuid = serde_json::from_str::<Value>(&first).expect("json")["swish_ref"]
        .as_str()
        .expect("uuid")
        .to_string();
    let before = t.state.store.snapshot(PAYER, "C-9").await.expect("store").expect("row");
    let echoed = common::callback_identifier(&t.state, PAYER, "C-9").await;
    assert_eq!(echoed.len(), 32, "the payout was stored with its identifier");

    let send = |header: Option<String>| {
        let url = format!("{}/swish/callback", t.callback);
        let uuid = uuid.clone();
        async move {
            let mut request = reqwest::Client::new().post(url).json(&json!({
                "payerAlias": common::PAYER_NUMBER,
                "payerPaymentReference": "C-9",
                "payoutInstructionUUID": uuid,
                "status": "PAID",
            }));
            if let Some(header) = header {
                request = request.header("callbackIdentifier", header);
            }
            request.send().await.expect("request").status().as_u16()
        }
    };

    for forged in [None, Some("0".repeat(32)), Some(echoed[..31].to_string())] {
        assert_eq!(send(forged.clone()).await, 200, "acknowledged, so a forger learns nothing");
        let after = t.state.store.snapshot(PAYER, "C-9").await.expect("store").expect("row");
        assert_eq!(after.status, before.status, "{forged:?} must not settle the payout");
    }

    assert_eq!(send(Some(echoed)).await, 200);
    let settled = t.state.store.snapshot(PAYER, "C-9").await.expect("store").expect("row");
    assert_eq!(settled.status.as_deref(), Some("PAID"), "the echoed identifier settles it");
}

// Each instruction carries its own fresh identifier, sent to Swish and stored with the payout.
#[tokio::test]
async fn each_payout_sends_and_stores_its_own_callback_identifier() {
    let t = target().await;
    authed(&t, body("C-10")).await;
    authed(&t, body("C-11")).await;

    let sent: Vec<String> = t
        .swish
        .posted()
        .iter()
        .map(|post| post["callbackIdentifier"].as_str().expect("sent").to_string())
        .collect();
    assert_eq!(sent.len(), 2);
    assert_ne!(sent[0], sent[1], "a fresh identifier per payout");
    for (reference, identifier) in ["C-10", "C-11"].iter().zip(&sent) {
        assert_eq!(identifier.len(), 32);
        assert!(identifier.bytes().all(|b| b.is_ascii_hexdigit()), "within Swish's [0-9a-zA-Z-]{{32,36}}");
        assert_eq!(&common::callback_identifier(&t.state, PAYER, reference).await, identifier);
    }
}

// A payout stored before swisha sent identifiers has none to compare, so its callbacks are still
// matched by UUID alone rather than refused for an echo that was never asked for.
#[tokio::test]
async fn a_payout_from_before_identifiers_still_settles_by_uuid() {
    let t = target().await;
    let uuid = "AAAABBBBCCCCDDDDEEEEFFFF00004444";
    t.state
        .store
        .claim(&swisha::domain::payout::NewPayout {
            payer: PAYER,
            reference: "C-12",
            payer_alias: common::PAYER_NUMBER,
            payee_alias: "46701234567",
            payee_ssn: Some(SSN),
            amount: 100.0,
            message: "x",
            swish_ref: uuid,
            callback_identifier: None,
        })
        .await
        .expect("claim");

    forge(&t, "C-12", uuid, "PAID").await;
    let after = t.state.store.snapshot(PAYER, "C-12").await.expect("store").expect("row");
    assert_eq!(after.status.as_deref(), Some("PAID"));
}

// A callback listener bound to [::], behind a proxy on the same host, sees every connection as
// ::ffff:127.0.0.1. In production a callback forwarded for one of Swish's addresses still lands.
#[tokio::test]
async fn a_production_callback_reaches_a_listener_bound_to_every_address() {
    const UUID: &str = "AAAABBBBCCCCDDDDEEEEFFFF00006666";
    let mut config = common::config();
    config.swish_env = "production".into();
    config.trusted_proxies = vec!["127.0.0.1".parse().expect("loopback")];
    let state = common::state_with(config).await;
    let staged = swisha::domain::payout::NewPayout {
        payer: PAYER,
        reference: "V6-1",
        payer_alias: common::PAYER_NUMBER,
        payee_alias: PHONE,
        payee_ssn: Some(SSN),
        amount: 100.0,
        message: "v6",
        swish_ref: UUID,
        callback_identifier: None,
    };
    state.store.claim(&staged).await.expect("claim");

    let listener = tokio::net::TcpListener::bind("[::]:0").await.expect("bind every address");
    let port = listener.local_addr().expect("address").port();
    let router = swisha::http::callback_router().with_state(state.clone());
    tokio::spawn(async move {
        axum::serve(listener, router.into_make_service_with_connect_info::<std::net::SocketAddr>()).await.ok();
    });

    let answered = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/swish/callback"))
        .header("x-forwarded-for", "89.46.83.171")
        .json(&json!({
            "payerAlias": common::PAYER_NUMBER,
            "payerPaymentReference": "V6-1",
            "payoutInstructionUUID": UUID,
            "status": "DEBITED",
        }))
        .send()
        .await
        .expect("swisha answered")
        .status()
        .as_u16();
    assert_eq!(answered, 200, "Swish's own callback, through the proxy");
    let status = state.store.snapshot(PAYER, "V6-1").await.expect("store").and_then(|s| s.status);
    assert_eq!(status.as_deref(), Some("DEBITED"));
}

// In production only Swish's eight published addresses may report an outcome. Anything else is
// refused before the payout is read, and the payout is left as it was.
#[tokio::test]
async fn in_production_only_swishs_addresses_may_report_an_outcome() {
    const UUID: &str = "AAAABBBBCCCCDDDDEEEEFFFF00007777";
    let published = [
        "193.53.81.202", "193.53.81.203", "193.53.81.204", "193.53.81.205",
        "213.132.115.86", "213.132.115.90", "213.132.115.94", "89.46.83.171",
    ];
    assert_eq!(swisha::http::routes::callback::active_callback_ips(), published);

    let mut config = common::config();
    config.swish_env = "production".into();
    config.trusted_proxies = vec!["127.0.0.1".parse().expect("loopback")];
    let state = common::state_with(config).await;
    let staged = swisha::domain::payout::NewPayout {
        payer: PAYER,
        reference: "PROD-1",
        payer_alias: common::PAYER_NUMBER,
        payee_alias: PHONE,
        payee_ssn: Some(SSN),
        amount: 100.0,
        message: "production",
        swish_ref: UUID,
        callback_identifier: None,
    };
    state.store.claim(&staged).await.expect("claim");
    let callback = common::serve(swisha::http::callback_router().with_state(state.clone())).await;
    let report = |headers: &[(&'static str, &'static str)]| {
        let mut request = reqwest::Client::new().post(format!("{callback}/swish/callback")).json(&json!({
            "payerAlias": common::PAYER_NUMBER,
            "payerPaymentReference": "PROD-1",
            "payoutInstructionUUID": UUID,
            "status": "DEBITED",
        }));
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        async move { request.send().await.expect("swisha answered").status().as_u16() }
    };
    let status = || async {
        state.store.snapshot(PAYER, "PROD-1").await.expect("store").and_then(|s| s.status).unwrap_or_default()
    };

    assert_eq!(report(&[("x-forwarded-for", "203.0.113.9")]).await, 403, "an address Swish never calls from");
    assert_eq!(report(&[]).await, 403, "the proxy itself is not Swish");
    assert_eq!(
        report(&[("x-forwarded-for", "10.0.0.5"), ("x-real-ip", "89.46.83.171")]).await,
        403,
        "a Swish address the caller put in X-Real-IP"
    );
    assert_eq!(status().await, "CREATED", "nothing changed");
    assert_eq!(report(&[("x-forwarded-for", "89.46.83.171")]).await, 200, "one of Swish's own");
    assert_eq!(status().await, "DEBITED");
}

// A redirect is followed with the same body, so a 307 from anything posing as Swish would hand the
// signed payout, personnummer included, to the host it names. No client of swisha's follows one.
#[tokio::test]
async fn attack_a_redirect_never_carries_a_payout_elsewhere() {
    use axum::http::{StatusCode, header::LOCATION};
    let reached = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let heard = reached.clone();
    let elsewhere = common::serve(axum::Router::new().fallback(move |body: String| {
        let heard = heard.clone();
        async move {
            heard.lock().expect("record").push(body);
            "{}"
        }
    }))
    .await;
    let redirector = common::serve(axum::Router::new().fallback(move || {
        let to = format!("{elsewhere}/elsewhere");
        async move { (StatusCode::TEMPORARY_REDIRECT, [(LOCATION, to)]) }
    }))
    .await;

    let client = swisha::swish::client::build_swish_client(&common::bundle("1231234567", "30")).expect("client");
    let payout = json!({ "payeeSSN": SSN, "payeeAlias": "46701234567" });
    let sent = swisha::swish::client::swish_post(&format!("{redirector}/swish-cpcapi/api/v1/payouts"), &payout, &client)
        .await
        .expect("an answer");
    assert_eq!(sent.status().as_u16(), 307, "the redirect is Swish's answer, not a step to take");
    let asked = swisha::swish::client::swish_get(&format!("{redirector}/swish-cpcapi/api/v1/payouts/X"), &client)
        .await
        .expect("an answer");
    assert_eq!(asked.status().as_u16(), 307);
    assert!(swisha::swish::status_page::fetch(&format!("{redirector}/api/v2/summary.json")).await.is_err());
    assert!(reached.lock().expect("record").is_empty(), "nothing reached the other host");
}
