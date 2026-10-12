#![cfg(feature = "http")]

// submit and reconcile reach Swish over the network, so they are driven here against a scripted
// stand-in that can give every answer Swish documents.

mod common;

use common::{MockSwish, PAYER, PHONE, SSN};
use swisha::domain::payout::NewPayout;
use swisha::error::ApiError;
use swisha::state::SharedState;
use swisha::store::PayoutStore;
use swisha::swish::payload::ExecutePayoutArgs;
use swisha::domain::payout::CheckSchedule;
use swisha::domain::status::{ANSWERS, TERMINAL_ADVANCE};
use swisha::swish::reconcile::{reconcile, sweep};
use swisha::swish::submit::{
    NoAnswer, PollAnswer, Polled, lookup_payout_status, poll_payout_status, sign_payout, submit_payout,
};

const UUID: &str = "AAAABBBBCCCCDDDDEEEEFFFF00001111";
// The reference a poll is about. Nothing is stored under it, so every question goes to Swish.
const POLLED: &str = "NOT-STORED";

fn answered(polled: Polled) -> PollAnswer {
    match polled {
        Polled::Answered(answer) => answer,
        other => panic!("Swish gave an answer, and nothing is stored to settle: {other:?}"),
    }
}

// Due almost at once and again soon after, with every check past the window: each is a last check.
const LAST: CheckSchedule = CheckSchedule {
    first_after: std::time::Duration::from_millis(50),
    every: std::time::Duration::from_millis(200),
    window: std::time::Duration::ZERO,
};

// One round of checks on LAST, as a sweep runs it without the pause between questions.
async fn check_now(state: &SharedState) -> usize {
    let due = state.store.claim_due_checks(PAYER, &LAST, 5).await.expect("claim");
    let count = due.len();
    for check in due {
        reconcile(check.payer, check.reference, check.last, state.clone()).await;
    }
    count
}

async fn app_with(mock: MockSwish) -> SharedState {
    let swish = mock.start().await;
    let mut config = common::config();
    config.swish_base_url = swish;
    common::state_with(config).await
}

fn args<'a>(reference: &'a str, uuid: &'a str) -> ExecutePayoutArgs<'a> {
    ExecutePayoutArgs {
        reference,
        payout_uuid: uuid,
        payee_alias: "46701234567",
        payee_ssn: Some(SSN),
        amount: 100.0,
        message: "test",
    }
}

// The one number the tests pay from, as the registry holds it.
fn sender(state: &SharedState) -> std::sync::Arc<swisha::state::PayerState> {
    state.payer(PAYER).expect("the test payer is loaded")
}

// One instruction, signed and submitted as the payout route sends it.
async fn submit(state: &SharedState) -> Result<(), ApiError> {
    let sender = sender(state);
    let signed = sign_payout(&sender, &args("INV-1", UUID), state).await?;
    submit_payout(&sender, signed, UUID, state).await
}

async fn stage(state: &SharedState, reference: &str, swish_ref: Option<&str>, status: &str) {
    state
        .store
        .claim(&NewPayout {
            payer: PAYER,
            reference,
            payer_alias: common::PAYER_NUMBER,
            payee_alias: PHONE,
            payee_ssn: Some(SSN),
            amount: 100.0,
            message: "test",
            swish_ref: swish_ref.unwrap_or(""),
            callback_identifier: None,
        })
        .await
        .expect("claim");
    common::stage(state, PAYER, reference, status).await;
}

#[tokio::test]
async fn a_201_means_the_instruction_was_accepted() {
    let state = app_with(MockSwish::new().accepts()).await;
    assert!(submit(&state).await.is_ok());
}

// Swish answers a rejected instruction with an array of Error objects, and the caller has to be
// able to read the code out of it: that code is what tells a person whether to pay again.
#[tokio::test]
async fn a_rejection_carries_the_status_and_the_body() {
    let body = r#"[{"errorCode":"PA02","errorMessage":"Amount value is missing"}]"#;
    let state = app_with(MockSwish::new().rejects(422, body)).await;

    match submit(&state).await {
        Err(ApiError::SwishRejected { code, body }) => {
            assert_eq!(code, 422);
            assert!(body.contains("PA02"), "body was {body}");
        }
        other => panic!("expected a Swish rejection, got {other:?}"),
    }
}

#[tokio::test]
async fn any_status_other_than_201_is_a_rejection() {
    for code in [400, 401, 403, 415, 429, 500] {
        let state = app_with(MockSwish::new().rejects(code, "")).await;
        match submit(&state).await {
            Err(ApiError::SwishRejected { code: got, .. }) => assert_eq!(got, code),
            other => panic!("{code} should be a rejection, got {other:?}"),
        }
    }
}

// DECLINED tells a person no money moved, so what a refusal proves is pinned case by case.
// Widening it is how a payout Swish holds gets written off as never sent.
#[test]
fn a_refusal_is_read_for_its_code_and_what_it_proves() {
    use swisha::domain::status::PayoutStatus::{Declined, Pending};
    use swisha::swish::submit::read_rejection;

    let pa06 = r#"[{"errorCode":"PA06","errorMessage":"Incorrect ssn format"}]"#;
    for (http, body, code, status) in [
        (422, pa06, "PA06", Declined),
        (422, r#"[{"errorCode":"AM02"},{"errorCode":"PA06"}]"#, "AM02", Declined),
        (422, r#"{"errorCode":"ACMT07"}"#, "ACMT07", Declined),
        (422, r#"[{"errorCode":" VR02 "}]"#, "VR02", Declined),
        (422, r#"[{"errorCode":"PA06"},{"errorCode":"RP09"}]"#, "PA06", Pending),
        (422, r#"[{"errorCode":"rp09"}]"#, "rp09", Pending),
        (422, "[]", "422", Declined),
        (400, "", "400", Declined),
        (401, "<html>Unauthorized</html>", "401", Declined),
        (403, r#"[{"errorCode":"  "}]"#, "403", Declined),
        (415, "", "415", Declined),
        (429, "Too Many Requests", "429", Declined),
        (404, "", "404", Pending),
        (409, pa06, "PA06", Pending),
        (500, pa06, "PA06", Pending),
        (503, "", "503", Pending),
    ] {
        let rejection = read_rejection(http, body);
        assert_eq!(rejection.code, code, "{http} {body}");
        assert_eq!(rejection.status, status, "{http} {body}");
    }
}

// Only Swish's four answers are outcomes. Anything else it reports about a payout, such as the
// stages it passes through on the way, is no answer yet.
#[tokio::test]
async fn a_lookup_reads_anything_but_an_answer_as_pending() {
    for status in ["PAID", "DEBITED", "DECLINED", "ERROR"] {
        let state = app_with(MockSwish::new().resolves_to(status)).await;
        let answer = lookup_payout_status(&sender(&state), UUID, &state).await.expect("answer");
        assert_eq!(answer.status, status);
    }
    for status in ["CREATED", "INITIATED", "BIR_PAYMENT_INITIATED", "NEEDS_REVIEW", "paid", ""] {
        let state = app_with(MockSwish::new().resolves_to(status)).await;
        let answer = lookup_payout_status(&sender(&state), UUID, &state).await.expect("answer");
        assert_eq!(answer.status, "PENDING", "{status:?} is not an outcome");
    }
}

// A lookup Swish does not answer is never an outcome, and says why: no record, an answer that is
// not a payout such as 429, or no answer at all.
#[tokio::test]
async fn a_lookup_without_an_answer_says_why() {
    let state = app_with(MockSwish::new()).await;
    assert_eq!(lookup_payout_status(&sender(&state), UUID, &state).await, Err(NoAnswer::NotFound));

    let state = app_with(MockSwish::new().answers_gets_with(429)).await;
    assert_eq!(lookup_payout_status(&sender(&state), UUID, &state).await, Err(NoAnswer::Unusable));

    let state = common::state().await;
    assert_eq!(lookup_payout_status(&sender(&state), UUID, &state).await, Err(NoAnswer::Unreachable));
}

#[tokio::test]
async fn polling_returns_whichever_answer_swish_reports() {
    for status in ["PAID", "DEBITED", "DECLINED", "ERROR"] {
        let state = app_with(MockSwish::new().resolves_to(status)).await;
        assert_eq!(
            answered(poll_payout_status(&sender(&state), POLLED, UUID, &state, ANSWERS).await.expect("poll")).status,
            status
        );
    }
}

// DEBITED is not Swish's last word. Polled for what can follow it, the poll keeps asking past
// DEBITED and stops at PAID, or at ERROR when the recipient's bank refused the money.
#[tokio::test]
async fn polling_follows_a_debited_payout_to_swishs_last_word() {
    for last in ["PAID", "ERROR"] {
        let mock = MockSwish::new().resolves_through(&["DEBITED", "DEBITED", last]);
        let state = app_with(mock.clone()).await;
        let answer = poll_payout_status(&sender(&state), POLLED, UUID, &state, TERMINAL_ADVANCE.1).await;
        assert_eq!(answered(answer.expect("poll")).status, last);
        assert_eq!(mock.get_count(), 3, "it stops as soon as Swish gives its last word");
    }
}

// Slow, and worth its cost: a poll that never hears PAID or ERROR hands back the DEBITED it did
// hear rather than PENDING, which would be a step backwards.
#[tokio::test]
async fn polling_hands_back_debited_when_swish_says_nothing_more() {
    let state = app_with(MockSwish::new().resolves_to("DEBITED")).await;
    let answer = poll_payout_status(&sender(&state), POLLED, UUID, &state, TERMINAL_ADVANCE.1).await;
    assert_eq!(answered(answer.expect("poll")).status, "DEBITED");
}

// A UUID Swish has no record of must not read as pending: pending invites waiting for an answer
// that is never coming.
#[tokio::test]
async fn an_instruction_swish_has_no_record_of_is_an_error_rather_than_pending() {
    let state = app_with(MockSwish::new()).await;
    assert!(poll_payout_status(&sender(&state), POLLED, UUID, &state, ANSWERS).await.is_err());
}

// Swish saying only that it holds the instruction is no PENDING to record: it is Swish holding it,
// which a later "no record" must not undo. Nothing usable at all is no answer. Both run at once.
#[tokio::test]
async fn a_poll_tells_swish_holding_it_from_swish_saying_nothing() {
    let held = app_with(MockSwish::new().resolves_to("CREATED")).await;
    let silent = app_with(MockSwish::new().answers_gets_with(429)).await;
    let (holding, quiet) = (sender(&held), sender(&silent));
    let (held, silent) = tokio::join!(
        poll_payout_status(&holding, POLLED, UUID, &held, ANSWERS),
        poll_payout_status(&quiet, POLLED, UUID, &silent, ANSWERS),
    );
    assert!(matches!(held, Ok(Polled::Held)), "{held:?}");
    assert!(silent.is_err(), "{silent:?}");
}

// Swish's answer to a GET carries its error code and message, as its callback does. Both are
// kept, so a failure settled without a callback still says what Swish said.
fn declined() -> serde_json::Value {
    serde_json::json!({ "status": "ERROR", "errorCode": "RF07", "errorMessage": "Transaction declined" })
}

#[tokio::test]
async fn polling_keeps_swishs_error_code_and_message() {
    let state = app_with(MockSwish::new().resolves_with(declined())).await;
    let answer = answered(poll_payout_status(&sender(&state), POLLED, UUID, &state, ANSWERS).await.expect("poll"));
    assert_eq!(answer.status, "ERROR");
    assert_eq!(answer.error_code.as_deref(), Some("RF07"));
    assert_eq!(answer.error_message.as_deref(), Some("Transaction declined"));
}

#[tokio::test]
async fn reconcile_keeps_swishs_error_code() {
    let state = app_with(MockSwish::new().resolves_with(declined())).await;
    stage(&state, "INV-1", Some(UUID), "PENDING").await;

    reconcile(PAYER.into(), "INV-1".into(), false, state.clone()).await;

    let view = state.store.status_view(PAYER, "INV-1").await.expect("store").expect("row");
    assert_eq!(view.status.as_deref(), Some("ERROR"));
    assert_eq!(view.error_code.as_deref(), Some("RF07"));
}

// The sweep announces an answer only when the store took it. A refused write means the payout
// already holds an answer, announced when it was written, and announcing this one would contradict it.
#[tokio::test]
async fn reconcile_announces_only_what_it_wrote() {
    let state = app_with(MockSwish::new().resolves_to("DEBITED")).await;
    stage(&state, "INV-1", Some(UUID), "PAID").await;
    let mut announced = state.stream.subscribe(PAYER).expect("room for a stream");

    reconcile(PAYER.into(), "INV-1".into(), false, state.clone()).await;

    assert_eq!(status_of(&state, "INV-1").await, "PAID");
    assert_nothing_announced(&mut announced);
}

#[tokio::test]
async fn reconcile_settles_a_payout_swish_has_resolved() {
    let mock = MockSwish::new().resolves_to("PAID");
    let state = app_with(mock.clone()).await;
    stage(&state, "INV-1", Some(UUID), "PENDING").await;

    reconcile(PAYER.into(), "INV-1".into(), false, state.clone()).await;

    let snapshot = state.store.snapshot(PAYER, "INV-1").await.expect("snapshot").expect("row");
    assert_eq!(snapshot.status.as_deref(), Some("PAID"));
    assert_eq!(
        snapshot.swish_ref.as_deref(),
        Some(UUID),
        "reconcile must not mint a new instruction UUID"
    );
    assert_eq!(mock.get_count(), 1, "one question per check");
}

// A DEBITED payout whose PAID callback never arrived is settled by asking.
#[tokio::test]
async fn reconcile_records_the_paid_a_lost_callback_carried() {
    let state = app_with(MockSwish::new().resolves_to("PAID")).await;
    stage(&state, "INV-1", Some(UUID), "DEBITED").await;

    reconcile(PAYER.into(), "INV-1".into(), false, state.clone()).await;

    assert_eq!(status_of(&state, "INV-1").await, "PAID");
}

// The recipient's bank refused the money and Swish returned it. The payout must stop looking paid,
// and say why.
#[tokio::test]
async fn reconcile_records_a_late_error_after_debited() {
    let state = app_with(MockSwish::new().resolves_with(declined())).await;
    stage(&state, "INV-1", Some(UUID), "DEBITED").await;
    let mut announced = state.stream.subscribe(PAYER).expect("room for a stream");

    reconcile(PAYER.into(), "INV-1".into(), false, state.clone()).await;

    let view = state.store.status_view(PAYER, "INV-1").await.expect("store").expect("row");
    assert_eq!(view.status.as_deref(), Some("ERROR"));
    assert_eq!(view.error_code.as_deref(), Some("RF07"));
    assert_eq!(announced_statuses(&mut announced), ["ERROR"]);
}

// A DEBITED arriving after the late ERROR, such as Swish's one callback retry, is the reverse of
// that move and must not make a returned payout look paid again.
#[tokio::test]
async fn a_stale_debited_never_undoes_a_late_error() {
    let state = app_with(MockSwish::new().resolves_to("DEBITED")).await;
    stage(&state, "INV-1", Some(UUID), "DEBITED").await;
    stage(&state, "INV-1", Some(UUID), "ERROR").await;

    reconcile(PAYER.into(), "INV-1".into(), false, state.clone()).await;

    assert_eq!(status_of(&state, "INV-1").await, "ERROR");
}

// Asked hourly for two days, Swish mostly repeats itself. That adds nothing to the trail.
#[tokio::test]
async fn reconcile_adds_nothing_when_swish_repeats_what_is_stored() {
    let state = app_with(MockSwish::new().resolves_to("DEBITED")).await;
    stage(&state, "INV-1", Some(UUID), "DEBITED").await;
    let mut announced = state.stream.subscribe(PAYER).expect("room for a stream");

    reconcile(PAYER.into(), "INV-1".into(), false, state.clone()).await;

    assert_eq!(events_of(&state, "INV-1").await, Vec::<String>::new());
    assert_nothing_announced(&mut announced);
}

// The guarantee, observed rather than asserted about the source: reconcile reads and never
// submits. A POST arriving at the mock during reconciliation is a second payout instruction.
#[tokio::test]
async fn reconcile_never_sends_an_instruction() {
    for status in ["PAID", "DEBITED", "DECLINED", "ERROR"] {
        for last in [false, true] {
            let mock = MockSwish::new().resolves_to(status);
            let state = app_with(mock.clone()).await;
            stage(&state, "INV-1", Some(UUID), "PENDING").await;

            reconcile(PAYER.into(), "INV-1".into(), last, state).await;

            assert!(
                mock.posted().is_empty(),
                "reconciling a {status} payout sent {} instruction(s) to Swish",
                mock.posted().len()
            );
            assert!(mock.get_count() > 0, "it should have asked, though");
        }
    }
}

// A terminal status is never walked backwards, even by reconcile writing what Swish just said.
#[tokio::test]
async fn reconcile_cannot_move_a_paid_payout() {
    let state = app_with(MockSwish::new().resolves_to("ERROR")).await;
    stage(&state, "INV-1", Some(UUID), "PAID").await;

    reconcile(PAYER.into(), "INV-1".into(), true, state.clone()).await;

    assert_eq!(status_of(&state, "INV-1").await, "PAID", "PAID is final");
}

// Nothing was ever submitted under a UUID, so there is nothing to ask Swish about. Polling a
// blank one would request the payouts collection and read whatever came back as a status.
#[tokio::test]
async fn a_payout_with_no_instruction_uuid_goes_straight_to_review() {
    let mock = MockSwish::new().resolves_to("PAID");
    let state = app_with(mock.clone()).await;
    stage(&state, "INV-1", None, "PENDING").await;

    reconcile(PAYER.into(), "INV-1".into(), false, state.clone()).await;

    assert_eq!(status_of(&state, "INV-1").await, "NEEDS_REVIEW");
    assert_eq!(mock.get_count(), 0, "there was nothing to ask about");
}

// NEEDS_REVIEW is swisha's own "no answer", so the store refuses it over an answer Swish gave. A
// refused write is not announced either, or the stream would contradict the payout.
#[tokio::test]
async fn needs_review_is_announced_only_when_it_was_written() {
    let state = app_with(MockSwish::new().resolves_to("PAID")).await;
    stage(&state, "INV-1", None, "DEBITED").await;
    let mut announced = state.stream.subscribe(PAYER).expect("room for a stream");

    reconcile(PAYER.into(), "INV-1".into(), false, state.clone()).await;

    assert_eq!(status_of(&state, "INV-1").await, "DEBITED");
    assert_nothing_announced(&mut announced);
    assert!(!events_of(&state, "INV-1").await.contains(&"NEEDS_REVIEW".to_string()));
}

// Before the last check a payout Swish has not resolved stays as it is, for the next check.
#[tokio::test]
async fn reconcile_leaves_an_unresolved_payout_alone_before_the_last_check() {
    let mock = MockSwish::new().resolves_to("CREATED"); // never an answer
    let state = app_with(mock.clone()).await;
    stage(&state, "INV-1", Some(UUID), "PENDING").await;

    reconcile(PAYER.into(), "INV-1".into(), false, state.clone()).await;

    assert_eq!(status_of(&state, "INV-1").await, "PENDING", "it waits for the next check");
    assert!(mock.posted().is_empty(), "waiting must not send anything");
}

// A payer that is not loaded cannot ask Swish, which says nothing about the payout: Swish may still
// pay it. Its checks wait until an instance that has it loaded can ask, the last one included.
#[tokio::test]
async fn a_payout_whose_payer_is_not_loaded_waits_until_it_is_loaded() {
    let mock = MockSwish::new().resolves_to("PAID");
    let state = app_with(mock.clone()).await;
    state
        .store
        .claim(&NewPayout {
            payer: "gone",
            reference: "INV-1",
            payer_alias: "1239999999",
            payee_alias: PHONE,
            payee_ssn: Some(SSN),
            amount: 100.0,
            message: "test",
            swish_ref: UUID,
            callback_identifier: None,
        })
        .await
        .expect("claim");
    let status = |state: SharedState| async move {
        state.store.snapshot("gone", "INV-1").await.expect("store").and_then(|s| s.status).unwrap_or_default()
    };

    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert_eq!(sweep(&state, "gone", &LAST).await.expect("sweep"), 0, "this instance cannot ask");
    let checked: Option<bool> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT TRUE FROM {} WHERE payer = 'gone' AND checked_at IS NOT NULL",
        state.config.table_payouts
    )))
    .fetch_optional(state.store.pool())
    .await
    .expect("read");
    assert_eq!(checked, None, "a check it cannot run is left for an instance that can");

    reconcile("gone".into(), "INV-1".into(), true, state.clone()).await;
    assert_eq!(status(state.clone()).await, "CREATED", "no question asked is no outcome, last check or not");
    assert_eq!(mock.get_count(), 0, "only the payer that sent it may ask");
}

// A last check whose answer the store refused has not happened: the payout keeps its checks, rather
// than going to review with redo offered, or ending, with Swish's PAID lost.
#[tokio::test]
async fn a_last_answer_the_store_refused_is_asked_for_again() {
    for stored in ["PENDING", "DEBITED"] {
        let mock = MockSwish::new().resolves_to("PAID");
        let state = app_with(mock.clone()).await;
        stage(&state, "INV-1", Some(UUID), stored).await;
        let payouts = state.config.table_payouts.clone();
        common::refuse(&state, &payouts, "status = 'PAID'").await;
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        assert_eq!(check_now(&state).await, 1, "{stored}");
        assert_eq!(mock.get_count(), 1, "{stored}: Swish was asked");
        assert_eq!(status_of(&state, "INV-1").await, stored, "{stored}");
        assert!(events_of(&state, "INV-1").await.is_empty(), "{stored}: nothing recorded, nothing ended");

        common::allow(&state, &payouts).await;
        tokio::time::sleep(LAST.every + std::time::Duration::from_millis(50)).await;
        assert_eq!(check_now(&state).await, 1, "{stored}: the check is due again");
        assert_eq!(status_of(&state, "INV-1").await, "PAID", "{stored}");
    }
}

// The same when the store refuses the answer's event: a status never lands without the event that
// explains it, so the next check finds Swish's ERROR new and records it with its code.
#[tokio::test]
async fn an_answer_whose_event_was_refused_is_recorded_whole_at_the_next_check() {
    let error = serde_json::json!({ "status": "ERROR", "errorCode": "RF07", "errorMessage": "Declined" });
    let state = app_with(MockSwish::new().resolves_with(error)).await;
    stage(&state, "INV-1", Some(UUID), "PENDING").await;
    let events = state.config.table_events.clone();
    common::refuse(&state, &events, "event = 'ERROR'").await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    check_now(&state).await;
    assert_eq!(status_of(&state, "INV-1").await, "PENDING", "no ERROR without its code");

    common::allow(&state, &events).await;
    tokio::time::sleep(LAST.every + std::time::Duration::from_millis(50)).await;
    check_now(&state).await;
    let view = state.store.status_view(PAYER, "INV-1").await.expect("store").expect("row");
    assert_eq!((view.status.as_deref(), view.error_code.as_deref()), (Some("ERROR"), Some("RF07")));
}

// The last check hands what Swish never answered to a person.
#[tokio::test]
async fn an_unresolved_payout_reaches_review_at_the_last_check() {
    for stored in ["CREATED", "PENDING"] {
        let mock = MockSwish::new().resolves_to("CREATED"); // never an answer
        let state = app_with(mock.clone()).await;
        stage(&state, "INV-1", Some(UUID), stored).await;

        reconcile(PAYER.into(), "INV-1".into(), true, state.clone()).await;

        assert_eq!(status_of(&state, "INV-1").await, "NEEDS_REVIEW", "from {stored}");
        assert!(mock.posted().is_empty(), "giving up must not send anything");
    }
}

// A last check that got no answer has not happened, whatever stopped it: Swish unreachable, or
// answering 429 or 503. Each payout keeps its status and its checks; only Swish's PA01 ends them.
#[tokio::test]
async fn a_last_check_without_an_answer_keeps_the_checks() {
    let staged = [("N-1", "CREATED"), ("N-2", "PENDING"), ("N-3", "DEBITED"), ("N-4", "ERROR")];
    for (gets, why) in [(Some(429), "429"), (Some(503), "503"), (None, "unreachable")] {
        let state = match gets {
            Some(code) => app_with(MockSwish::new().answers_gets_with(code)).await,
            None => common::state_with(common::config()).await,
        };
        for (i, (reference, status)) in staged.into_iter().enumerate() {
            stage(&state, reference, Some(&format!("AAAABBBBCCCCDDDDEEEEFFFF0000000{i}")), status).await;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        assert_eq!(check_now(&state).await, 4, "{why}");
        for (reference, status) in staged {
            assert_eq!(status_of(&state, reference).await, status, "{why}: {reference} keeps {status}");
            assert!(events_of(&state, reference).await.is_empty(), "{why}: nothing ended for {reference}");
        }
        tokio::time::sleep(LAST.every + std::time::Duration::from_millis(50)).await;
        assert_eq!(check_now(&state).await, 4, "{why}: every last check is due again");
    }

    let state = app_with(MockSwish::new()).await;
    stage(&state, "N-5", Some(UUID), "PENDING").await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    check_now(&state).await;
    assert_eq!(status_of(&state, "N-5").await, "NEEDS_REVIEW", "Swish's word that it holds nothing ends them");
}

// What Swish did say stands at the last check: a DEBITED or ERROR payout keeps Swish's word, and
// the trail and the stream say the checks have ended, so a person can take it from there.
#[tokio::test]
async fn the_last_check_keeps_swishs_word_and_says_the_checks_ended() {
    for stored in ["DEBITED", "ERROR"] {
        let state = app_with(MockSwish::new().resolves_to(stored)).await;
        stage(&state, "INV-1", Some(UUID), "DEBITED").await;
        stage(&state, "INV-1", Some(UUID), stored).await;
        let mut announced = state.stream.subscribe(PAYER).expect("room for a stream");

        reconcile(PAYER.into(), "INV-1".into(), true, state.clone()).await;

        assert_eq!(status_of(&state, "INV-1").await, stored);
        assert_eq!(events_of(&state, "INV-1").await, ["CHECKS_ENDED"], "{stored}");
        assert_eq!(announced_statuses(&mut announced), [stored]);
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert_eq!(check_now(&state).await, 0, "{stored}: never asked about again");
    }
}

// A sweep asks about every one of its number's payouts due a check, one question each, and the
// next sweep leaves them alone until their next check is due.
#[tokio::test]
async fn a_sweep_asks_once_about_each_due_payout() {
    let mock = MockSwish::new().resolves_to("PAID");
    let state = app_with(mock.clone()).await;
    stage(&state, "INV-1", Some(UUID), "PENDING").await;
    stage(&state, "INV-2", Some("AAAABBBBCCCCDDDDEEEEFFFF00002222"), "DEBITED").await;
    let schedule = CheckSchedule {
        first_after: std::time::Duration::from_millis(50),
        every: std::time::Duration::from_secs(3600),
        window: std::time::Duration::from_secs(3600),
    };
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    assert_eq!(sweep(&state, PAYER, &schedule).await.expect("sweep"), 2);
    assert_eq!(status_of(&state, "INV-1").await, "PAID");
    assert_eq!(status_of(&state, "INV-2").await, "PAID");
    assert_eq!(mock.get_count(), 2, "one question each");
    assert_eq!(sweep(&state, PAYER, &schedule).await.expect("sweep"), 0, "PAID ends the checks");
    assert!(mock.posted().is_empty());
}

// How long swisha keeps asking is a decision about how long money sits unconfirmed, and how fast
// one number may ask is a decision about Swish's load, so the numbers themselves are pinned.
#[test]
fn the_check_schedule_is_pinned() {
    use std::time::Duration;
    use swisha::swish::reconcile::{BETWEEN_CHECKS, CHECK_TICK, CHECKS, PER_CLAIM};
    assert_eq!(CHECKS.first_after, Duration::from_secs(5 * 60));
    assert_eq!(CHECKS.every, Duration::from_secs(60 * 60));
    assert_eq!(CHECKS.window, Duration::from_secs(48 * 60 * 60));
    assert_eq!(CHECK_TICK, Duration::from_secs(5));
    assert_eq!(PER_CLAIM, 5);
    assert_eq!(BETWEEN_CHECKS, Duration::from_secs(1), "one question a second per number");
}

async fn status_of(state: &SharedState, reference: &str) -> String {
    let snapshot = state.store.snapshot(PAYER, reference).await.expect("store").expect("row");
    snapshot.status.unwrap_or_default()
}

async fn events_of(state: &SharedState, reference: &str) -> Vec<String> {
    sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT event FROM {} WHERE reference = $1 ORDER BY id",
        state.config.table_events
    )))
    .bind(reference)
    .fetch_all(state.store.pool())
    .await
    .expect("events")
}

fn announced_statuses(stream: &mut swisha::events::Subscription) -> Vec<String> {
    let mut statuses = Vec::new();
    while let Ok(event) = stream.try_recv() {
        if event.channel.ends_with(":updates") {
            let payload: serde_json::Value = serde_json::from_str(&event.payload).expect("json");
            statuses.push(payload["status"].as_str().unwrap_or_default().to_string());
        }
    }
    statuses
}

fn assert_nothing_announced(stream: &mut swisha::events::Subscription) {
    let statuses = announced_statuses(stream);
    assert!(statuses.is_empty(), "announced {statuses:?}");
}

// A connection that never completes holds a send for 5 seconds, TLS included, not for the 55 a
// whole request may take: the payout reaches the checks, PENDING, that much sooner.
#[tokio::test]
async fn a_connection_that_never_completes_gives_up_within_seconds() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((socket, _)) = listener.accept().await {
            held.push(socket);
        }
    });
    let client = swisha::swish::client::build_swish_client(&common::bundle(common::PAYER_NUMBER, "30")).expect("client");
    let started = std::time::Instant::now();
    let url = format!("https://{addr}/swish-cpcapi/api/v1/payouts");
    let sent = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        swisha::swish::client::swish_post(&url, &serde_json::json!({}), &client),
    )
    .await;
    assert!(matches!(sent, Ok(Err(_))), "it gave up on its own");
    assert!(started.elapsed() < std::time::Duration::from_secs(7), "{:?}", started.elapsed());
}
