#![cfg(feature = "http")]

// POST /swish/payout end to end, against a scripted stand-in for Swish, so the whole
// accept-and-submit path runs without touching MSS.

mod common;

use common::{MockSwish, PAYER, PHONE, SECRET, SSN};
use sqlx::AssertSqlSafe;
use serde_json::{json, Value};
use swisha::state::SharedState;
use swisha::store::PayoutStore;

async fn app_with(mock: MockSwish) -> (String, SharedState) {
    let swish = mock.start().await;
    let mut config = common::config();
    config.swish_base_url = swish;
    let state = common::state_with(config).await;
    let base = common::serve(swisha::http::internal_router().with_state(state.clone())).await;
    (base, state)
}

async fn app() -> (String, SharedState) {
    app_with(MockSwish::new().accepts().resolves_to("PAID")).await
}

async fn post(base: &str, body: Value) -> (u16, Value) {
    let response = reqwest::Client::new()
        .post(format!("{base}/swish/payout"))
        .header("x-api-secret", SECRET)
        .header(common::KEY_HEADER, common::key(common::PAYER))
        .json(&body)
        .send()
        .await
        .expect("request");
    let status = response.status().as_u16();
    let text = response.text().await.expect("body");
    (status, serde_json::from_str(&text).unwrap_or(Value::String(text)))
}

fn valid() -> Value {
    json!({ "reference": "INV-1", "payee_alias": PHONE, "payee_ssn": SSN, "amount": 100.0 })
}

fn with(field: &str, value: Value) -> Value {
    let mut body = valid();
    body[field] = value;
    body
}

#[tokio::test]
async fn a_valid_payout_is_accepted_and_stored() {
    let (base, state) = app().await;
    let (status, body) = post(&base, valid()).await;

    assert_eq!(status, 202, "body: {body}");
    assert_eq!(body["status"], "CREATED");
    assert_eq!(body["success"], true);
    assert_eq!(
        body["swish_ref"].as_str().expect("swish_ref").len(),
        32,
        "the payout instruction UUID is 32 hex characters"
    );

    let snapshot = state.store.snapshot(PAYER, "INV-1").await.expect("snapshot").expect("row");
    assert_eq!(snapshot.swish_ref.as_deref(), body["swish_ref"].as_str());
}

// At most twice the cores sign at once: with every slot taken, a payout waits for one instead of
// taking a thread of its own, and goes as soon as one frees.
#[tokio::test]
async fn a_payout_waits_for_a_free_signing_slot() {
    let swish = MockSwish::new().accepts().resolves_to("PAID");
    let (base, state) = app_with(swish.clone()).await;
    let slots = u32::try_from(state.signing.available_permits()).expect("slots");
    assert!(slots >= 2, "twice the cores");
    let held = state.signing.acquire_many(slots).await.expect("every slot");

    let paying = tokio::spawn(async move { post(&base, valid()).await.0 });
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    assert!(swish.posted().is_empty(), "sent without a signing slot");
    assert!(!paying.is_finished(), "answered without a signing slot");

    drop(held);
    assert_eq!(paying.await.expect("join"), 202);
    assert_eq!(swish.posted().len(), 1);
}

#[tokio::test]
async fn the_reference_is_checked_before_anything_is_sent() {
    let (base, _) = app().await;
    for (label, value) in [
        ("empty", json!("")),
        ("blank", json!("   ")),
        ("36 characters", json!("x".repeat(36))),
        ("control character", json!("INV\u{0}1")),
        // Outside the characters Swish takes for payerPaymentReference, which it would refuse
        // with FF08 after the reference was already spent.
        ("a space", json!("INV 1")),
        ("an underscore", json!("INV_1")),
        ("a slash", json!("INV/1")),
        ("a dot", json!("INV.1")),
        ("a letter outside a-z", json!("Göteborg1")),
        ("a hash", json!("Kvitto#7")),
    ] {
        let (status, body) = post(&base, with("reference", value)).await;
        assert_eq!(status, 400, "{label} should be refused, got {body}");
    }
    // Exactly at the limit is allowed, so the boundary is pinned from both sides.
    let (status, _) = post(&base, with("reference", json!("x".repeat(35)))).await;
    assert_eq!(status, 202, "35 characters is the documented maximum");
    for fine in ["INV1042", "inv-1042", "1042"] {
        assert_eq!(post(&base, with("reference", json!(fine))).await.0, 202, "{fine} is a reference Swish takes");
    }
}

#[tokio::test]
async fn the_amount_is_bounded_at_both_ends() {
    let (base, _) = app().await;
    for (label, value) in [
        ("zero", json!(0.0)),
        ("below one", json!(0.99)),
        ("negative", json!(-100.0)),
        ("above the ceiling", json!(50_001.0)),
    ] {
        let (status, body) = post(&base, with("amount", value)).await;
        assert_eq!(status, 400, "{label} should be refused, got {body}");
    }
    for (i, (label, value)) in [("exactly one", json!(1.0)), ("exactly the ceiling", json!(50_000.0))]
        .into_iter()
        .enumerate()
    {
        let mut body = with("amount", value);
        body["reference"] = json!(format!("INV-B{i}"));
        let (status, _) = post(&base, body).await;
        assert_eq!(status, 202, "{label} is inside the range");
    }
}

// JSON has no NaN or infinity, and serde_json refuses an exponent past f64's range as a syntax
// error, so such an amount never reaches the handler. Sent as raw text: the literal won't compile.
#[tokio::test]
async fn a_non_finite_amount_never_reaches_swish() {
    let (base, _) = app().await;
    let raw = format!(
        r#"{{"reference":"INV-INF","payee_alias":"{PHONE}","payee_ssn":"{SSN}","amount":1e309}}"#
    );
    let code = reqwest::Client::new()
        .post(format!("{base}/swish/payout"))
        .header("x-api-secret", SECRET)
        .header(common::KEY_HEADER, common::key(common::PAYER))
        .header("content-type", "application/json")
        .body(raw)
        .send()
        .await
        .expect("request")
        .status()
        .as_u16();
    assert_eq!(code, 400, "an infinite amount must not be treated as a number");
}

// Swish takes at most two decimals. A third is refused rather than rounded, so the amount swisha
// stores and the amount Swish pays cannot drift apart, and no reference is spent on it.
#[tokio::test]
async fn an_amount_with_more_than_two_decimals_is_refused_before_anything_is_sent() {
    let mock = MockSwish::new().accepts().resolves_to("PAID");
    let (base, state) = app_with(mock.clone()).await;

    for (i, amount) in [1.005f64, 100.004, 100.006, 1234.567, 100.0000001].into_iter().enumerate() {
        let reference = format!("DEC-{i}");
        let mut body = with("amount", json!(amount));
        body["reference"] = json!(reference);
        let (status, answer) = post(&base, body).await;
        assert_eq!(status, 400, "{amount} should be refused: {answer}");
        let stored = state.store.snapshot(PAYER, &reference).await.expect("store");
        assert!(stored.is_none(), "{amount} spent a reference");
    }
    assert!(mock.posted().is_empty(), "nothing refused may reach Swish");

    // Binary floating point drifts in the last bits; a value a person typed with two decimals
    // must still go through, and reach Swish as exactly what they typed.
    for (i, amount) in [99.9f64, 1234.5, 49_999.99, 1.0 + 0.1 + 0.2].into_iter().enumerate() {
        let mut body = with("amount", json!(amount));
        body["reference"] = json!(format!("OK-{i}"));
        assert_eq!(post(&base, body).await.0, 202, "{amount} has two decimals");
    }
    let sent: Vec<String> = mock
        .posted()
        .iter()
        .map(|s| common::payload(s)["amount"].as_str().unwrap_or_default().to_string())
        .collect();
    assert_eq!(sent, ["99.90", "1234.50", "49999.99", "1.30"]);
}

#[tokio::test]
async fn the_payee_number_must_be_a_swedish_mobile_number() {
    let (base, _) = app().await;
    for bad in ["", "12345", "+1 555 0100", "46701234567890", "not a number"] {
        let (status, body) = post(&base, with("payee_alias", json!(bad))).await;
        assert_eq!(status, 400, "{bad:?} should be refused, got {body}");
    }
}

// A caller that supplies an identity number asked Swish to check it. Dropping it silently and
// sending the payout anyway is not that check, so a bad one is refused instead.
#[tokio::test]
async fn a_malformed_ssn_is_refused_rather_than_dropped() {
    let (base, _) = app().await;
    for (label, value) in [
        ("ten digits", json!("640823-3234")),
        ("ten digits, centenarian marker", json!("250101+1234")),
        ("failing the luhn check", json!("196408233235")),
        ("thirteen digits", json!("1964082332345")),
        ("letters", json!("19640823ABCD")),
    ] {
        let (status, body) = post(&base, with("payee_ssn", value)).await;
        assert_eq!(status, 400, "{label} should be refused, got {body}");
    }
}

#[tokio::test]
async fn the_twelve_digit_ssn_reaches_swish_verbatim() {
    let mock = MockSwish::new().accepts().resolves_to("PAID");
    let (base, _) = app_with(mock.clone()).await;
    post(&base, with("payee_ssn", json!("19640823-3234"))).await;

    let sent = mock.posted();
    assert_eq!(sent.len(), 1, "exactly one instruction should have been sent");
    let payload = common::payload(&sent[0]);
    assert_eq!(
        payload["payeeSSN"], "196408233234",
        "separators are stripped, the century is not invented"
    );
}

// Asserted on what reaches Swish rather than what is stored: the recipient reads the message
// out of their Swish app, so the wire is where it actually has to be right.
#[tokio::test]
async fn the_message_defaults_to_the_template_and_can_be_overridden() {
    let mock = MockSwish::new().accepts().resolves_to("PAID");
    let (base, _) = app_with(mock.clone()).await;

    post(&base, valid()).await;
    let mut custom = with("message", json!("Scrap metal, week 34"));
    custom["reference"] = json!("INV-2");
    post(&base, custom).await;

    let messages: Vec<String> = mock
        .posted()
        .iter()
        .map(|sent| common::payload(sent)["message"].as_str().unwrap_or_default().to_string())
        .collect();

    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0], "INV-1", "the default template substitutes the reference");
    assert_eq!(messages[1], "Scrap metal, week 34", "a caller may override it");
}

#[tokio::test]
async fn a_second_payout_for_one_reference_is_refused() {
    let (base, _) = app().await;
    assert_eq!(post(&base, valid()).await.0, 202);

    let (status, body) = post(&base, valid()).await;
    assert_eq!(status, 409, "one reference is one payout, forever");
    assert!(
        body["error"].as_str().unwrap_or_default().contains("already"),
        "the refusal should say why: {body}"
    );
}

// The body a live MSS payout without a personnummer produced. Swish stored nothing, so it is
// DECLINED, and the caller gets the code Swish named in the status endpoint's shape.
#[tokio::test]
async fn a_swish_refusal_is_answered_with_the_code_swish_gave() {
    let mock = MockSwish::new()
        .rejects(422, r#"[{"errorCode":"PA06","errorMessage":"Incorrect ssn format"}]"#);
    let (base, state) = app_with(mock).await;

    let (status, body) = post(&base, valid()).await;
    let pa06 = swisha::domain::errors::describe(Some("PA06"));
    assert_eq!(status, 422, "Swish refused the payout's data: {body}");
    assert_eq!(body["status"], "DECLINED");
    assert_eq!(body["error_code"], "PA06");
    assert_eq!(body["error_category"], "user_fixable");
    assert_eq!(body["error_message"], pa06.english);
    assert_eq!(body["error"], pa06.english, "every error keeps the shared `error` field");
    assert_eq!(body["reference"], "INV-1");
    assert_eq!(body["swish_ref"].as_str().map(str::len), Some(32));

    let snapshot = state.store.snapshot(PAYER, "INV-1").await.expect("snapshot").expect("row");
    assert_eq!(snapshot.status.as_deref(), Some("DECLINED"));
    assert_eq!(snapshot.swish_ref.as_deref(), body["swish_ref"].as_str());

    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert_eq!(
        state.store.status_view(PAYER, "INV-1").await.expect("store").and_then(|v| v.error_code).as_deref(),
        Some("PA06"),
        "the status endpoint explains it later from the same code"
    );

    // Nothing exists at Swish, so there is nothing for the sweep to ask about.
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    let schedule = swisha::domain::payout::CheckSchedule {
        first_after: std::time::Duration::from_millis(10),
        every: std::time::Duration::from_secs(3600),
        window: std::time::Duration::from_secs(3600),
    };
    let swept = state.store.claim_due_checks(PAYER, &schedule, 100).await.expect("sweep");
    assert!(swept.iter().all(|p| p.reference != "INV-1"), "a declined payout is never checked");

    let (status, body) = post(&base, valid()).await;
    assert_eq!(status, 409, "the reference is still spent");
    assert!(body["error"].as_str().unwrap_or_default().contains("declined"), "{body}");
}

// What a refusal proves decides the status: only a documented one without RP09 proves Swish stored
// nothing. Anything else is PENDING, never "failed", and whose problem it is decides the HTTP code.
#[tokio::test]
async fn each_kind_of_swish_refusal_gets_its_own_answer() {
    let rp09 = r#"[{"errorCode":"RP09","errorMessage":"instructionUUID is not available"}]"#;
    for (swish, swish_body, http, status, code) in [
        (401, "", 502, "DECLINED", "401"),
        (403, r#"[{"errorCode":"PA01"}]"#, 502, "DECLINED", "PA01"),
        (429, "Too Many Requests", 502, "DECLINED", "429"),
        (422, rp09, 422, "PENDING", "RP09"),
        (404, "<html>not here</html>", 502, "PENDING", "404"),
        (500, "upstream exploded", 503, "PENDING", "500"),
    ] {
        let (base, state) = app_with(MockSwish::new().rejects(swish, swish_body)).await;
        let (got, body) = post(&base, valid()).await;
        assert_eq!(got, http, "Swish {swish} should answer {http}: {body}");
        assert_eq!(body["status"], status, "Swish {swish}: {body}");
        let error = body["error"].as_str().unwrap_or_default();
        if status == "DECLINED" {
            assert_eq!(body["error_code"], code, "Swish {swish}: {body}");
            assert!(!error.is_empty(), "{body}");
        } else {
            assert!(body["error_code"].is_null(), "no answer has no failure to explain: {body}");
            assert!(error.contains(code) && error.contains("spent"), "Swish {swish}: {body}");
        }

        let stored = state.store.snapshot(PAYER, "INV-1").await.expect("snapshot").expect("row");
        assert_eq!(stored.status.as_deref(), Some(status), "Swish {swish}");
    }
}

// Swish refuses a payout without the recipient's personnummer, so swisha refuses it first, with
// 400 and nothing stored or sent. Absent, null, empty and blank all count as missing.
#[tokio::test]
async fn a_payout_without_an_ssn_is_refused_before_anything_is_sent() {
    let mock = MockSwish::new().accepts().resolves_to("PAID");
    let (base, state) = app_with(mock.clone()).await;

    for (reference, missing) in [
        ("SSN-1", json!({ "reference": "SSN-1", "payee_alias": PHONE, "amount": 100.0 })),
        ("SSN-2", json!({ "reference": "SSN-2", "payee_alias": PHONE, "amount": 100.0, "payee_ssn": null })),
        ("SSN-3", json!({ "reference": "SSN-3", "payee_alias": PHONE, "amount": 100.0, "payee_ssn": "" })),
        ("SSN-4", json!({ "reference": "SSN-4", "payee_alias": PHONE, "amount": 100.0, "payee_ssn": "   " })),
    ] {
        let (status, body) = post(&base, missing.clone()).await;
        assert_eq!(status, 400, "should refuse {missing}");
        assert!(
            body["error"].as_str().unwrap_or_default().contains("payee_ssn is required"),
            "the error must say why: {body}"
        );
        let stored = state.store.snapshot(PAYER, reference).await.expect("store");
        assert!(stored.is_none(), "{reference} spent a reference on a payout Swish would refuse");
    }

    // Nothing reached Swish. The guard has to stop the payout, not merely complain about it.
    assert_eq!(mock.posted().len(), 0, "a refused payout must never be submitted");
}

// Swish shows 50 characters. A longer message is cut once, before it is stored, so the record and
// the payout never disagree and the insert never overflows.
#[tokio::test]
async fn the_message_stored_is_the_message_sent() {
    let swish = MockSwish::new().accepts().resolves_to("PAID");
    let (base, state) = app_with(swish.clone()).await;
    for (i, length) in [51usize, 256, 1000].into_iter().enumerate() {
        let reference = format!("MSG-{i}");
        let message: String = "åäö".chars().cycle().take(length).collect();
        let (code, body) = post(&base, json!({
            "reference": reference, "payee_alias": PHONE, "payee_ssn": SSN, "amount": 100.0, "message": message,
        }))
        .await;
        assert_eq!(code, 202, "{length} characters: {body}");

        let sent = common::payload(&swish.posted()[i])["message"].as_str().expect("message").to_string();
        let stored: String = sqlx::query_scalar(AssertSqlSafe(format!(
            "SELECT message FROM {} WHERE payer = $1 AND reference = $2",
            state.config.table_payouts
        )))
        .bind(PAYER)
        .bind(&reference)
        .fetch_one(state.store.pool())
        .await
        .expect("stored");
        assert_eq!(sent, message.chars().take(50).collect::<String>());
        assert_eq!(stored, sent, "{length} characters");
    }
}

// A failure the poll finds, with no callback, keeps Swish's code, so the caller reads what Swish
// said rather than "The Swish payout failed.".
#[tokio::test]
async fn a_failure_the_poll_finds_keeps_swishs_error_code() {
    let swish = MockSwish::new()
        .accepts()
        .resolves_with(json!({ "status": "ERROR", "errorCode": "RF07", "errorMessage": "Transaction declined" }));
    let (base, _) = app_with(swish).await;
    assert_eq!(post(&base, valid()).await.0, 202);
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;

    let answer: Value = reqwest::Client::new()
        .get(format!("{base}/swish/status/INV-1"))
        .header("x-api-secret", SECRET)
        .header(common::KEY_HEADER, common::key(common::PAYER))
        .send()
        .await
        .expect("request")
        .json()
        .await
        .expect("json");
    assert_eq!(answer["status"], "ERROR");
    assert_eq!(answer["error_code"], "RF07", "{answer}");
    assert_ne!(answer["error_message"], "The Swish payout failed.", "{answer}");
}

// A character Swish refuses would come back as RP02 and spend the reference. Refused here first:
// nothing stored, nothing sent, and the same reference pays once the message is fixed.
#[tokio::test]
async fn a_message_swish_would_refuse_is_refused_before_the_claim() {
    let swish = MockSwish::new().accepts().resolves_to("PAID");
    let (base, state) = app_with(swish.clone()).await;
    for message in ["Faktura #1042", "Tack 😀", "Café"] {
        let (code, body) = post(&base, with("message", json!(message))).await;
        assert_eq!(code, 400, "{message}: {body}");
        assert!(body["error"].as_str().unwrap_or_default().contains("!?(),.-:;"), "{body}");
    }
    assert!(swish.posted().is_empty());
    assert!(state.store.snapshot(PAYER, "INV-1").await.expect("store").is_none(), "nothing stored");
    assert_eq!(post(&base, with("message", json!("Faktura 1042"))).await.0, 202, "the reference is still free");
}

// A certificate that expired while swisha runs cannot authenticate or sign. Refused before the
// claim, so the reference is still usable once the certificate is renewed.
#[tokio::test]
async fn an_expired_certificate_is_refused_before_the_claim() {
    for which in ["tls", "signing"] {
        let swish = MockSwish::new().accepts().resolves_to("PAID");
        let mut config = common::config();
        config.swish_base_url = swish.clone().start().await;
        let expired = swisha::util::time::unix_now() - 60;
        let mut payer = common::payer(PAYER, common::PAYER_NUMBER);
        match which {
            "tls" => payer.tls.expires = expired,
            _ => payer.signing.expires = expired,
        }
        let (state, pool) = common::state_and_pool_with(config.clone(), vec![payer]).await;
        let base = common::serve(swisha::http::internal_router().with_state(state.clone())).await;

        let (code, body) = post(&base, valid()).await;
        assert_eq!(code, 503, "{which}: {body}");
        let error = body["error"].as_str().unwrap_or_default().to_string();
        assert!(error.to_lowercase().contains(which) && error.contains("expired"), "{which}: {error}");
        assert!(swish.posted().is_empty(), "{which}: nothing sent");
        assert!(state.store.snapshot(PAYER, "INV-1").await.expect("store").is_none(), "{which}: nothing stored");

        // Renewed: the same reference pays.
        let renewed = common::payer_state(&common::payer(PAYER, common::PAYER_NUMBER));
        let state = common::state_over(pool, config, vec![renewed]).await;
        let base = common::serve(swisha::http::internal_router().with_state(state)).await;
        assert_eq!(post(&base, valid()).await.0, 202, "{which}: the reference is still free");
    }
}

// A personnummer whose date does not exist would come back from Swish as PA06 and spend the
// reference. Refused here first, nothing stored.
#[tokio::test]
async fn a_personnummer_with_a_date_that_does_not_exist_is_refused_first() {
    let swish = MockSwish::new().accepts().resolves_to("PAID");
    let (base, state) = app_with(swish.clone()).await;
    for ssn in ["196413233237", "196408323233", "196408833231"] {
        let (code, body) = post(&base, with("payee_ssn", json!(ssn))).await;
        assert_eq!(code, 400, "{ssn}: {body}");
    }
    assert!(swish.posted().is_empty());
    assert!(state.store.snapshot(PAYER, "INV-1").await.expect("store").is_none(), "nothing stored");
}

// When Swish cannot be reached the payout may have reached it, so the reference is spent. The
// answer says so in the status endpoint's shape, as PENDING, never "failed".
#[tokio::test]
async fn an_unreachable_swish_answers_with_the_status_it_left() {
    let mut config = common::config();
    config.swish_base_url = "http://127.0.0.1:1".into();
    let state = common::state_with(config).await;
    let base = common::serve(swisha::http::internal_router().with_state(state.clone())).await;

    let (code, body) = post(&base, valid()).await;
    assert_eq!(code, 503, "{body}");
    assert_eq!(body["status"], "PENDING", "{body}");
    assert!(body["swish_ref"].as_str().is_some_and(|r| r.len() == 32), "{body}");
    let error = body["error"].as_str().unwrap_or_default();
    assert!(error.starts_with("Swish may have received this payout"), "{body}");
    assert!(body["error_message"].is_null(), "no answer has no failure to explain: {body}");
    assert_eq!(post(&base, valid()).await.0, 409, "the reference is spent");
}

// Without a callback the follow-up keeps asking past DEBITED, to PAID or, when the recipient's bank
// refused the money, ERROR. Both steps are announced, DEBITED first.
#[tokio::test]
async fn a_debited_payout_is_followed_to_swishs_last_word_without_a_callback() {
    for last in ["PAID", "ERROR"] {
        let mock = MockSwish::new().accepts().resolves_through(&["DEBITED", "DEBITED", last]);
        let (base, state) = app_with(mock.clone()).await;
        let mut announced = state.stream.subscribe(PAYER).expect("room for a stream");
        assert_eq!(post(&base, valid()).await.0, 202);
        tokio::time::sleep(std::time::Duration::from_secs(4)).await;

        let stored = state.store.snapshot(PAYER, "INV-1").await.expect("store").expect("row");
        assert_eq!(stored.status.as_deref(), Some(last));
        let mut statuses = Vec::new();
        while let Ok(event) = announced.try_recv() {
            if event.channel.ends_with(":updates") {
                let payload: Value = serde_json::from_str(&event.payload).expect("json");
                statuses.push(payload["status"].as_str().unwrap_or_default().to_string());
            }
        }
        assert_eq!(statuses, ["DEBITED", last]);
        assert_eq!(mock.posted().len(), 1, "following up never sends anything");
    }
}
