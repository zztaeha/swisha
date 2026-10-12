#![cfg(feature = "http")]

// What a payout costs Swish and the database once Swish's callbacks bring its outcome: the
// follow-up asks only for what no callback has told swisha yet.

mod common;

use std::time::Duration;

use common::{MockSwish, PAYER, PHONE, SECRET, SSN};
use serde_json::{Value, json};
use swisha::state::SharedState;

struct Rig {
    internal: String,
    callback: String,
    state: SharedState,
    swish: MockSwish,
}

async fn rig(swish: MockSwish) -> Rig {
    let mut config = common::config();
    config.swish_base_url = swish.clone().start().await;
    let state = common::state_with(config).await;
    Rig {
        internal: common::serve(swisha::http::internal_router().with_state(state.clone())).await,
        callback: common::serve(swisha::http::callback_router().with_state(state.clone())).await,
        state,
        swish,
    }
}

// Pays, and answers the instruction's UUID.
async fn pay(r: &Rig, reference: &str) -> String {
    let response = reqwest::Client::new()
        .post(format!("{}/swish/payout", r.internal))
        .header("x-api-secret", SECRET)
        .header(common::KEY_HEADER, common::key(PAYER))
        .json(&json!({ "reference": reference, "payee_alias": PHONE, "payee_ssn": SSN, "amount": 100.0 }))
        .send()
        .await
        .expect("request");
    assert_eq!(response.status().as_u16(), 202);
    let body: Value = response.json().await.expect("json");
    body["swish_ref"].as_str().expect("uuid").to_string()
}

async fn callback(r: &Rig, reference: &str, uuid: &str, status: &str) {
    let echoed = common::callback_identifier(&r.state, PAYER, reference).await;
    let code = reqwest::Client::new()
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
        .as_u16();
    assert_eq!(code, 200);
}

async fn trail(r: &Rig, reference: &str) -> Vec<(String, Option<String>)> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT event, status FROM {} WHERE reference = $1 ORDER BY id",
        r.state.config.table_events
    )))
    .bind(reference)
    .fetch_all(r.state.store.pool())
    .await
    .expect("events")
}

fn row(event: &str, status: Option<&str>) -> (String, Option<String>) {
    (event.to_string(), status.map(str::to_string))
}

// Swish brought both steps before the follow-up's first question, so it asks nothing and records
// nothing a second time.
#[tokio::test]
async fn a_payout_its_callbacks_settled_is_not_asked_about_again() {
    let r = rig(MockSwish::new().accepts().resolves_to("PAID")).await;
    let uuid = pay(&r, "F-1").await;
    callback(&r, "F-1", &uuid, "DEBITED").await;
    callback(&r, "F-1", &uuid, "PAID").await;
    tokio::time::sleep(Duration::from_secs(3)).await;

    assert_eq!(r.swish.get_count(), 0, "a callback already told swisha");
    let expected = [
        row("INITIATED", None),
        row("ACCEPTED", None),
        row("CALLBACK", Some("DEBITED")),
        row("CALLBACK", Some("PAID")),
    ];
    assert_eq!(trail(&r, "F-1").await, expected);
}

// A callback brought DEBITED alone, so the follow-up still asks for the last word, and only for it.
#[tokio::test]
async fn after_a_callback_debited_the_follow_up_asks_only_for_paid() {
    let r = rig(MockSwish::new().accepts().resolves_to("PAID")).await;
    let uuid = pay(&r, "F-2").await;
    callback(&r, "F-2", &uuid, "DEBITED").await;
    tokio::time::sleep(Duration::from_secs(3)).await;

    assert_eq!(r.swish.get_count(), 1, "one question, for PAID");
    let expected = [
        row("INITIATED", None),
        row("ACCEPTED", None),
        row("CALLBACK", Some("DEBITED")),
        row("PAID", Some("PAID")),
    ];
    assert_eq!(trail(&r, "F-2").await, expected);
}

// What a listener in another process hears of one payout, from now until `quiet` passes with nothing
// more: each message's channel kind, event or status, and the backend that sent it.
async fn heard(
    listener: &mut sqlx::postgres::PgListener,
    reference: &str,
    quiet: Duration,
) -> Vec<(String, String, u32)> {
    let mut heard = Vec::new();
    while let Ok(Ok(note)) = tokio::time::timeout(quiet, listener.recv()).await {
        let body: Value = serde_json::from_str(note.payload()).expect("json");
        if body["reference"] != reference {
            continue;
        }
        let kind = note.channel().rsplit(':').next().unwrap_or_default().to_string();
        let field = |name: &str| body[name].as_str().unwrap_or_default().to_string();
        let what = match kind.as_str() {
            "events" => format!("{} {}", field("event"), field("status")),
            _ => field("status"),
        };
        heard.push((kind, what.trim().to_string(), note.process_id()));
    }
    heard
}

async fn listening(r: &Rig) -> sqlx::postgres::PgListener {
    let mut listener = sqlx::postgres::PgListener::connect_with(r.state.store.pool()).await.expect("listener");
    let channels = [swisha::events::channel_events(PAYER), swisha::events::channel_updates(PAYER)];
    listener.listen_all(channels.iter().map(String::as_str)).await.expect("listen");
    listener
}

// Each change Swish brings reaches other processes as its event, then the payout as it now stands,
// sent in one statement; the submission's events go out as they happen.
#[tokio::test]
async fn listeners_hear_each_change_with_the_payout_it_leaves() {
    let r = rig(MockSwish::new().accepts().resolves_to("PAID")).await;
    let mut listener = listening(&r).await;
    let uuid = pay(&r, "N-1").await;
    callback(&r, "N-1", &uuid, "DEBITED").await;
    callback(&r, "N-1", &uuid, "PAID").await;

    let heard = heard(&mut listener, "N-1", Duration::from_secs(2)).await;
    let said: Vec<(&str, &str)> = heard.iter().map(|(kind, what, _)| (kind.as_str(), what.as_str())).collect();
    assert_eq!(
        said,
        [
            ("events", "INITIATED"),
            ("events", "ACCEPTED"),
            ("events", "CALLBACK DEBITED"),
            ("updates", "DEBITED"),
            ("events", "CALLBACK PAID"),
            ("updates", "PAID"),
        ]
    );
    assert_eq!(heard[2].2, heard[3].2, "DEBITED and the payout it left, in one statement");
    assert_eq!(heard[4].2, heard[5].2, "PAID and the payout it left, in one statement");
}

// A write the store refused announces nothing new, and its event still reaches other processes.
#[tokio::test]
async fn a_refused_write_still_sends_its_event_alone() {
    let r = rig(MockSwish::new().accepts().resolves_to("PAID")).await;
    let uuid = pay(&r, "N-2").await;
    callback(&r, "N-2", &uuid, "PAID").await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let mut listener = listening(&r).await;

    let fields = swisha::events::SwishLogFields {
        payer:         PAYER.into(),
        reference:     "N-2".into(),
        swish_ref:     Some(uuid),
        status:        Some("ERROR".into()),
        amount:        None,
        payee_alias:   None,
        error_code:    None,
        error_message: None,
        ip:            None,
        actor:         None,
        note:          None,
    };
    let change = swisha::events::Change::Status("ERROR");
    let recorded = swisha::events::record("CHECKED", fields, change, &r.state).await.expect("recorded");
    assert!(!recorded.applied, "PAID is final");
    assert!(recorded.announce_if_applied(PAYER, "N-2", &r.state).await.is_none());

    let heard = heard(&mut listener, "N-2", Duration::from_secs(1)).await;
    let said: Vec<(&str, &str)> = heard.iter().map(|(kind, what, _)| (kind.as_str(), what.as_str())).collect();
    assert_eq!(said, [("events", "CHECKED ERROR")]);
}
