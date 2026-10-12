#![cfg(feature = "http")]

// Health says whose problem an outage is: what swisha sees of Swish beside what its status page
// says, and how many numbers are in trouble. Never which: that is behind each number's key.

mod common;

use std::sync::{Arc, Mutex};

use common::SECRET;
use serde_json::{Value, json};
use swisha::state::SharedState;
use swisha::swish::status_page::read;

const SUMMARY: &str = include_str!("fixtures/swish-status-summary.json");

fn summary() -> Value {
    serde_json::from_str(SUMMARY).expect("fixture")
}

// The real page, as Swish served it on 2026-10-07: payouts operational, and a Swish-wide
// maintenance on 3 November that includes them. Bank maintenance before it does not count.
#[test]
fn the_real_page_reads_as_operational_with_the_november_window_next() {
    let status = read(&summary());
    assert_eq!(status.payout.as_deref(), Some("operational"));
    let next = status.next_maintenance.expect("a payout maintenance is announced");
    assert_eq!(next.name, "Planned maintenance at Swish");
    assert_eq!(next.from, "2026-11-03T02:00:00.000+01:00");
}

fn page(payout: &str, maintenances: Value) -> Value {
    json!({
        "components": [
            { "id": "x1", "name": "Commerce", "status": "operational" },
            { "id": "67rlmr7vkql2", "name": "Payout", "status": payout },
        ],
        "scheduled_maintenances": maintenances,
    })
}

fn window(name: &str, status: &str, from: &str, components: Value) -> Value {
    json!({ "name": name, "status": status, "scheduled_for": from, "scheduled_until": from, "components": components })
}

// Across a daylight saving change the text of two times sorts differently from the instants
// they name: 02:30+02:00 is earlier than 02:00+01:00. Windows are ordered by instant.
#[test]
fn the_earliest_unfinished_payout_window_wins() {
    let payout = json!([{ "id": "67rlmr7vkql2", "name": "Payout" }]);
    let status = read(&page("operational", json!([
        window("later", "scheduled", "2026-10-25T02:00:00.000+01:00", payout.clone()),
        window("earlier", "scheduled", "2026-10-25T02:30:00.000+02:00", payout.clone()),
        window("finished", "completed", "2026-10-01T02:00:00.000+02:00", payout.clone()),
        window("a bank", "scheduled", "2026-10-02T02:00:00.000+02:00", json!([{ "id": "b1", "name": "Nordea" }])),
    ])));
    assert_eq!(status.next_maintenance.expect("window").name, "earlier");
}

#[test]
fn the_component_is_found_by_name_if_its_id_ever_changes() {
    let mut p = page("under_maintenance", json!([]));
    p["components"][1]["id"] = json!("new-id");
    assert_eq!(read(&p).payout.as_deref(), Some("under_maintenance"));

    // A component group that happens to be called Payout is a heading, not the component.
    let group = json!({ "components": [{ "id": "g", "name": "Payout", "status": "major_outage", "group": true }] });
    assert_eq!(read(&group).payout, None);
}

// A status page standing in for Swish's, serving whatever the test last set.
async fn status_page(body: Arc<Mutex<Value>>) -> String {
    use axum::{Json, Router, extract::State, routing::get};
    async fn summary(State(body): State<Arc<Mutex<Value>>>) -> Json<Value> {
        Json(body.lock().unwrap().clone())
    }
    let router = Router::new().route("/api/v2/summary.json", get(summary)).with_state(body);
    format!("{}/api/v2/summary.json", common::serve(router).await)
}

async fn health(base: &str) -> Value {
    reqwest::Client::new()
        .get(format!("{base}/system/health"))
        .header("x-api-secret", SECRET)
        .send()
        .await
        .expect("request")
        .json()
        .await
        .expect("json")
}

// One number as its own key reads it.
async fn number(base: &str, name: &str) -> Value {
    reqwest::Client::new()
        .get(format!("{base}/swish/payers/{name}"))
        .header("x-api-secret", SECRET)
        .header(common::KEY_HEADER, common::key(name))
        .send()
        .await
        .expect("request")
        .json()
        .await
        .expect("json")
}

async fn app(status_url: &str, payers: Vec<swisha::payers::Payer>) -> (String, SharedState) {
    let mut config = common::config();
    config.swish_status_url = status_url.to_string();
    let state = common::state_with_payers(config, payers).await;
    let base = common::serve(swisha::http::internal_router().with_state(state.clone())).await;
    (base, state)
}

#[tokio::test]
async fn health_reports_what_swish_says_about_payouts() {
    let body = Arc::new(Mutex::new(summary()));
    let url = status_page(body.clone()).await;
    let (base, state) = app(&url, common::payers()).await;

    swisha::swish::status_page::refresh(&state).await;
    let seen = health(&base).await;
    assert_eq!(seen["swish_payout_status"], "operational");
    assert_eq!(seen["swish_next_maintenance"]["from"], "2026-11-03T02:00:00.000+01:00");
    assert_eq!(seen["status"], "ok", "an announced window is information, not trouble");

    // Swish says payouts are down: degraded, and the reason is visible beside it.
    body.lock().unwrap()["components"]
        .as_array_mut()
        .expect("components")
        .iter_mut()
        .filter(|c| c["name"] == "Payout")
        .for_each(|c| c["status"] = json!("under_maintenance"));
    swisha::swish::status_page::refresh(&state).await;
    let seen = health(&base).await;
    assert_eq!(seen["swish_payout_status"], "under_maintenance");
    assert_eq!(seen["status"], "degraded");
}

// A status page that cannot be read says nothing about Swish. Unknown is not a failure, or every
// blip on a third-party page would mark swisha degraded.
#[tokio::test]
async fn an_unreadable_status_page_is_unknown_rather_than_degraded() {
    let (base, state) = app("http://127.0.0.1:1/api/v2/summary.json", common::payers()).await;
    swisha::swish::status_page::refresh(&state).await;
    let seen = health(&base).await;
    assert!(seen["swish_payout_status"].is_null());
    assert!(seen["swish_next_maintenance"].is_null());
    assert_eq!(seen["status"], "ok");
}

// With no number taking payouts nothing has asked Swish, so health cannot say it answers.
#[tokio::test]
async fn with_no_number_to_ask_swish_is_unknown() {
    let (base, _) = app("http://127.0.0.1:1/api/v2/summary.json", Vec::new()).await;
    let seen = health(&base).await;
    assert!(seen["swish_online"].is_null(), "{seen}");
    assert_eq!(seen["status"], "ok", "unknown is not trouble");
}

// One copy asks Swish about one number a second, so after a start the last of 10,000 numbers is
// first asked hours later. Until then the numbers not yet asked say nothing either way.
#[tokio::test]
async fn swish_is_online_once_one_number_answers_and_none_has_failed() {
    let payers = vec![common::payer("one", "1231111111"), common::payer("two", "1232222222")];
    let (base, state) = app("http://127.0.0.1:1/api/v2/summary.json", payers).await;
    let answered = swisha::state::SwishProbe { reachable: true, at: std::time::Instant::now() };
    *state.payer("one").expect("loaded").probe.write().await = Some(answered);

    let seen = health(&base).await;
    assert_eq!(seen["swish_online"], true, "{seen}");
    assert_eq!(seen["status"], "ok");

    // The newest answer says when Swish was last heard from: numbers are asked one at a time.
    let earlier = swisha::state::SwishProbe { reachable: true, at: std::time::Instant::now() - std::time::Duration::from_secs(300) };
    *state.payer("two").expect("loaded").probe.write().await = Some(earlier);
    assert_eq!(health(&base).await["swish_checked_seconds_ago"], 0);

    let failed = swisha::state::SwishProbe { reachable: false, at: std::time::Instant::now() };
    *state.payer("two").expect("loaded").probe.write().await = Some(failed);
    let seen = health(&base).await;
    assert_eq!(seen["swish_online"], false, "{seen}");
    assert_eq!(seen["payers"]["offline"], 1);
    assert_eq!(seen["status"], "degraded");
}

// Swish answering 401 or 403 refuses the number's certificate or the number itself, so the number
// does not reach Swish, whether a probe heard it or a payout did.
#[tokio::test]
async fn a_swish_refusing_the_certificate_reads_offline() {
    for code in [401, 403] {
        let swish = common::MockSwish::new().rejects(code, "").answers_gets_with(code);
        let mut config = common::config();
        config.swish_base_url = swish.start().await;
        config.swish_status_url = "http://127.0.0.1:1/api/v2/summary.json".into();
        let state = common::state_with(config).await;
        let base = common::serve(swisha::http::internal_router().with_state(state.clone())).await;
        let payer = state.payer(common::PAYER).expect("loaded");
        let probed = swisha::swish::client::probe(&payer.client, &state.config.swish_base_url).await;
        assert!(!probed.reachable, "{code}");

        let (phone, ssn) = (common::PHONE, common::SSN);
        let paid = reqwest::Client::new()
            .post(format!("{base}/swish/payout"))
            .header("x-api-secret", SECRET)
            .header(common::KEY_HEADER, common::key(common::PAYER))
            .json(&json!({ "reference": "R-1", "payee_alias": phone, "payee_ssn": ssn, "amount": 100.0 }))
            .send()
            .await
            .expect("request");
        assert_eq!(paid.json::<Value>().await.expect("json")["status"], "DECLINED", "{code}");
        let seen = health(&base).await;
        assert_eq!(seen["payers"]["offline"], 1, "{code}: {seen}");
        assert_eq!(seen["status"], "degraded", "{code}: {seen}");
        assert_eq!(number(&base, common::PAYER).await["swish_online"], false, "{code}");
    }
}

// A certificate that expires stops its number outright, so health warns a month ahead. It counts
// the number, and the number's own key reads which certificate.
#[tokio::test]
async fn a_certificate_close_to_expiry_is_reported_and_degrades_health() {
    let now = swisha::util::time::unix_now();
    let mut payer = common::payer(common::PAYER, common::PAYER_NUMBER);
    payer.tls.expires = now + 10 * 86_400 + 3_600;
    payer.signing.expires = now + 400 * 86_400 + 3_600;
    let (base, _) = app("http://127.0.0.1:1/api/v2/summary.json", vec![payer]).await;

    let seen = health(&base).await;
    assert_eq!(seen["payers"], json!({ "stored": 1, "disabled": 0, "unusable": 0, "offline": 0, "expiring": 1, "waiting": 0 }));
    assert_eq!(seen["status"], "degraded");
    assert!(!seen.to_string().contains(common::PAYER_NUMBER), "health names no number: {seen}");

    let own = number(&base, common::PAYER).await;
    assert_eq!(own["tls_expires_in_days"], 10);
    assert_eq!(own["signing_expires_in_days"], 400);
}

#[tokio::test]
async fn certificates_with_time_left_keep_health_ok() {
    let now = swisha::util::time::unix_now();
    let mut payer = common::payer(common::PAYER, common::PAYER_NUMBER);
    payer.tls.expires = now + 31 * 86_400 + 3_600;
    payer.signing.expires = now + 31 * 86_400 + 3_600;
    let (base, _) = app("http://127.0.0.1:1/api/v2/summary.json", vec![payer]).await;
    let seen = health(&base).await;
    assert_eq!(seen["payers"]["expiring"], 0);
    assert_eq!(seen["status"], "ok");
    assert_eq!(number(&base, common::PAYER).await["tls_expires_in_days"], 31);
}

// Against Swish's live page, when asked for: SWISHA_TEST_LIVE_STATUS=1 cargo test --test health
#[tokio::test]
async fn the_live_status_page_reads() {
    if std::env::var("SWISHA_TEST_LIVE_STATUS").is_err() {
        eprintln!("skipped: SWISHA_TEST_LIVE_STATUS is not set");
        return;
    }
    let status = swisha::swish::status_page::fetch(swisha::swish::status_page::SUMMARY_URL)
        .await
        .expect("Swish's status page answers and parses");
    eprintln!("   Payout: {:?}, next window: {:?}", status.payout, status.next_maintenance);
    assert!(status.payout.is_some(), "the page lists the Payout component");
}

// A number whose payouts are due a check, but which this instance cannot load, leaves them waiting
// on its certificates. Health says so rather than stay ok, and confirm says what they wait for.
#[tokio::test]
async fn payouts_no_one_here_can_ask_about_degrade_health() {
    use swisha::domain::payout::{CheckSchedule, NewPayout};
    use swisha::store::PayoutStore;
    let (base, state) = app("http://127.0.0.1:1/api/v2/summary.json", Vec::new()).await;
    let hash = swisha::payers::key_hash(&common::key("gone")).expect("a test key");
    let row = swisha::domain::payer::NewPayer {
        name: "gone",
        nickname: "gone",
        number: "1239990011",
        added_by: "Anna",
        key_hash: &hash,
    };
    state.store.add_payer(&row, &common::logged()).await.expect("stored, never loaded here");
    let payout = NewPayout {
        payer: "gone",
        reference: "W-1",
        payer_alias: "1239990011",
        payee_alias: "46701234567",
        payee_ssn: Some(common::SSN),
        amount: 1.0,
        message: "W-1",
        swish_ref: "AAAABBBBCCCCDDDDEEEEFFFF0000CCCC",
        callback_identifier: None,
    };
    state.store.claim(&payout).await.expect("claim");
    assert!(common::stage(&state, "gone", "W-1", "DEBITED").await);
    assert_eq!(health(&base).await["payers"]["waiting"], 0, "nothing has looked for due checks yet");

    let schedule = CheckSchedule {
        first_after: std::time::Duration::from_millis(50),
        every: std::time::Duration::from_secs(3600),
        window: std::time::Duration::from_secs(3600),
    };
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    tokio::spawn(swisha::swish::reconcile::run(state.clone(), schedule));
    let since = std::time::Instant::now();
    let seen = loop {
        let seen = health(&base).await;
        if seen["payers"]["waiting"] == 1 {
            break seen;
        }
        assert!(since.elapsed() < std::time::Duration::from_secs(3), "never counted: {seen}");
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    };
    assert_eq!(seen["status"], "degraded", "{seen}");

    let refused: Value = reqwest::Client::new()
        .post(format!("{base}/swish/payout/gone/W-1/confirm"))
        .header("x-api-secret", SECRET)
        .header(common::KEY_HEADER, common::key("gone"))
        .json(&json!({ "by": "Anna", "note": "bank" }))
        .send()
        .await
        .expect("request")
        .json()
        .await
        .expect("json");
    let error = refused["error"].as_str().unwrap_or_default();
    assert!(!error.contains("48 hours"), "no check is coming while it cannot load: {error}");
    assert!(error.contains("certificates"), "{error}");
}

// A payout Swish reported paid after a person redid it may have paid its recipient twice. Health
// counts them, and never degrades on it: swisha cannot know one has been dealt with, and a count
// that only grows would keep health degraded for good. Monitoring alerts when it grows.
#[tokio::test]
async fn health_counts_payouts_paid_after_they_were_redone() {
    use swisha::domain::payout::{EventRecord, NewPayout, NewRedo};
    use swisha::store::PayoutStore;
    let (base, state) = app("http://127.0.0.1:1/api/v2/summary.json", Vec::new()).await;
    assert_eq!(health(&base).await["payouts_paid_twice"], 0);

    let payout = NewPayout {
        payer: "sthlm",
        reference: "H-1",
        payer_alias: "1231234567",
        payee_alias: "46701234567",
        payee_ssn: Some(common::SSN),
        amount: 1.0,
        message: "H-1",
        swish_ref: "AAAABBBBCCCCDDDDEEEEFFFF0000AAAA",
        callback_identifier: None,
    };
    state.store.claim(&payout).await.expect("claim");
    common::stage(&state, "sthlm", "H-1", "ERROR").await;
    let redo = NewRedo {
        payer: "sthlm",
        replaces: "H-1",
        reference: "H-1-B",
        payer_alias: "1231234567",
        swish_ref: "AAAABBBBCCCCDDDDEEEEFFFF0000BBBB",
        callback_identifier: None,
    };
    let redone = EventRecord {
        payer: "sthlm".into(),
        reference: "H-1".into(),
        swish_ref: None,
        event: "REDONE".into(),
        status: None,
        amount: None,
        payee_alias: None,
        error_code: None,
        error_message: None,
        ip: None,
        actor: Some("Anna".into()),
        note: None,
    };
    state.store.claim_redo(&redo, &redone).await.expect("redo");
    assert_eq!(health(&base).await["payouts_paid_twice"], 0, "redone, and not paid");

    common::stage(&state, "sthlm", "H-1", "PAID").await;
    let seen = health(&base).await;
    assert_eq!(seen["payouts_paid_twice"], 1, "{seen}");
    assert_eq!(seen["status"], "ok", "{seen}");
}
