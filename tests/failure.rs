#![cfg(feature = "http")]

// What swisha does when its dependencies fail underneath it. Every test here asks the same
// question: after the failure, can this produce a second payment or a lost one?
//
// The database is killed by closing its pool, which makes every subsequent query fail the way a
// dead server does. Swish is killed by pointing the client at a port nothing listens on.

mod common;

use common::{MockSwish, PAYER, PHONE, SECRET, SSN};
use serde_json::{json, Value};
use swisha::domain::payout::NewPayout;
use swisha::state::SharedState;
use swisha::store::PayoutStore;

struct Rig {
    internal: String,
    callback: String,
    state: SharedState,
    pool: sqlx::PgPool,
    swish: MockSwish,
}

async fn rig_with(swish: MockSwish, reachable: bool) -> Rig {
    let url = if reachable {
        swish.clone().start().await
    } else {
        // Nothing listens here, so every call is refused immediately.
        "http://127.0.0.1:1".to_string()
    };
    let mut config = common::config();
    config.swish_base_url = url;
    let (state, pool) = common::state_and_pool(config).await;
    Rig {
        internal: common::serve(swisha::http::internal_router().with_state(state.clone())).await,
        callback: common::serve(swisha::http::callback_router().with_state(state.clone())).await,
        state,
        pool,
        swish,
    }
}

async fn rig() -> Rig {
    rig_with(MockSwish::new().accepts().resolves_to("PAID"), true).await
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
        .expect("swisha answered");
    let code = response.status().as_u16();
    (code, response.text().await.unwrap_or_default())
}

// The database dies

// Nothing may be sent to Swish that swisha cannot write down. A payout it cannot record is a
// payout it cannot later refuse to repeat.
#[tokio::test]
async fn a_dead_database_refuses_the_payout_before_swish_hears_about_it() {
    let r = rig().await;
    r.pool.close().await;

    let (code, _) = pay(&r, body("F-1")).await;
    assert_eq!(code, 503, "the caller is told, not left guessing");
    assert!(r.swish.posted().is_empty(), "no instruction may be sent without a record of it");
}

#[tokio::test]
async fn a_dead_database_does_not_invent_a_status() {
    let r = rig().await;
    pay(&r, body("F-2")).await;
    r.pool.close().await;

    let code = reqwest::Client::new()
        .get(format!("{}/swish/status/F-2", r.internal))
        .header("x-api-secret", SECRET)
        .header(common::KEY_HEADER, common::key(common::PAYER))
        .send()
        .await
        .expect("swisha answered")
        .status()
        .as_u16();
    assert_eq!(code, 503, "an unreadable payout is not a missing one, and not a settled one");
}

// A callback whose payout cannot be read must not be acknowledged as handled: a 200 tells Swish
// its word was taken when it was not.
#[tokio::test]
async fn a_callback_whose_payout_cannot_be_read_is_not_acknowledged() {
    let r = rig().await;
    pay(&r, body("F-3")).await;
    r.pool.close().await;

    let code = reqwest::Client::new()
        .post(format!("{}/swish/callback", r.callback))
        .json(&json!({
            "payerAlias": common::PAYER_NUMBER,
            "payerPaymentReference": "F-3",
            "payoutInstructionUUID": "00000000000000000000000000000000",
            "status": "PAID",
        }))
        .send()
        .await
        .expect("swisha answered")
        .status()
        .as_u16();
    assert_eq!(code, 500, "Swish must not be told this is done");
}

const UUID: &str = "AAAABBBBCCCCDDDDEEEEFFFF00009999";

// A payout Swish holds under UUID, with no callbackIdentifier to echo.
fn payout_for(reference: &str) -> NewPayout<'_> {
    NewPayout {
        payer: PAYER,
        reference,
        payer_alias: common::PAYER_NUMBER,
        payee_alias: PHONE,
        payee_ssn: Some(SSN),
        amount: 100.0,
        message: "failure",
        swish_ref: UUID,
        callback_identifier: None,
    }
}

async fn staged(r: &Rig, reference: &str) {
    r.state.store.claim(&payout_for(reference)).await.expect("claim");
}

async fn callback(r: &Rig, reference: &str, status: &str) -> u16 {
    reqwest::Client::new()
        .post(format!("{}/swish/callback", r.callback))
        .json(&json!({
            "payerAlias": common::PAYER_NUMBER,
            "payerPaymentReference": reference,
            "payoutInstructionUUID": UUID,
            "status": status,
        }))
        .send()
        .await
        .expect("swisha answered")
        .status()
        .as_u16()
}

async fn status_and_trail(r: &Rig, reference: &str) -> (String, Vec<String>) {
    let status = r.state.store.snapshot(PAYER, reference).await.expect("store").and_then(|s| s.status);
    let trail = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT event FROM {} WHERE reference = $1 ORDER BY id",
        r.state.config.table_events
    )))
    .bind(reference)
    .fetch_all(&r.pool)
    .await
    .expect("events");
    (status.unwrap_or_default(), trail)
}

// A callback the store refused to record is not acknowledged. Its status and event are one write:
// whichever the store refuses, neither lands, and the same callback sent again lands whole.
#[tokio::test]
async fn a_callback_the_store_refused_is_not_acknowledged() {
    let r = rig().await;
    let refusals = [
        (r.state.config.table_payouts.clone(), "status = 'DEBITED'"),
        (r.state.config.table_events.clone(), "event = 'CALLBACK'"),
    ];
    for (n, (table, rows)) in refusals.iter().enumerate() {
        let reference = format!("F-9-{n}");
        staged(&r, &reference).await;
        common::refuse(&r.state, table, rows).await;
        assert_eq!(callback(&r, &reference, "DEBITED").await, 500, "{rows}: not acknowledged");
        assert_eq!(status_and_trail(&r, &reference).await, ("CREATED".into(), vec![]), "{rows}");

        common::allow(&r.state, table).await;
        assert_eq!(callback(&r, &reference, "DEBITED").await, 200, "{rows}");
        let landed = ("DEBITED".to_string(), vec!["CALLBACK".to_string()]);
        assert_eq!(status_and_trail(&r, &reference).await, landed, "{rows}: sent again, it lands whole");
    }
}

// A number this instance has not loaded, as one another instance added moments ago, is looked up
// in the store. A lookup that fails is a store that could not be read, and is not acknowledged.
#[tokio::test]
async fn a_callback_whose_number_cannot_be_looked_up_is_not_acknowledged() {
    let r = rig().await;
    let (name, number) = ("late", "1239999999");
    let hash = swisha::payers::key_hash(&common::key(name)).expect("a test key");
    let row = swisha::domain::payer::NewPayer { name, nickname: name, number, added_by: "test", key_hash: &hash };
    r.state.store.add_payer(&row, &common::logged()).await.expect("add");
    r.state
        .store
        .claim(&NewPayout { payer: name, payer_alias: number, ..payout_for("F-17") })
        .await
        .expect("claim");
    let callback = || async {
        reqwest::Client::new()
            .post(format!("{}/swish/callback", r.callback))
            .json(&json!({
                "payerAlias": number,
                "payerPaymentReference": "F-17",
                "payoutInstructionUUID": UUID,
                "status": "PAID",
            }))
            .send()
            .await
            .expect("swisha answered")
            .status()
            .as_u16()
    };
    let payers = r.state.config.table_payers.clone();
    let rename = |from: &str, to: &str| format!("ALTER TABLE {from} RENAME TO {to}");
    let away = format!("{payers}_away");

    sqlx::query(sqlx::AssertSqlSafe(rename(&payers, &away))).execute(&r.pool).await.expect("rename");
    let unreadable = callback().await;
    sqlx::query(sqlx::AssertSqlSafe(rename(&away, &payers))).execute(&r.pool).await.expect("rename back");
    assert_eq!(unreadable, 500, "a number swisha could not look up is not one it ignores");
    let status = r.state.store.snapshot(name, "F-17").await.expect("store").and_then(|s| s.status);
    assert_eq!(status.as_deref(), Some("CREATED"));

    assert_eq!(callback().await, 200, "sent again, it lands");
    let status = r.state.store.snapshot(name, "F-17").await.expect("store").and_then(|s| s.status);
    assert_eq!(status.as_deref(), Some("PAID"));
}

// What Swish sends is recorded whatever its length. A status or code longer than the trail holds is
// cut to fit, rather than failing the write and, with it, the callback.
#[tokio::test]
async fn an_overlong_status_or_code_is_recorded_cut_to_fit() {
    let r = rig().await;
    staged(&r, "F-10").await;
    let (status, code) = ("REFUNDED_BY_THE_RECEIVING_BANK", "ABCDEFGHIJKLMNOPQRSTUVWXY");
    for (sent, error_code) in [(status, None), ("ERROR", Some(code))] {
        let answered = reqwest::Client::new()
            .post(format!("{}/swish/callback", r.callback))
            .json(&json!({
                "payerAlias": common::PAYER_NUMBER,
                "payerPaymentReference": "F-10",
                "payoutInstructionUUID": UUID,
                "status": sent,
                "errorCode": error_code,
            }))
            .send()
            .await
            .expect("swisha answered")
            .status()
            .as_u16();
        assert_eq!(answered, 200, "{sent}");
    }

    let view = r.state.store.status_view(PAYER, "F-10").await.expect("store").expect("row");
    assert_eq!((view.status.as_deref(), view.error_code.as_deref()), (Some("ERROR"), Some(&code[..20])));
    let trail: Vec<Option<String>> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT status FROM {} WHERE reference = 'F-10' ORDER BY id",
        r.state.config.table_events
    )))
    .fetch_all(&r.pool)
    .await
    .expect("events");
    assert_eq!(trail, [Some(status[..20].to_string()), Some("ERROR".to_string())]);
}

// A claim the store refused is read back before the answer: with nothing stored the reference is
// free, and held by an earlier request it is spent, a 409 at once rather than after a resend.
#[tokio::test]
async fn a_claim_the_store_refused_is_read_back_before_the_answer() {
    let r = rig().await;
    let payouts = r.state.config.table_payouts.clone();
    staged(&r, "F-12").await;
    common::refuse(&r.state, &payouts, "reference IN ('F-11', 'F-12')").await;

    let (code, answer) = pay(&r, body("F-11")).await;
    assert_eq!(code, 503, "{answer}");
    assert!(answer.contains("Nothing was stored"), "{answer}");
    let (code, answer) = pay(&r, body("F-12")).await;
    assert_eq!(code, 409, "stored earlier, so spent: {answer}");
    assert!(r.swish.posted().is_empty(), "neither may reach Swish");

    common::allow(&r.state, &payouts).await;
    assert_eq!(pay(&r, body("F-11")).await.0, 202, "free, as the 503 said");
}

// The claim and the trail's first event are one write: a payout whose INITIATED cannot be stored
// is not claimed either, so it never leaves without the record that it did.
#[tokio::test]
async fn a_payout_whose_trail_cannot_open_is_neither_claimed_nor_sent() {
    let r = rig().await;
    let events = r.state.config.table_events.clone();
    common::refuse(&r.state, &events, "event = 'INITIATED' AND reference = 'F-18'").await;

    let (code, answer) = pay(&r, body("F-18")).await;
    assert_eq!(code, 503, "{answer}");
    assert!(answer.contains("Nothing was stored"), "{answer}");
    assert!(r.swish.posted().is_empty(), "nothing may reach Swish");

    common::allow(&r.state, &events).await;
    assert_eq!(pay(&r, body("F-18")).await.0, 202, "free, as the 503 said");
    assert_eq!(status_and_trail(&r, "F-18").await.1.first().map(String::as_str), Some("INITIATED"));
}

// The submission's own record lands before the payout is answered, so a stop right after the 202
// cannot lose it. Held up by a lock on the trail, the payout waits for it.
#[tokio::test]
async fn the_trail_opens_before_the_payout_is_answered() {
    let r = rig().await;
    let mut lock = r.pool.begin().await.expect("begin");
    let events = r.state.config.table_events.clone();
    sqlx::query(sqlx::AssertSqlSafe(format!("LOCK TABLE {events} IN SHARE MODE")))
        .execute(&mut *lock)
        .await
        .expect("lock");

    let url = format!("{}/swish/payout", r.internal);
    let paying = tokio::spawn(async move {
        reqwest::Client::new()
            .post(url)
            .header("x-api-secret", SECRET)
            .header(common::KEY_HEADER, common::key(common::PAYER))
            .json(&body("F-13"))
            .send()
            .await
            .expect("swisha answered")
            .status()
            .as_u16()
    });
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    assert!(!paying.is_finished(), "answered before its INITIATED was recorded");

    lock.commit().await.expect("release");
    assert_eq!(paying.await.expect("join"), 202);
    assert_eq!(status_and_trail(&r, "F-13").await.1.first().map(String::as_str), Some("INITIATED"));
}

// Swish took the payout while every connection was busy, so the write of its 201 timed out. It is
// written once one frees, before the answer: a busy database does not cost the trail that row.
#[tokio::test]
async fn a_201_whose_record_timed_out_is_written_again() {
    let swish = MockSwish::new().accepts().answers_after(std::time::Duration::from_millis(800));
    let mut config = common::config();
    config.swish_base_url = swish.clone().start().await;
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .acquire_timeout(std::time::Duration::from_secs(1))
        .connect(&common::database_url())
        .await
        .expect("pool");
    common::apply_schema(&pool, &config).await;
    let ready = vec![common::payer_state(&common::payer(PAYER, common::PAYER_NUMBER))];
    let state = common::state_over(pool.clone(), config, ready).await;
    let internal = common::serve(swisha::http::internal_router().with_state(state.clone())).await;
    let r = Rig { callback: String::new(), internal, state, pool, swish };

    let request = reqwest::Client::new()
        .post(format!("{}/swish/payout", r.internal))
        .header("x-api-secret", SECRET)
        .header(common::KEY_HEADER, common::key(common::PAYER))
        .json(&body("F-19"))
        .send();
    let paying = tokio::spawn(async move { request.await.expect("swisha answered").status().as_u16() });

    // Once the payout is claimed, both connections are taken until Swish's 201 has come and the
    // write of it has given up waiting for one.
    let watcher = sqlx::PgPool::connect(&common::database_url()).await.expect("watcher");
    let claimed = format!("SELECT EXISTS (SELECT 1 FROM {} WHERE reference = 'F-19')", r.state.config.table_payouts);
    let is_claimed = || sqlx::query_scalar::<_, bool>(sqlx::AssertSqlSafe(claimed.clone())).fetch_one(&watcher);
    while !is_claimed().await.expect("watch") {
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    let held = (r.pool.acquire().await.expect("first"), r.pool.acquire().await.expect("second"));
    tokio::time::sleep(std::time::Duration::from_millis(3500)).await;
    assert_eq!(r.swish.posted().len(), 1, "Swish took it while the connections were held");
    drop(held);

    assert_eq!(paying.await.expect("join"), 202);
    let trail = status_and_trail(&r, "F-19").await.1;
    assert_eq!(trail[..trail.len().min(2)], ["INITIATED", "ACCEPTED"], "{trail:?}");
}

async fn post(r: &Rig, path: &str, body: Value) -> (u16, Value) {
    let response = reqwest::Client::new()
        .post(format!("{}{path}", r.internal))
        .header("x-api-secret", SECRET)
        .header(common::KEY_HEADER, common::key(common::PAYER))
        .json(&body)
        .send()
        .await
        .expect("swisha answered");
    let code = response.status().as_u16();
    (code, response.json().await.unwrap_or_default())
}

// A confirmation or a redo is one write with the record of who did it and why. When the store
// refuses that record, neither lands: no status moves and nothing is sent.
#[tokio::test]
async fn a_confirmation_or_redo_whose_record_is_refused_changes_nothing() {
    let r = rig_with(MockSwish::new().accepts(), true).await;
    let events = r.state.config.table_events.clone();
    staged(&r, "F-14").await;
    assert!(common::stage(&r.state, PAYER, "F-14", "NEEDS_REVIEW").await);
    let (confirm, redo) = (format!("/swish/payout/{PAYER}/F-14/confirm"), format!("/swish/payout/{PAYER}/F-14/redo"));

    common::refuse(&r.state, &events, "event = 'CONFIRMED_PAID'").await;
    let (code, answer) = post(&r, &confirm, json!({ "by": "Anna", "note": "bank" })).await;
    assert_eq!(code, 503, "{answer}");
    assert_eq!(status_and_trail(&r, "F-14").await, ("NEEDS_REVIEW".into(), vec![]));
    common::allow(&r.state, &events).await;

    // Its send ended unanswered, so Swish's having no record of it lets the redo on to its claim.
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "INSERT INTO {events} (payer, reference, event, status) VALUES ($1, 'F-14', 'PENDING', 'PENDING')"
    )))
    .bind(PAYER)
    .execute(&r.pool)
    .await
    .expect("the send's ending");
    common::refuse(&r.state, &events, "event = 'REDONE'").await;
    let (code, answer) = post(&r, &redo, json!({ "reference": "F-14-B", "by": "Anna" })).await;
    assert_eq!(code, 503, "{answer}");
    assert!(r.state.store.snapshot(PAYER, "F-14-B").await.expect("store").is_none(), "nothing claimed");
    assert!(r.swish.posted().is_empty(), "nothing sent");
    common::allow(&r.state, &events).await;

    let (code, answer) = post(&r, &confirm, json!({ "by": "Anna", "note": "bank" })).await;
    assert_eq!(code, 200, "{answer}");
    assert_eq!(answer["manual_confirmation"]["by"], "Anna", "it lands whole: {answer}");
}

// A confirmation that committed is answered as done, even when reading the payout back fails. A
// 503 would say "try again", and the retry would be told the payout is already paid.
#[tokio::test]
async fn a_committed_confirmation_is_answered_as_done() {
    let r = rig_with(MockSwish::new().accepts(), true).await;
    staged(&r, "F-15").await;
    assert!(common::stage(&r.state, PAYER, "F-15", "NEEDS_REVIEW").await);
    let payouts = r.state.config.table_payouts.clone();
    let rename = |from: &str, to: &str| format!("ALTER TABLE {payouts} RENAME COLUMN {from} TO {to}");
    sqlx::query(sqlx::AssertSqlSafe(rename("swish_ref", "unreadable"))).execute(&r.pool).await.expect("rename");

    let confirm = format!("/swish/payout/{PAYER}/F-15/confirm");
    let (code, answer) = post(&r, &confirm, json!({ "by": "Anna", "note": "bank" })).await;
    sqlx::query(sqlx::AssertSqlSafe(rename("unreadable", "swish_ref"))).execute(&r.pool).await.expect("rename back");
    assert_eq!(code, 200, "{answer}");
    assert_eq!(answer["status"], "PAID", "{answer}");
    assert_eq!(status_and_trail(&r, "F-15").await, ("PAID".into(), vec!["CONFIRMED_PAID".into()]));
}

// A redo goes out only on Swish's word that nothing was paid. With Swish unreachable there is
// none, so nothing is redone.
#[tokio::test]
async fn a_redo_is_never_sent_while_swish_cannot_be_reached() {
    let r = rig_with(MockSwish::new(), false).await;
    staged(&r, "F-16").await;
    assert!(common::stage(&r.state, PAYER, "F-16", "ERROR").await);
    let (code, answer) = post(&r, &format!("/swish/payout/{PAYER}/F-16/redo"), json!({ "reference": "F-16-B", "by": "Anna" })).await;
    assert_eq!(code, 503, "{answer}");
    assert!(answer["error"].as_str().unwrap_or_default().contains("nothing was redone"), "{answer}");
    assert!(r.state.store.snapshot(PAYER, "F-16-B").await.expect("store").is_none(), "nothing claimed");
}

// A check that cannot read the payout decides nothing, even its last: the database still takes
// writes, but a payout swisha could not look at is never sent to review or ended blind.
#[tokio::test]
async fn reconcile_decides_nothing_about_a_payout_it_could_not_read() {
    use swisha::swish::reconcile::reconcile;
    let r = rig().await;
    staged(&r, "F-4").await;
    let payouts = r.state.config.table_payouts.clone();
    let rename = |from: &str, to: &str| format!("ALTER TABLE {payouts} RENAME COLUMN {from} TO {to}");
    sqlx::query(sqlx::AssertSqlSafe(rename("swish_ref", "unreadable"))).execute(&r.pool).await.expect("rename");
    reconcile(PAYER.into(), "F-4".into(), true, r.state.clone()).await;
    sqlx::query(sqlx::AssertSqlSafe(rename("unreadable", "swish_ref"))).execute(&r.pool).await.expect("rename back");

    assert_eq!(status_and_trail(&r, "F-4").await, ("CREATED".to_string(), Vec::new()));
}

#[tokio::test]
async fn the_sweep_survives_an_unreadable_store() {
    let r = rig().await;
    r.pool.close().await;
    let swept = swisha::swish::reconcile::sweep(&r.state, PAYER, &swisha::swish::reconcile::CHECKS).await;
    assert!(swept.is_err(), "the sweep reports the failure rather than treating it as nothing to do");
}

#[tokio::test]
async fn swisha_keeps_answering_after_a_failed_request() {
    let r = rig().await;
    r.pool.close().await;
    for i in 0..5 {
        let (code, _) = pay(&r, body(&format!("F-5-{i}"))).await;
        assert_eq!(code, 503, "request {i}");
    }
    // The process is still up and still refusing cleanly rather than having fallen over, and says
    // why.
    let health: Value = reqwest::Client::new()
        .get(format!("{}/system/health", r.internal))
        .header("x-api-secret", SECRET)
        .send()
        .await
        .expect("swisha is still serving")
        .json()
        .await
        .expect("json");
    assert_eq!(health["db"], false, "{health}");
}

// Swish fails

// Swish may or may not hold a payout it could not be reached about, so there is no answer yet:
// PENDING, which the sweep asks about, never ERROR, which says Swish refused it.
#[tokio::test]
async fn an_unreachable_swish_leaves_the_payout_without_an_answer() {
    let r = rig_with(MockSwish::new(), false).await;
    let (code, _) = pay(&r, body("S-1")).await;
    assert_eq!(code, 503);

    let snapshot = r.state.store.snapshot(PAYER, "S-1").await.expect("store").expect("row");
    assert_eq!(snapshot.status.as_deref(), Some("PENDING"));
}

// The dangerous shape: Swish took the instruction and swisha could not confirm what became of it.
// The money may have moved, so nothing is written or announced, and the sweep asks again later.
#[tokio::test]
async fn an_accepted_but_unconfirmable_payout_is_never_resubmitted() {
    // Accepts the POST, then has no record of the UUID on the GET.
    let r = rig_with(MockSwish::new().accepts(), true).await;
    let mut updates = r.state.stream.subscribe(PAYER).expect("room for a stream");
    assert_eq!(pay(&r, body("S-2")).await.0, 202, "Swish accepted it");
    tokio::time::sleep(std::time::Duration::from_secs(17)).await;

    let snapshot = r.state.store.snapshot(PAYER, "S-2").await.expect("store").expect("row");
    assert_eq!(snapshot.status.as_deref(), Some("CREATED"), "swisha learned nothing to write");
    while let Ok(event) = updates.try_recv() {
        assert!(
            !event.channel.ends_with(":updates"),
            "an accepted payout must not be announced as anything: {}",
            event.payload
        );
    }

    // One instruction was sent; one is all there is.
    for _ in 0..3 {
        assert_eq!(pay(&r, body("S-2")).await.0, 409, "an unconfirmed payout is not permission to pay again");
    }
    assert_eq!(r.swish.posted().len(), 1, "a second instruction would be a second debit");
}

#[tokio::test]
async fn a_swish_server_error_is_not_treated_as_acceptance() {
    let r = rig_with(MockSwish::new().rejects(500, "upstream exploded"), true).await;
    let (code, _) = pay(&r, body("S-3")).await;
    assert_eq!(code, 503);

    let snapshot = r.state.store.snapshot(PAYER, "S-3").await.expect("store").expect("row");
    assert_eq!(snapshot.status.as_deref(), Some("PENDING"), "Swish may hold it: no answer yet");
    assert_eq!(r.swish.posted().len(), 1, "it was attempted exactly once");
}

// Swish refusing with 429 stored nothing, so the payout is DECLINED rather than unknown. The
// reference still stays spent: a new attempt goes out under a new one.
#[tokio::test]
async fn swish_rate_limiting_us_does_not_lose_the_payout() {
    let r = rig_with(MockSwish::new().rejects(429, "Too Many Requests"), true).await;
    assert_eq!(pay(&r, body("S-4")).await.0, 502);

    let snapshot = r.state.store.snapshot(PAYER, "S-4").await.expect("store").expect("row");
    assert_eq!(snapshot.status.as_deref(), Some("DECLINED"), "the attempt is recorded");
    assert_eq!(pay(&r, body("S-4")).await.0, 409, "and the reference stays spent");
}

// The process dies

// A crash between claiming the reference and sending the instruction leaves a row whose UUID
// Swish has never seen. Recovery must not assume either way: it asks, and when Swish has no
// record it hands the payout to a person rather than paying it.
#[tokio::test]
async fn a_crash_between_claim_and_submit_never_pays_twice() {
    use swisha::swish::reconcile::reconcile;
    let r = rig_with(MockSwish::new(), true).await;

    // What the process had written the instant before it died.
    r.state
        .store
        .claim(&NewPayout {
            payer: PAYER,
            payer_alias: common::PAYER_NUMBER,
            reference: "C-1",
            payee_alias: PHONE,
            payee_ssn: Some(SSN),
            amount: 100.0,
            message: "crash",
            swish_ref: "AAAABBBBCCCCDDDDEEEEFFFF00001111",
            callback_identifier: None,
        })
        .await
        .expect("claim");

    // Recovery, as the sweep would run it after a restart, through to the last check.
    for last in [false, false, true] {
        reconcile(PAYER.into(), "C-1".into(), last, r.state.clone()).await;
    }

    let snapshot = r.state.store.snapshot(PAYER, "C-1").await.expect("store").expect("row");
    assert_eq!(snapshot.status.as_deref(), Some("NEEDS_REVIEW"), "a person decides, not swisha");
    assert!(r.swish.posted().is_empty(), "recovery must never send an instruction");
}

// The reference survives the crash, so the ERP retrying the same one is refused rather than
// producing a second payout for the same invoice.
#[tokio::test]
async fn a_reference_claimed_before_a_crash_stays_claimed_after_it() {
    let r = rig().await;
    r.state
        .store
        .claim(&NewPayout {
            payer: PAYER,
            payer_alias: common::PAYER_NUMBER,
            reference: "C-2",
            payee_alias: PHONE,
            payee_ssn: Some(SSN),
            amount: 100.0,
            message: "crash",
            swish_ref: "AAAABBBBCCCCDDDDEEEEFFFF00002222",
            callback_identifier: None,
        })
        .await
        .expect("claim");

    // The process that made the claim is gone: a new one starts over the same database.
    let payers = vec![common::payer_state(&common::payer(PAYER, common::PAYER_NUMBER))];
    let restarted = common::state_over(r.pool.clone(), r.state.config.clone(), payers).await;
    let after = Rig {
        internal: common::serve(swisha::http::internal_router().with_state(restarted.clone())).await,
        callback: common::serve(swisha::http::callback_router().with_state(restarted.clone())).await,
        state: restarted,
        pool: r.pool.clone(),
        swish: r.swish.clone(),
    };
    let (code, _) = pay(&after, body("C-2")).await;
    assert_eq!(code, 409, "the claim outlived the process that made it");
    assert!(r.swish.posted().is_empty());
}

// A health check that calls a dead database reachable is worse than not having one: a probe
// reading "ok" keeps routing traffic to an instance that cannot pay anybody. The database is
// pinged live for exactly this reason, so the answer can never be stale.
#[tokio::test]
async fn health_reports_a_dead_database_immediately() {
    let r = rig().await;

    let healthy: Value = reqwest::Client::new()
        .get(format!("{}/system/health", r.internal))
        .header("x-api-secret", SECRET)
        .send()
        .await
        .expect("request")
        .json()
        .await
        .expect("json");
    assert_eq!(healthy["db"], true);
    assert_eq!(healthy["status"], "ok");

    r.pool.close().await;

    let dead: Value = reqwest::Client::new()
        .get(format!("{}/system/health", r.internal))
        .header("x-api-secret", SECRET)
        .send()
        .await
        .expect("request")
        .json()
        .await
        .expect("json");
    assert_eq!(dead["db"], false, "the database is down and health must say so");
    assert_eq!(dead["status"], "degraded");
}

// The Swish half is cached, so it has to say how old it is. An unknown answer is reported as
// unknown rather than guessed at, which is what a probe sees during the first moments of a boot.
#[tokio::test]
async fn health_says_how_stale_its_swish_answer_is() {
    let r = rig().await;
    let seen: Value = reqwest::Client::new()
        .get(format!("{}/system/health", r.internal))
        .header("x-api-secret", SECRET)
        .send()
        .await
        .expect("request")
        .json()
        .await
        .expect("json");

    // Nothing has probed Swish in this harness, so both fields are null rather than false.
    assert!(seen["swish_online"].is_null(), "not asked yet is not the same as unreachable");
    assert!(seen["swish_checked_seconds_ago"].is_null());
    assert_eq!(seen["status"], "ok", "an unknown Swish must not read as degraded");
}

// The database's answer is lost

// A network that loses one answer: relays PostgreSQL both ways and, once armed, swallows the
// server's reply to the next COMMIT and closes the connection. The COMMIT itself goes through.
struct LossyLink {
    url: String,
    armed: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

async fn lossy_link() -> LossyLink {
    use std::sync::atomic::AtomicBool;
    let direct = reqwest::Url::parse(&common::database_url()).expect("the test database URL");
    let upstream = format!("{}:{}", direct.host_str().expect("host"), direct.port().unwrap_or(5432));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let mut url = direct.clone();
    url.set_host(Some("127.0.0.1")).expect("host");
    url.set_port(Some(listener.local_addr().expect("addr").port())).expect("port");
    url.set_query(Some("sslmode=disable"));
    let armed = std::sync::Arc::new(AtomicBool::new(false));
    let trigger = armed.clone();
    tokio::spawn(async move {
        while let Ok((client, _)) = listener.accept().await {
            let server = tokio::net::TcpStream::connect(&upstream).await.expect("the test database");
            tokio::spawn(relay(client, server, trigger.clone()));
        }
    });
    LossyLink { url: url.to_string(), armed }
}

async fn relay(
    client: tokio::net::TcpStream,
    server: tokio::net::TcpStream,
    armed: std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let ((mut from_client, mut to_client), (mut from_server, mut to_server)) = (client.into_split(), server.into_split());
    let upstream = tokio::spawn(async move { tokio::io::copy(&mut from_client, &mut to_server).await });
    let (mut pending, mut chunk) = (Vec::new(), [0u8; 8192]);
    while let Ok(read @ 1..) = from_server.read(&mut chunk).await {
        pending.extend_from_slice(&chunk[..read]);
        // Whole messages only: a type byte, then a length that counts itself and the body.
        while pending.len() >= 5 {
            let total = 1 + u32::from_be_bytes([pending[1], pending[2], pending[3], pending[4]]) as usize;
            if pending.len() < total {
                break;
            }
            let message: Vec<u8> = pending.drain(..total).collect();
            let commit = message[0] == b'C' && message[5..].starts_with(b"COMMIT\0");
            if (commit && armed.swap(false, std::sync::atomic::Ordering::SeqCst))
                || to_client.write_all(&message).await.is_err()
            {
                upstream.abort();
                return;
            }
        }
    }
    upstream.abort();
}

// A rig whose swisha reaches the database through the lossy link. Its tables are made directly.
async fn lossy_rig(swish: MockSwish, payers: Vec<swisha::payers::Payer>) -> (Rig, LossyLink) {
    let mut config = common::config();
    config.swish_base_url = swish.clone().start().await;
    let pool = common::pool_with_schema(&config).await;
    let link = lossy_link().await;
    let linked = sqlx::postgres::PgPoolOptions::new().max_connections(4).connect(&link.url).await.expect("connect");
    let state = common::state_over(linked, config, payers.iter().map(common::payer_state).collect()).await;
    let rig = Rig {
        internal: common::serve(swisha::http::internal_router().with_state(state.clone())).await,
        callback: common::serve(swisha::http::callback_router().with_state(state.clone())).await,
        state,
        pool,
        swish,
    };
    (rig, link)
}

// A redo whose COMMIT went through but whose answer was lost is the redo this request claimed. Asked
// again with its instruction, the store finds it, and it is sent once, never answered as unstored.
#[tokio::test]
async fn a_redo_whose_commit_answer_was_lost_is_sent_once() {
    let (r, link) = lossy_rig(MockSwish::new().accepts(), common::payers()).await;
    staged(&r, "L-1").await;
    assert!(common::stage(&r.state, PAYER, "L-1", "ERROR").await);
    r.swish.clone().resolves_for(UUID, "ERROR");

    link.armed.store(true, std::sync::atomic::Ordering::SeqCst);
    let (code, answer) = post(&r, &format!("/swish/payout/{PAYER}/L-1/redo"), json!({ "reference": "L-1-B", "by": "Anna" })).await;
    assert!(!link.armed.load(std::sync::atomic::Ordering::SeqCst), "the redo's COMMIT answer was lost");
    assert_eq!(code, 202, "{answer}");
    assert_eq!(r.swish.posted().len(), 1, "sent once");
    let (_, trail) = status_and_trail(&r, "L-1").await;
    assert_eq!(trail.iter().filter(|event| *event == "REDONE").count(), 1, "{trail:?}");
}

// An add whose COMMIT went through but whose answer was lost stored the number. It is found by its
// new key, its files are put in place and its key is answered, so the number can be used at once.
#[tokio::test]
async fn an_add_whose_commit_answer_was_lost_answers_its_key() {
    let (r, link) = lossy_rig(MockSwish::new().accepts(), Vec::new()).await;
    let number = "1234679304";
    let body = json!({
        "name": "sthlm", "nickname": "sthlm", "number": number,
        "tls": common::bundle(number, "30"), "signing": common::bundle(number, "31"), "by": "Anna",
    });

    link.armed.store(true, std::sync::atomic::Ordering::SeqCst);
    let response = reqwest::Client::new()
        .post(format!("{}/swish/payers", r.internal))
        .header("x-api-secret", SECRET)
        .json(&body)
        .send()
        .await
        .expect("swisha answered");
    assert!(!link.armed.load(std::sync::atomic::Ordering::SeqCst), "the add's COMMIT answer was lost");
    let code = response.status().as_u16();
    let answer: Value = response.json().await.unwrap_or_default();
    assert_eq!(code, 201, "{answer}");
    let key = answer["key"].as_str().expect("the key");

    let paid = reqwest::Client::new()
        .post(format!("{}/swish/payout", r.internal))
        .header("x-api-secret", SECRET)
        .header(common::KEY_HEADER, key)
        .json(&body_for("sthlm", "L-2"))
        .send()
        .await
        .expect("swisha answered")
        .status()
        .as_u16();
    assert_eq!(paid, 202, "the number pays with the key it was answered");
}

fn body_for(payer: &str, reference: &str) -> Value {
    json!({ "payer": payer, "reference": reference, "payee_alias": PHONE, "payee_ssn": SSN, "amount": 100.0 })
}
