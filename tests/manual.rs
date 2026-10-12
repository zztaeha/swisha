#![cfg(feature = "http")]

// Confirm paid and redo, what a person does once swisha stops asking or Swish reports a failure.
// Both are guards as much as buttons: a redo never follows money that left, and happens once.

mod common;

use common::{MockSwish, PAYER, PHONE, SECRET, SSN};
use serde_json::{Value, json};
use swisha::state::SharedState;
use swisha::store::PayoutStore;

struct Rig {
    base: String,
    state: SharedState,
    swish: MockSwish,
}

// Swish accepts every send and, asked about one, has no record of it, so nothing moves unless a
// test moves it.
async fn rig() -> Rig {
    rig_with(MockSwish::new().accepts()).await
}

async fn rig_with(swish: MockSwish) -> Rig {
    let mut config = common::config();
    config.swish_base_url = swish.clone().start().await;
    let state = common::state_with(config).await;
    let base = common::serve(swisha::http::internal_router().with_state(state.clone())).await;
    Rig { base, state, swish }
}

// What the send recorded when it ended without Swish taking the payout: `PENDING` when Swish gave no
// answer, `DECLINED` when it refused.
async fn send_ended(r: &Rig, reference: &str, status: &str) {
    let ended = swisha::domain::payout::EventRecord { actor: None, note: None, ..event(reference, status, status) };
    r.state.store.record_event(&ended).await.expect("event");
}

async fn post(r: &Rig, path: &str, body: Value) -> (u16, Value) {
    let response = reqwest::Client::new()
        .post(format!("{}{path}", r.base))
        .header("x-api-secret", SECRET)
        .header(common::KEY_HEADER, common::key(common::PAYER))
        .json(&body)
        .send()
        .await
        .expect("request");
    let code = response.status().as_u16();
    let text = response.text().await.expect("body");
    (code, serde_json::from_str(&text).unwrap_or(Value::String(text)))
}

// A payout sent and accepted, then moved to `status` as Swish or the sweep would have.
async fn paid_out(r: &Rig, reference: &str, status: &str) -> String {
    let body = json!({ "reference": reference, "payee_alias": PHONE, "payee_ssn": SSN, "amount": 250.5, "message": "Kvitto 7" });
    assert_eq!(post(r, "/swish/payout", body).await.0, 202);
    if status != "CREATED" {
        assert!(common::stage(&r.state, PAYER, reference, status).await, "staged {status}");
    }
    reference.to_string()
}

// Moves a payout's creation back. Only end_checks ends the checks, so age alone ends nothing.
async fn age(r: &Rig, reference: &str, hours: i32) {
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE {} SET created_at = NOW() - make_interval(hours => $1) WHERE reference = $2",
        r.state.config.table_payouts
    )))
    .bind(hours)
    .bind(reference)
    .execute(r.state.store.pool())
    .await
    .expect("age");
}

async fn view(r: &Rig, reference: &str) -> Value {
    reqwest::Client::new()
        .get(format!("{}/swish/status/{PAYER}/{reference}", r.base))
        .header("x-api-secret", SECRET)
        .header(common::KEY_HEADER, common::key(common::PAYER))
        .send()
        .await
        .expect("request")
        .json()
        .await
        .expect("json")
}

async fn events(r: &Rig, reference: &str) -> Vec<(String, Option<String>, Option<String>)> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT event, actor, note FROM {} WHERE reference = $1 ORDER BY id",
        r.state.config.table_events
    )))
    .bind(reference)
    .fetch_all(r.state.store.pool())
    .await
    .expect("events")
}

async fn uuid(r: &Rig, reference: &str) -> String {
    r.state.store.snapshot(PAYER, reference).await.expect("store").and_then(|s| s.swish_ref).expect("uuid")
}

fn event(reference: &str, event: &str, status: &str) -> swisha::domain::payout::EventRecord {
    swisha::domain::payout::EventRecord {
        payer: PAYER.into(),
        reference: reference.into(),
        swish_ref: None,
        event: event.into(),
        status: Some(status.into()),
        amount: None,
        payee_alias: None,
        error_code: None,
        error_message: None,
        ip: None,
        actor: Some("Anna".into()),
        note: Some("bank".into()),
    }
}

// The last check recorded with the payout still at `status`, as the checks end it.
async fn end_checks(r: &Rig, reference: &str, status: &str) {
    let ended = event(reference, "CHECKS_ENDED", status);
    assert!(r.state.store.end_checks(status, &ended).await.expect("store"), "the checks end");
}

fn confirm(reference: &str) -> String {
    format!("/swish/payout/{PAYER}/{reference}/confirm")
}

fn redo(reference: &str) -> String {
    format!("/swish/payout/{PAYER}/{reference}/redo")
}

fn anna() -> Value {
    json!({ "by": "Anna", "note": "Bank statement shows it arrived" })
}

// Confirm paid

#[tokio::test]
async fn a_payout_swisha_stopped_asking_about_is_confirmed_with_who_and_why() {
    let r = rig().await;
    paid_out(&r, "C-1", "NEEDS_REVIEW").await;
    let mut announced = r.state.stream.subscribe(PAYER).expect("room for a stream");

    let (code, body) = post(&r, &confirm("C-1"), anna()).await;
    assert_eq!(code, 200, "{body}");
    assert_eq!(body["status"], "PAID");
    assert_eq!(body["manual_confirmation"]["by"], "Anna");
    assert_eq!(body["manual_confirmation"]["note"], "Bank statement shows it arrived");
    assert!(body["manual_confirmation"]["at"].as_str().is_some_and(|at| at.contains('T')), "{body}");
    assert_eq!(body["actions"], json!([]), "nothing is left to do");
    assert!(body["review"].is_null());

    assert_eq!(view(&r, "C-1").await, body, "the status endpoint says the same");
    let trail = events(&r, "C-1").await;
    assert!(
        trail.contains(&("CONFIRMED_PAID".into(), Some("Anna".into()), Some("Bank statement shows it arrived".into()))),
        "{trail:?}"
    );
    let (mut updates, mut trail) = (Vec::new(), Vec::new());
    while let Ok(event) = announced.try_recv() {
        let payload = serde_json::from_str::<Value>(&event.payload).expect("json");
        if event.channel.ends_with(":updates") {
            updates.push(payload);
        } else if event.channel.ends_with(":events") {
            trail.push(payload);
        }
    }
    // `updates` is the channel to show a browser, so who confirmed and why stay on `events`.
    let mut shown = body.clone();
    shown["manual_confirmation"] = json!({ "at": body["manual_confirmation"]["at"] });
    assert_eq!(updates.last(), Some(&shown), "the stream carries the endpoint's shape, but for who and why");
    assert!(
        trail.iter().any(|e| e["actor"] == "Anna" && e["note"] == "Bank statement shows it arrived"),
        "{trail:?}"
    );
}

// Until the checks end Swish may still give its last word, so a person waits for it.
#[tokio::test]
async fn debited_or_error_is_confirmed_only_once_the_checks_end() {
    let r = rig().await;
    for (reference, status) in [("C-2", "DEBITED"), ("C-3", "ERROR")] {
        paid_out(&r, reference, status).await;
        let (code, body) = post(&r, &confirm(reference), anna()).await;
        assert_eq!(code, 409, "{status}: {body}");
        assert!(body["error"].as_str().unwrap_or_default().contains("48 hours"), "{body}");
        assert_eq!(view(&r, reference).await["actions"].as_array().map(|a| a.contains(&json!("confirm_paid"))), Some(false));

        // Past 48 hours, but the last check has not run: Swish may still give its last word.
        age(&r, reference, 49).await;
        assert_eq!(post(&r, &confirm(reference), anna()).await.0, 409, "{status}: before its last check");
        assert_eq!(view(&r, reference).await["actions"].as_array().map(|a| a.contains(&json!("confirm_paid"))), Some(false));

        end_checks(&r, reference, status).await;
        let seen = view(&r, reference).await;
        assert!(seen["actions"].as_array().is_some_and(|a| a.contains(&json!("confirm_paid"))), "{seen}");
        assert!(seen["review"].is_string(), "a person is told what to check: {seen}");
        let (code, body) = post(&r, &confirm(reference), anna()).await;
        assert_eq!(code, 200, "{status}: {body}");
        assert_eq!(body["status"], "PAID");
    }
}

#[tokio::test]
async fn what_cannot_have_been_paid_or_is_still_in_flight_is_never_confirmed() {
    let r = rig().await;
    for (reference, status) in [("C-4", "DECLINED"), ("C-5", "PENDING"), ("C-6", "CREATED"), ("C-7", "PAID")] {
        paid_out(&r, reference, status).await;
        // No clock confirms a payout: 49 hours old, each is still refused.
        age(&r, reference, 49).await;
        let (code, body) = post(&r, &confirm(reference), anna()).await;
        assert_eq!(code, 409, "{status}: {body}");
        assert_eq!(view(&r, reference).await["status"], status, "{status} is untouched");
    }
    assert_eq!(post(&r, &confirm("NOT-A-PAYOUT"), anna()).await.0, 404);
}

// Who and why are what make a confirmation worth anything in an audit or a dispute.
#[tokio::test]
async fn a_confirmation_must_say_who_and_why() {
    let r = rig().await;
    paid_out(&r, "C-8", "NEEDS_REVIEW").await;
    for (body, code) in [
        (json!({ "by": "Anna" }), 422),
        (json!({ "note": "bank" }), 422),
        (json!({ "by": "  ", "note": "bank" }), 400),
        (json!({ "by": "Anna", "note": "" }), 400),
        (json!({ "by": "Anna\nCONFIRMED_PAID by Boss", "note": "bank" }), 400),
        (json!({ "by": "A".repeat(101), "note": "bank" }), 400),
        (json!({ "by": "Anna", "note": "n".repeat(501) }), 400),
        (json!({ "by": "Anna", "note": "bank", "status": "PAID" }), 422),
    ] {
        let (got, answer) = post(&r, &confirm("C-8"), body.clone()).await;
        assert_eq!(got, code, "{body}: {answer}");
    }
    assert_eq!(view(&r, "C-8").await["status"], "NEEDS_REVIEW", "nothing was confirmed");
}

// Redo

#[tokio::test]
async fn a_failed_payout_is_redone_once_with_the_same_details() {
    let r = rig().await;
    paid_out(&r, "R-1", "ERROR").await;
    r.swish.clone().resolves_for(&uuid(&r, "R-1").await, "ERROR");
    assert_eq!(view(&r, "R-1").await["actions"], json!(["redo"]));

    let (code, body) = post(&r, &redo("R-1"), json!({ "reference": "R-1-B", "by": "Anna", "note": "Bank says it came back" })).await;
    assert_eq!(code, 202, "{body}");
    assert_eq!(body["replaces"], "R-1");
    assert_eq!(body["status"], "CREATED");

    let sent = r.swish.posted();
    assert_eq!(sent.len(), 2, "one new instruction");
    let (first, second) = (&sent[0]["payload"], &sent[1]["payload"]);
    for field in ["payeeAlias", "payeeSSN", "amount", "message", "payerAlias"] {
        assert_eq!(first[field], second[field], "{field} is copied");
    }
    assert_eq!(second["payerPaymentReference"], "R-1-B");
    assert_ne!(first["payoutInstructionUUID"], second["payoutInstructionUUID"], "a new instruction");

    let original = view(&r, "R-1").await;
    assert_eq!(original["replaced_by"], "R-1-B");
    assert_eq!(original["actions"], json!([]), "it cannot be redone again");
    assert_eq!(view(&r, "R-1-B").await["replaces"], "R-1");
    assert!(events(&r, "R-1").await.contains(&("REDONE".into(), Some("Anna".into()), Some("Bank says it came back".into()))));

    let (code, body) = post(&r, &redo("R-1"), json!({ "reference": "R-1-C", "by": "Bo" })).await;
    assert_eq!(code, 409, "{body}");
    assert!(body["error"].as_str().unwrap_or_default().contains("R-1-B"), "it names the redo: {body}");
    assert_eq!(r.swish.posted().len(), 2, "no third instruction");
}

// Two staff members press redo on the same payout at once. One payout goes out.
#[tokio::test]
async fn simultaneous_redos_send_one_payout() {
    let r = rig().await;
    staged(&r, "R-2", "DECLINED").await;
    send_ended(&r, "R-2", "DECLINED").await;
    let mut presses = Vec::new();
    for i in 0..8 {
        let (url, body) = (format!("{}{}", r.base, redo("R-2")), json!({ "reference": format!("R-2-{i}"), "by": "Staff" }));
        presses.push(tokio::spawn(async move {
            reqwest::Client::new()
                .post(url)
                .header("x-api-secret", SECRET)
                .header(common::KEY_HEADER, common::key(common::PAYER))
                .json(&body)
                .send()
                .await
                .expect("request")
                .status()
                .as_u16()
        }));
    }
    let mut codes = Vec::new();
    for press in presses {
        codes.push(press.await.expect("join"));
    }
    codes.sort_unstable();
    assert_eq!(codes, [202, 409, 409, 409, 409, 409, 409, 409], "the others are told it was redone");
    assert_eq!(r.swish.posted().len(), 1, "one redo");
}

// The money left, or Swish may still pay it: a redo would be the second payment. Here Swish's
// answer lets each redo through to the store, so the refusal is the store's own.
#[tokio::test]
async fn a_payout_whose_money_left_or_is_in_flight_is_never_redone() {
    let r = rig().await;
    let cases = [
        ("R-3", "DEBITED", Some("DECLINED"), "money for this payout has left"),
        ("R-4", "PAID", Some("ERROR"), "was paid"),
        ("R-5", "PENDING", None, "may still pay it"),
        ("R-6", "CREATED", None, "may still pay it"),
    ];
    for (reference, status, swish_says, refused) in cases {
        let uuid = staged(&r, reference, status).await;
        if let Some(said) = swish_says {
            r.swish.clone().resolves_for(&uuid, said);
        }
        let (code, body) = post(&r, &redo(reference), json!({ "reference": format!("{reference}-B"), "by": "Anna" })).await;
        assert_eq!(code, 409, "{status}: {body}");
        assert!(body["error"].as_str().unwrap_or_default().contains(refused), "{status}: {body}");
        assert!(r.state.store.snapshot(PAYER, &format!("{reference}-B")).await.expect("store").is_none());
        assert_eq!(view(&r, reference).await["status"], status, "{status} stands");
    }
    assert!(r.swish.posted().is_empty(), "nothing was sent");
}

#[tokio::test]
async fn a_redo_needs_a_new_valid_reference_and_a_name() {
    let r = rig().await;
    paid_out(&r, "R-7", "ERROR").await;
    paid_out(&r, "R-8", "ERROR").await;
    r.swish.clone().resolves_for(&uuid(&r, "R-7").await, "ERROR");
    for (body, code) in [
        (json!({ "reference": "R-7", "by": "Anna" }), 400),
        (json!({ "reference": "R 7", "by": "Anna" }), 400),
        (json!({ "reference": "R-7-B", "by": "" }), 400),
        (json!({ "reference": "R-7-B" }), 422),
        (json!({ "reference": "R-8", "by": "Anna" }), 409),
    ] {
        let (got, answer) = post(&r, &redo("R-7"), body.clone()).await;
        assert_eq!(got, code, "{body}: {answer}");
    }
    let (code, _) = post(&r, "/swish/payout/atlantis/R-7/redo", json!({ "reference": "R-7-B", "by": "Anna" })).await;
    assert_eq!(code, 403, "another number than the key's, whether it exists or not");
    assert_eq!(post(&r, &redo("NOT-A-PAYOUT"), json!({ "reference": "N-B", "by": "Anna" })).await.0, 404);
    assert_eq!(view(&r, "R-7").await["actions"], json!(["redo"]), "every refusal left it redoable");
    assert_eq!(r.swish.posted().len(), 2);
}

// Swish is asked first

// A payout staged in the store at `status` under its own instruction, never sent, so no follow-up
// asks Swish about it: only the redo does. Returns the instruction.
async fn staged(r: &Rig, reference: &str, status: &str) -> String {
    let uuid = swisha::swish::random_payout_uuid();
    let new = swisha::domain::payout::NewPayout {
        payer: PAYER,
        reference,
        payer_alias: common::PAYER_NUMBER,
        payee_alias: "46701234567",
        payee_ssn: Some(SSN),
        amount: 250.5,
        message: "Kvitto 7",
        swish_ref: &uuid,
        callback_identifier: None,
    };
    r.state.store.claim(&new).await.expect("claim");
    if status != "CREATED" {
        assert!(common::stage(&r.state, PAYER, reference, status).await);
    }
    uuid
}

// Before a payout is paid again, Swish is asked, and what it says is recorded. Only its word that
// it failed lets the redo through, or its having no record of a send that ended without its 201.
#[tokio::test]
async fn a_redo_goes_out_only_when_swish_says_nothing_was_paid() {
    let r = rig().await;
    // "no record" is Swish's 400 naming PA01. A bare 404 or 400 is no answer: Swish documents
    // neither for a payout it does not know.
    for (reference, stored, swish, code) in [
        ("S-1", "ERROR", "ERROR", 202),
        ("S-2", "NEEDS_REVIEW", "no record of a send left unanswered", 202),
        ("S-3", "DECLINED", "no record of a send it declined", 202),
        ("S-20", "NEEDS_REVIEW", "no record", 409),
        ("S-4", "ERROR", "PAID", 409),
        ("S-5", "NEEDS_REVIEW", "DEBITED", 409),
        ("S-6", "NEEDS_REVIEW", "CREATED", 409),
        ("S-13", "ERROR", "no record", 409),
        ("S-14", "NEEDS_REVIEW", "404", 503),
        ("S-15", "NEEDS_REVIEW", "400", 503),
    ] {
        let uuid = staged(&r, reference, stored).await;
        match swish {
            "no record of a send left unanswered" => send_ended(&r, reference, "PENDING").await,
            "no record of a send it declined" => send_ended(&r, reference, "DECLINED").await,
            "no record" => {}
            "404" | "400" => drop(r.swish.clone().answers_for(&uuid, swish.parse().expect("code"))),
            answer => drop(r.swish.clone().resolves_for(&uuid, answer)),
        }
        let sent = r.swish.posted().len();
        let new = format!("{reference}-B");
        let (got, body) = post(&r, &redo(reference), json!({ "reference": new, "by": "Anna" })).await;
        assert_eq!(got, code, "{stored}, Swish says {swish}: {body}");
        assert_eq!(r.swish.posted().len() - sent, usize::from(code == 202), "{reference}");
        if code != 202 {
            assert!(r.state.store.snapshot(PAYER, &new).await.expect("store").is_none(), "nothing claimed");
        }
    }
    for (reference, status) in [("S-4", "PAID"), ("S-5", "DEBITED")] {
        assert_eq!(view(&r, reference).await["status"], status, "Swish's word is recorded");
    }
    let (_, body) = post(&r, &redo("S-4"), json!({ "reference": "S-4-C", "by": "Anna" })).await;
    assert!(body["error"].as_str().unwrap_or_default().contains("PAID"), "{body}");
}

// Swish could not be asked, so nothing is known: nothing is redone, and the redo can be pressed
// again once Swish answers.
#[tokio::test]
async fn a_redo_swish_cannot_be_asked_about_is_not_sent() {
    let r = rig().await;
    let uuid = staged(&r, "S-7", "ERROR").await;
    r.swish.clone().answers_for(&uuid, 429);
    let (code, body) = post(&r, &redo("S-7"), json!({ "reference": "S-7-B", "by": "Anna" })).await;
    assert_eq!(code, 503, "{body}");
    assert!(body["error"].as_str().unwrap_or_default().contains("nothing was redone"), "{body}");
    assert!(r.state.store.snapshot(PAYER, "S-7-B").await.expect("store").is_none());
    assert!(r.swish.posted().is_empty());

    r.swish.clone().resolves_for(&uuid, "ERROR");
    assert_eq!(post(&r, &redo("S-7"), json!({ "reference": "S-7-B", "by": "Anna" })).await.0, 202);
}

// A redo that failed in turn is redone only if nothing before it was paid. Swish is asked about
// every payout in the chain, so a late PAID on the first stops the third.
#[tokio::test]
async fn a_redo_of_a_redo_asks_swish_about_every_payout_before_it() {
    let r = rig().await;
    let first = staged(&r, "S-8A", "ERROR").await;
    r.swish.clone().resolves_for(&first, "ERROR");
    assert_eq!(post(&r, &redo("S-8A"), json!({ "reference": "S-8B", "by": "Anna" })).await.0, 202);
    assert!(common::stage(&r.state, PAYER, "S-8B", "ERROR").await);
    r.swish.clone().resolves_for(&uuid(&r, "S-8B").await, "ERROR");
    r.swish.clone().resolves_for(&first, "PAID");

    let (code, body) = post(&r, &redo("S-8B"), json!({ "reference": "S-8C", "by": "Anna" })).await;
    assert_eq!(code, 409, "{body}");
    assert!(body["error"].as_str().unwrap_or_default().contains("S-8A"), "it names the payout paid: {body}");
    assert!(r.state.store.snapshot(PAYER, "S-8C").await.expect("store").is_none());
    assert_eq!(view(&r, "S-8A").await["status"], "PAID");
    assert_eq!(r.swish.posted().len(), 1, "the first redo only");
}

// Swish took the instruction, and later says it has no record of it. That contradicts what it
// said, so it is no proof the payout was never sent, and nothing is paid again.
#[tokio::test]
async fn a_payout_swish_accepted_is_never_redone_on_its_saying_it_has_no_record() {
    let r = rig().await;
    paid_out(&r, "S-16", "CREATED").await;
    assert!(events(&r, "S-16").await.iter().any(|(event, _, _)| event == "ACCEPTED"), "Swish's 201 is on the trail");
    assert!(common::stage(&r.state, PAYER, "S-16", "NEEDS_REVIEW").await);

    let (code, body) = post(&r, &redo("S-16"), json!({ "reference": "S-16-B", "by": "Anna" })).await;
    assert_eq!(code, 409, "{body}");
    assert_eq!(r.swish.posted().len(), 1, "the original only");
}

// A payout Swish accepted, whose record of the 201 is lost. A failed write of it is tried again,
// so only a stop between Swish's answer and the write loses it.
async fn sent_with_the_201_lost(r: &Rig, reference: &str) {
    paid_out(r, reference, "CREATED").await;
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "DELETE FROM {} WHERE reference = $1 AND event = 'ACCEPTED'",
        r.state.config.table_events
    )))
    .bind(reference)
    .execute(r.state.store.pool())
    .await
    .expect("lose the 201");
}

// The same 201, its record lost. The trail then holds no way the send ended either, so Swish's
// "no record" proves nothing, and nothing is redone.
#[tokio::test]
async fn a_payout_whose_send_has_no_recorded_ending_is_never_redone_on_its_saying_it_has_no_record() {
    let r = rig().await;
    sent_with_the_201_lost(&r, "S-21").await;
    assert!(!events(&r, "S-21").await.iter().any(|(event, _, _)| event == "ACCEPTED"), "the 201 is lost");
    assert!(common::stage(&r.state, PAYER, "S-21", "NEEDS_REVIEW").await);

    let (code, body) = post(&r, &redo("S-21"), json!({ "reference": "S-21-B", "by": "Anna" })).await;
    assert_eq!(code, 409, "{body}");
    assert!(body["error"].as_str().unwrap_or_default().contains("new payout"), "{body}");
    assert_eq!(r.swish.posted().len(), 1, "the original only");
}

// The 201's record lost, and the follow-up then hears Swish holding it without an outcome. That is
// recorded as Swish holding it, never as a PENDING, which would read as a send that ended.
#[tokio::test]
async fn a_payout_the_follow_up_found_held_is_never_redone_on_its_saying_it_has_no_record() {
    let r = rig_with(MockSwish::new().accepts().resolves_to("CREATED")).await;
    sent_with_the_201_lost(&r, "S-23").await;

    let since = std::time::Instant::now();
    while !events(&r, "S-23").await.iter().any(|(event, _, _)| event == "ACCEPTED") {
        assert!(since.elapsed() < std::time::Duration::from_secs(25), "the follow-up never recorded it");
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    assert!(!events(&r, "S-23").await.iter().any(|(event, _, _)| event == "PENDING"));
    assert!(common::stage(&r.state, PAYER, "S-23", "NEEDS_REVIEW").await);
    r.swish.clone().forgets(&uuid(&r, "S-23").await);

    let (code, body) = post(&r, &redo("S-23"), json!({ "reference": "S-23-B", "by": "Anna" })).await;
    assert_eq!(code, 409, "{body}");
    assert!(body["error"].as_str().unwrap_or_default().contains("showed it held it"), "{body}");
    assert_eq!(r.swish.posted().len(), 1, "the original only");
}

// RP09 at the send is Swish saying an instruction already holds the UUID: it holds this one, so
// its saying later that it has no record of it is the same contradiction.
#[tokio::test]
async fn a_payout_swish_answered_rp09_is_never_redone_on_its_saying_it_has_no_record() {
    let rp09 = r#"[{"errorCode":"RP09","errorMessage":"The given instructionUUID is not available"}]"#;
    let r = rig_with(MockSwish::new().rejects(422, rp09)).await;
    let body = json!({ "reference": "S-22", "payee_alias": PHONE, "payee_ssn": SSN, "amount": 250.5 });
    let (_, answer) = post(&r, "/swish/payout", body).await;
    assert_eq!(answer["status"], "PENDING", "{answer}");
    assert!(common::stage(&r.state, PAYER, "S-22", "NEEDS_REVIEW").await);

    let (code, body) = post(&r, &redo("S-22"), json!({ "reference": "S-22-B", "by": "Anna" })).await;
    assert_eq!(code, 409, "{body}");
    assert_eq!(r.swish.posted().len(), 1, "the original only");
    assert!(events(&r, "S-22").await.iter().any(|(event, _, _)| event == "ACCEPTED"), "RP09 is on the trail");
}

// A redo goes out once it is claimed, even when the person who asked gives up waiting: one left
// claimed and never sent could not be paid again until its checks ended, two days on.
#[tokio::test]
async fn a_redo_whose_caller_leaves_is_still_sent() {
    let r = rig().await;
    let first = staged(&r, "H-1", "ERROR").await;
    r.swish.clone().resolves_for(&first, "ERROR");
    // The redo's first trail row takes two seconds to write, so its caller leaves after the claim.
    let table = r.state.config.table_events.clone();
    for statement in [
        format!(
            "CREATE FUNCTION {table}_slow() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN \
             IF NEW.event = 'INITIATED' THEN PERFORM pg_sleep(2); END IF; RETURN NEW; END $$"
        ),
        format!("CREATE TRIGGER slow BEFORE INSERT ON {table} FOR EACH ROW EXECUTE FUNCTION {table}_slow()"),
    ] {
        sqlx::query(sqlx::AssertSqlSafe(statement)).execute(r.state.store.pool()).await.expect("slow trail");
    }

    let left = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(1))
        .build()
        .expect("client")
        .post(format!("{}{}", r.base, redo("H-1")))
        .header("x-api-secret", SECRET)
        .header(common::KEY_HEADER, common::key(common::PAYER))
        .json(&json!({ "reference": "H-1-B", "by": "Anna" }))
        .send()
        .await;
    assert!(left.is_err(), "the caller left before the answer");

    let since = std::time::Instant::now();
    while !events(&r, "H-1-B").await.iter().any(|(event, _, _)| event == "ACCEPTED") {
        assert!(since.elapsed() < std::time::Duration::from_secs(8), "the redo stopped with its caller");
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert_eq!(r.swish.posted().len(), 1, "the redo reached Swish, once");
    sqlx::query(sqlx::AssertSqlSafe(format!("DROP FUNCTION {table}_slow() CASCADE")))
        .execute(r.state.store.pool())
        .await
        .expect("drop");
}

// A send whose connection never opened reached no one, so Swish having no record of it is no
// contradiction: in flight it is refused as a payout Swish may still pay, and in review redone.
#[tokio::test]
async fn a_payout_swish_never_received_is_redone_on_its_saying_it_has_no_record() {
    let mut config = common::config();
    let (unreachable, pool) = common::state_and_pool(config.clone()).await;
    let base = common::serve(swisha::http::internal_router().with_state(unreachable.clone())).await;
    let sent = Rig { base, state: unreachable, swish: MockSwish::new() };
    let body = json!({ "reference": "D4-1", "payee_alias": PHONE, "payee_ssn": SSN, "amount": 100.0 });
    let (code, answer) = post(&sent, "/swish/payout", body).await;
    assert_eq!((code, answer["status"].as_str()), (503, Some("PENDING")), "{answer}");

    let swish = MockSwish::new();
    config.swish_base_url = swish.clone().start().await;
    let ready = vec![common::payer_state(&common::payer(PAYER, common::PAYER_NUMBER))];
    let state = common::state_over(pool, config, ready).await;
    let base = common::serve(swisha::http::internal_router().with_state(state.clone())).await;
    let r = Rig { base, state, swish };

    let (code, answer) = post(&r, &redo("D4-1"), json!({ "reference": "D4-1-B", "by": "Anna" })).await;
    assert_eq!(code, 409, "{answer}");
    let error = answer["error"].as_str().unwrap_or_default();
    assert!(!error.contains("showed it held it"), "{answer}");
    assert!(error.contains("may still pay it"), "{answer}");
    assert!(r.swish.posted().is_empty());

    assert!(common::stage(&r.state, PAYER, "D4-1", "NEEDS_REVIEW").await);
    let (code, answer) = post(&r, &redo("D4-1"), json!({ "reference": "D4-1-B", "by": "Anna" })).await;
    assert_eq!(code, 202, "{answer}");
    assert_eq!(r.swish.posted().len(), 1);
}

// The send got no answer, yet a check found Swish holding the instruction without an outcome.
// That is recorded once, so a later "no record" is the same contradiction.
#[tokio::test]
async fn a_payout_a_check_found_held_is_never_redone_on_its_saying_it_has_no_record() {
    let r = rig().await;
    let uuid = staged(&r, "S-17", "CREATED").await;
    r.swish.clone().resolves_for(&uuid, "CREATED");
    for _ in 0..2 {
        swisha::swish::reconcile::reconcile(PAYER.into(), "S-17".into(), false, r.state.clone()).await;
    }
    let accepted = events(&r, "S-17").await.into_iter().filter(|(event, _, _)| event == "ACCEPTED").count();
    assert_eq!(accepted, 1, "recorded once, however often Swish says it holds it");
    assert!(common::stage(&r.state, PAYER, "S-17", "NEEDS_REVIEW").await);
    r.swish.clone().forgets(&uuid);

    let (code, body) = post(&r, &redo("S-17"), json!({ "reference": "S-17-B", "by": "Anna" })).await;
    assert_eq!(code, 409, "{body}");
    assert!(r.swish.posted().is_empty());
}

// The same, found by a redo: Swish holds it without an outcome, so the redo is refused, and its
// saying at the next press that it has no record of it is refused too.
#[tokio::test]
async fn a_payout_a_redo_found_held_is_never_redone_on_its_saying_it_has_no_record() {
    let r = rig().await;
    let uuid = staged(&r, "S-19", "NEEDS_REVIEW").await;
    r.swish.clone().resolves_for(&uuid, "CREATED");
    assert_eq!(post(&r, &redo("S-19"), json!({ "reference": "S-19-B", "by": "Anna" })).await.0, 409);
    r.swish.clone().forgets(&uuid);

    let (code, body) = post(&r, &redo("S-19"), json!({ "reference": "S-19-B", "by": "Anna" })).await;
    assert_eq!(code, 409, "{body}");
    assert!(r.swish.posted().is_empty());
}

// A payout with no instruction UUID on record cannot be asked about, so it is never redone.
#[tokio::test]
async fn a_payout_with_no_instruction_uuid_is_never_redone() {
    let r = rig().await;
    staged(&r, "S-18", "ERROR").await;
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE {} SET swish_ref = NULL WHERE reference = 'S-18'",
        r.state.config.table_payouts
    )))
    .execute(r.state.store.pool())
    .await
    .expect("drop the instruction");

    let (code, body) = post(&r, &redo("S-18"), json!({ "reference": "S-18-B", "by": "Anna" })).await;
    assert_eq!(code, 409, "{body}");
    assert!(body["error"].as_str().unwrap_or_default().contains("instruction UUID"), "{body}");
    assert!(r.swish.posted().is_empty());
}

// The store checks the chain too, under the lock: a PAID that lands after Swish was asked, and
// before the claim, still stops the redo.
#[tokio::test]
async fn a_redo_is_refused_when_an_earlier_payout_was_paid_meanwhile() {
    let r = rig().await;
    let first = staged(&r, "S-9A", "ERROR").await;
    r.swish.clone().resolves_for(&first, "ERROR");
    assert_eq!(post(&r, &redo("S-9A"), json!({ "reference": "S-9B", "by": "Anna" })).await.0, 202);
    assert!(common::stage(&r.state, PAYER, "S-9B", "ERROR").await);
    r.swish.clone().resolves_for(&uuid(&r, "S-9B").await, "ERROR");
    assert!(common::stage(&r.state, PAYER, "S-9A", "PAID").await, "a late PAID");

    let (code, body) = post(&r, &redo("S-9B"), json!({ "reference": "S-9C", "by": "Anna" })).await;
    assert_eq!(code, 409, "{body}");
    assert!(body["error"].as_str().unwrap_or_default().contains("S-9A"), "{body}");
    assert_eq!(view(&r, "S-9B").await["actions"], json!([]), "nothing to redo once an earlier one paid");
    assert_eq!(r.swish.posted().len(), 1);
}

// The same PAID, caught still committing: the chain is locked, so the claim waits for it, then
// sees it. Read without the lock, the claim would go ahead and pay a third time.
#[tokio::test]
async fn a_redo_waits_for_a_paid_still_committing_then_refuses() {
    use swisha::domain::payout::{NewRedo, RedoOutcome};
    let r = rig().await;
    staged(&r, "S-18A", "ERROR").await;
    let redo_of = |replaces: &'static str, reference: &'static str, uuid: &'static str| NewRedo {
        payer: PAYER,
        replaces,
        reference,
        payer_alias: common::PAYER_NUMBER,
        swish_ref: uuid,
        callback_identifier: None,
    };
    let first = redo_of("S-18A", "S-18B", "AAAABBBBCCCCDDDDEEEEFFFF0000D001");
    let claimed = r.state.store.claim_redo(&first, &event("S-18A", "REDONE", "")).await.expect("store");
    assert!(matches!(claimed, RedoOutcome::Claimed(_)), "the first redo is claimed");
    assert!(common::stage(&r.state, PAYER, "S-18B", "ERROR").await);

    let mut paying = r.state.store.pool().begin().await.expect("begin");
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE {} SET status = 'PAID' WHERE payer = $1 AND reference = 'S-18A'",
        r.state.config.table_payouts
    )))
    .bind(PAYER)
    .execute(&mut *paying)
    .await
    .expect("a PAID, not yet committed");
    let state = r.state.clone();
    let second = tokio::spawn(async move {
        let again = redo_of("S-18B", "S-18C", "AAAABBBBCCCCDDDDEEEEFFFF0000D002");
        state.store.claim_redo(&again, &event("S-18B", "REDONE", "")).await.expect("store")
    });
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    assert!(!second.is_finished(), "the claim waits for the PAID still committing");

    paying.commit().await.expect("commit");
    let outcome = second.await.expect("join");
    assert!(
        matches!(&outcome, RedoOutcome::EarlierPaid { reference, status } if reference == "S-18A" && status == "PAID"),
        "refused, naming the payout paid"
    );
    assert!(r.state.store.snapshot(PAYER, "S-18C").await.expect("store").is_none(), "nothing claimed");
}

// A redo is a new payout, held to the ceiling as one is: a payout made under a higher
// SWISH_MAX_PAYOUT is not paid again past a lowered one, and Swish is not even asked.
#[tokio::test]
async fn a_redo_is_held_to_the_payout_ceiling_of_now() {
    let swish = MockSwish::new().accepts();
    let mut config = common::config();
    config.swish_base_url = swish.clone().start().await;
    config.swish_max_payout = 100.0;
    let state = common::state_with(config).await;
    let base = common::serve(swisha::http::internal_router().with_state(state.clone())).await;
    let r = Rig { base, state, swish };
    let uuid = staged(&r, "M-1", "ERROR").await;
    r.swish.clone().resolves_for(&uuid, "ERROR");

    let (code, body) = post(&r, &redo("M-1"), json!({ "reference": "M-1-B", "by": "Anna" })).await;
    assert_eq!(code, 400, "{body}");
    let error = body["error"].as_str().unwrap_or_default();
    assert!(error.contains("250.50") && error.contains("100"), "{body}");
    assert_eq!((r.swish.get_count(), r.swish.posted().len()), (0, 0), "nothing asked, nothing sent");
    assert!(r.state.store.snapshot(PAYER, "M-1-B").await.expect("store").is_none());
}

// A redo needs a free reference. One already spent is named as taken, never answered with that
// other payout's own story, which would point at its redo or call it completed.
#[tokio::test]
async fn a_redo_onto_a_spent_reference_is_told_it_is_taken() {
    let r = rig().await;
    let uuid = staged(&r, "T-1", "ERROR").await;
    r.swish.clone().resolves_for(&uuid, "ERROR");
    staged(&r, "T-2", "ERROR").await;
    staged(&r, "T-3", "PAID").await;
    for taken in ["T-2", "T-3"] {
        let (code, body) = post(&r, &redo("T-1"), json!({ "reference": taken, "by": "Anna" })).await;
        assert_eq!(code, 409, "{body}");
        let error = body["error"].as_str().unwrap_or_default();
        assert!(error.contains(taken) && error.contains("taken"), "{taken}: {error}");
        assert!(!error.contains("/redo") && !error.contains("completed"), "{taken}: {error}");
    }
    assert!(r.swish.posted().is_empty());
}

// A decline over the recipient's own details meets the same decline however often it is sent, so
// it is never redone: each answer names a new payout with the details corrected instead.
#[tokio::test]
async fn a_payout_declined_over_the_recipients_details_is_not_redone() {
    let declined = |code: &str| format!(r#"[{{"errorCode":"{code}","errorMessage":"Declined"}}]"#);
    let r = rig_with(MockSwish::new().rejects(422, &declined("VR02"))).await;
    let body = json!({ "reference": "V-1", "payee_alias": PHONE, "payee_ssn": SSN, "amount": 250.5 });
    let (_, answer) = post(&r, "/swish/payout", body.clone()).await;
    assert_eq!(answer["status"], "DECLINED", "{answer}");
    assert_eq!(answer["actions"], json!([]), "{answer}");
    assert!(answer["review"].as_str().unwrap_or_default().contains("new payout"), "{answer}");

    let (repeat, redone, confirmed) = (
        post(&r, "/swish/payout", body).await,
        post(&r, &redo("V-1"), json!({ "reference": "V-1-B", "by": "Anna" })).await,
        post(&r, &confirm("V-1"), anna()).await,
    );
    for (what, (code, answer)) in [("repeat", repeat), ("redo", redone), ("confirm", confirmed)] {
        assert_eq!(code, 409, "{what}: {answer}");
        let error = answer["error"].as_str().unwrap_or_default();
        assert!(error.contains("new payout") && !error.to_lowercase().contains("redo it"), "{what}: {error}");
    }
    assert_eq!(r.swish.posted().len(), 1, "the original only");

    // A decline a resend can pass, such as the payer's own limit, is still redone.
    let r = rig_with(MockSwish::new().rejects(422, &declined("RF07"))).await;
    let body = json!({ "reference": "V-2", "payee_alias": PHONE, "payee_ssn": SSN, "amount": 250.5 });
    let (_, answer) = post(&r, "/swish/payout", body).await;
    assert_eq!(answer["status"], "DECLINED", "{answer}");
    assert_eq!(answer["actions"], json!(["redo"]), "{answer}");
}

// A repeat of a payout that failed or was never answered is pointed at redo, which asks Swish
// first and pays once, never at a new payout under a new reference, which would skip both.
#[tokio::test]
async fn a_repeat_of_a_failed_payout_is_pointed_at_redo() {
    let r = rig().await;
    for (reference, status) in [("S-10", "NEEDS_REVIEW"), ("S-11", "ERROR"), ("S-12", "DECLINED")] {
        paid_out(&r, reference, status).await;
        let body = json!({ "reference": reference, "payee_alias": PHONE, "payee_ssn": SSN, "amount": 250.5 });
        let (code, answer) = post(&r, "/swish/payout", body).await;
        assert_eq!(code, 409, "{status}");
        let text = answer["error"].as_str().unwrap_or_default();
        assert!(text.contains(&redo(reference)), "{status}: {text}");
        assert!(!text.contains("new reference"), "{status}: {text}");
    }
}

// What swisha tells a person

// A late failure: the money left and Swish then said it failed. The payout must stop looking paid
// and say to check the statement before paying again, from the moment it is known.
#[tokio::test]
async fn a_late_failure_tells_a_person_to_check_the_statement() {
    let r = rig().await;
    paid_out(&r, "V-1", "DEBITED").await;
    let uuid = r.state.store.snapshot(PAYER, "V-1").await.expect("store").and_then(|s| s.swish_ref).expect("uuid");
    r.state
        .store
        .record_event(&swisha::domain::payout::EventRecord {
            payer: PAYER.into(),
            reference: "V-1".into(),
            swish_ref: Some(uuid),
            event: "CALLBACK".into(),
            status: Some("DEBITED".into()),
            amount: None,
            payee_alias: None,
            error_code: None,
            error_message: None,
            ip: None,
            actor: None,
            note: None,
        })
        .await
        .expect("event");
    common::stage(&r.state, PAYER, "V-1", "ERROR").await;

    let seen = view(&r, "V-1").await;
    assert_eq!(seen["status"], "ERROR");
    assert_eq!(seen["review"], swisha::domain::review::RETURNED.english, "{seen}");
    assert_eq!(seen["actions"], json!(["redo"]));
}

#[tokio::test]
async fn a_payout_in_flight_asks_nothing_of_a_person() {
    let r = rig().await;
    for (reference, status) in [("V-2", "PENDING"), ("V-3", "DEBITED"), ("V-4", "CREATED")] {
        paid_out(&r, reference, status).await;
        let seen = view(&r, reference).await;
        assert!(seen["review"].is_null(), "{seen}");
        assert_eq!(seen["actions"], json!([]), "{seen}");
        assert!(seen["manual_confirmation"].is_null() && seen["replaces"].is_null() && seen["replaced_by"].is_null());
    }
    paid_out(&r, "V-5", "NEEDS_REVIEW").await;
    let seen = view(&r, "V-5").await;
    assert_eq!(seen["review"], swisha::domain::review::NO_OUTCOME.english);
    assert_eq!(seen["actions"], json!(["confirm_paid", "redo"]));
}

// Once a person paid it again, the payout is closed: the redo carries it on. Confirming the old one
// as well would put two payments in the books for one invoice.
#[tokio::test]
async fn a_redone_payout_is_closed_to_a_person() {
    let r = rig().await;
    staged(&r, "R-9", "NEEDS_REVIEW").await;
    send_ended(&r, "R-9", "PENDING").await;
    assert_eq!(view(&r, "R-9").await["actions"], json!(["confirm_paid", "redo"]));
    assert_eq!(post(&r, &redo("R-9"), json!({ "reference": "R-9-B", "by": "Anna" })).await.0, 202);

    let closed = view(&r, "R-9").await;
    assert_eq!(closed["actions"], json!([]), "{closed}");
    assert!(closed["review"].is_null(), "{closed}");
    let (code, body) = post(&r, &confirm("R-9"), anna()).await;
    assert_eq!(code, 409, "{body}");
    assert!(body["error"].as_str().unwrap_or_default().contains("R-9-B"), "it names the redo: {body}");
    assert_eq!(view(&r, "R-9").await["status"], "NEEDS_REVIEW");
}

// The interleaving the race needs, held open by hand: a redo has locked the payout and claimed its
// replacement, and a confirmation arrives before it commits. Once it commits, the confirmation must
// see the redo and refuse, or the books hold a confirmed payout beside a second payment.
#[tokio::test]
async fn a_confirmation_waiting_on_a_redo_sees_it() {
    let r = rig().await;
    paid_out(&r, "R-10", "NEEDS_REVIEW").await;
    let table = r.state.config.table_payouts.clone();
    let mut redo = r.state.store.pool().begin().await.expect("begin");
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT status FROM {table} WHERE payer = $1 AND reference = 'R-10' FOR UPDATE"
    )))
    .bind(PAYER)
    .execute(&mut *redo)
    .await
    .expect("lock");
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "INSERT INTO {table} (payer, reference, payer_alias, payee_alias, amount, message, swish_ref, status, replaces)
         VALUES ($1, 'R-10-B', '1234679304', '46701234567', 250.5, 'Kvitto 7', 'AAAABBBBCCCCDDDDEEEEFFFF0000AAAA', 'CREATED', 'R-10')"
    )))
    .bind(PAYER)
    .execute(&mut *redo)
    .await
    .expect("claim the redo");

    let state = r.state.clone();
    let confirm = tokio::spawn(async move {
        let confirmation = event("R-10", swisha::domain::review::CONFIRMED_PAID, "PAID");
        state.store.confirm_paid(&confirmation).await.expect("confirm")
    });
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(!confirm.is_finished(), "the confirmation waits for the redo's lock");
    redo.commit().await.expect("commit");

    assert_eq!(
        confirm.await.expect("join"),
        swisha::domain::payout::ConfirmOutcome::Redone("R-10-B".into())
    );
    assert_eq!(view(&r, "R-10").await["status"], "NEEDS_REVIEW", "the payout was not confirmed");
}

// The other order: a confirmation holds the payout, and a redo waits behind it. Once the payout is
// PAID the redo must see it and refuse, rather than send a second payment.
#[tokio::test]
async fn a_redo_waiting_on_a_confirmation_sees_it() {
    let r = rig().await;
    staged(&r, "R-11", "NEEDS_REVIEW").await;
    send_ended(&r, "R-11", "PENDING").await;
    let table = r.state.config.table_payouts.clone();
    let mut confirmation = r.state.store.pool().begin().await.expect("begin");
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE {table} SET status = 'PAID' WHERE payer = $1 AND reference = 'R-11'"
    )))
    .bind(PAYER)
    .execute(&mut *confirmation)
    .await
    .expect("confirm");

    let base = r.base.clone();
    let press = tokio::spawn(async move {
        reqwest::Client::new()
            .post(format!("{base}{}", redo("R-11")))
            .header("x-api-secret", SECRET)
            .header(common::KEY_HEADER, common::key(common::PAYER))
            .json(&json!({ "reference": "R-11-B", "by": "Bo" }))
            .send()
            .await
            .expect("request")
            .status()
            .as_u16()
    });
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(!press.is_finished(), "the redo waits for the confirmation's lock");
    confirmation.commit().await.expect("commit");

    assert_eq!(press.await.expect("join"), 409, "a paid payout is never redone");
    assert!(r.swish.posted().is_empty(), "no second payment went out");
}

// A time in seconds is the second it fell in. Rounded, a confirmation at 15.9 seconds would read
// as 16, after a reading taken later.
#[tokio::test]
async fn times_in_seconds_are_never_rounded_up() {
    let r = rig().await;
    paid_out(&r, "T-1", "NEEDS_REVIEW").await;
    assert_eq!(post(&r, &confirm("T-1"), anna()).await.0, 200);
    let pool = r.state.store.pool();
    let at = "2026-10-08 23:27:15.9+00";
    let events = &r.state.config.table_events;
    sqlx::query(sqlx::AssertSqlSafe(format!("UPDATE {events} SET created_at = $1::timestamptz WHERE reference = 'T-1'")))
        .bind(at)
        .execute(pool)
        .await
        .expect("set the time");
    let whole = swisha::util::time::unix_from_utc(2026, 10, 8, 23, 27, 15);
    let view = r.state.store.status_view(PAYER, "T-1").await.expect("store").expect("row");
    assert_eq!(view.manual_confirmation.expect("confirmed").at, whole, "the confirmation's time");

    let hash = swisha::payers::new_key().hash;
    let added = swisha::domain::payer::NewPayer {
        name: "t1",
        nickname: "T",
        number: "1230000001",
        added_by: "Anna",
        key_hash: &hash,
    };
    r.state.store.add_payer(&added, &common::logged()).await.expect("add");
    sqlx::query(sqlx::AssertSqlSafe(format!("UPDATE {} SET created_at = $1::timestamptz", r.state.config.table_payers)))
        .bind(at)
        .execute(pool)
        .await
        .expect("set the time");
    let stored = r.state.store.payers().await.expect("store");
    let added = stored.iter().find(|payer| payer.name == "t1").expect("stored");
    assert_eq!(added.created_at, whole, "the number's time");
}

// The service log is the operator's record, not the payout's: a redo's POST never waits on a write
// to it.
#[tokio::test]
async fn a_redo_reaches_swish_while_the_service_log_is_locked() {
    let r = rig().await;
    let uuid = staged(&r, "S-20", "ERROR").await;
    drop(r.swish.clone().resolves_for(&uuid, "ERROR"));
    let mut lock = r.state.store.pool().begin().await.expect("begin");
    sqlx::query(sqlx::AssertSqlSafe(format!("LOCK TABLE {} IN ACCESS EXCLUSIVE MODE", r.state.config.table_logs)))
        .execute(&mut *lock)
        .await
        .expect("lock");

    let sent = r.swish.posted().len();
    let started = std::time::Instant::now();
    let (url, body) = (format!("{}{}", r.base, redo("S-20")), json!({ "reference": "S-20-B", "by": "Anna" }));
    let request = tokio::spawn(async move {
        reqwest::Client::new()
            .post(url)
            .header("x-api-secret", SECRET)
            .header(common::KEY_HEADER, common::key(PAYER))
            .json(&body)
            .send()
            .await
            .expect("request")
            .status()
            .as_u16()
    });
    while r.swish.posted().len() == sent && started.elapsed() < std::time::Duration::from_secs(3) {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let reached = started.elapsed();
    lock.rollback().await.expect("unlock");
    assert_eq!(request.await.expect("join"), 202);
    assert!(reached < std::time::Duration::from_secs(1), "the POST waited {reached:?} on the service log");
}
