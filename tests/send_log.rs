#![cfg(feature = "http")]

// What a payout's send leaves in the server's log. A binary of its own: beside tests that hit the
// same lines from other threads, a subscriber set for one test misses them.

mod common;

use common::{MockSwish, PHONE, SECRET, SSN};
use serde_json::{Value, json};
use swisha::state::SharedState;

struct Rig {
    internal: String,
    state: SharedState,
}

async fn rig(swish: MockSwish, reachable: bool) -> Rig {
    let mut config = common::config();
    config.swish_base_url = if reachable { swish.start().await } else { "http://127.0.0.1:1".into() };
    let state = common::state_with(config).await;
    Rig { internal: common::serve(swisha::http::internal_router().with_state(state.clone())).await, state }
}

async fn pay(r: &Rig, reference: &str) -> u16 {
    let body: Value = json!({ "reference": reference, "payee_alias": PHONE, "payee_ssn": SSN, "amount": 100.0 });
    reqwest::Client::new()
        .post(format!("{}/swish/payout", r.internal))
        .header("x-api-secret", SECRET)
        .header(common::KEY_HEADER, common::key(common::PAYER))
        .json(&body)
        .send()
        .await
        .expect("swisha answered")
        .status()
        .as_u16()
}

async fn service_log(r: &Rig) -> Vec<(String, String)> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!("SELECT level, message FROM {} ORDER BY id", r.state.config.table_logs)))
        .fetch_all(r.state.store.pool())
        .await
        .expect("logs")
}

// What the subscriber writes, kept for the test to read.
#[derive(Clone, Default)]
struct Captured(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for Captured {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("capture").extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
    type Writer = Captured;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

// A send Swish refused, or that never reached it, says so in the server's log and in the service
// log. A sent one's history says so as INITIATED and ACCEPTED, so the service log does not. One
// thread, so the server's lines reach this test's subscriber.
#[tokio::test(flavor = "current_thread")]
async fn a_send_that_did_not_go_through_is_logged_as_such() {
    let captured = Captured::default();
    let _logging = tracing::subscriber::set_default(
        tracing_subscriber::fmt().with_writer(captured.clone()).with_ansi(false).finish(),
    );
    let refused = rig(MockSwish::new().rejects(422, r#"[{"errorCode":"RF07"}]"#), true).await;
    pay(&refused, "L-1").await;
    let unreachable = rig(MockSwish::new(), false).await;
    pay(&unreachable, "L-2").await;
    let sent = rig(MockSwish::new().accepts().resolves_to("PAID"), true).await;
    assert_eq!(pay(&sent, "L-3").await, 202);
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let lines = String::from_utf8(captured.0.lock().expect("capture").clone()).expect("utf-8");
    let warned = |reference: &str| lines.lines().find(|l| l.contains("WARN") && l.contains(reference)).map(str::to_string);
    let refusal = warned("L-1").expect("a warn line for the refusal");
    assert!(refusal.contains("422") && refusal.contains("RF07") && refusal.contains("DECLINED"), "{refusal}");
    let unsent = warned("L-2").expect("a warn line for the send that never arrived");
    assert!(unsent.contains("PENDING"), "{unsent}");

    let logged = |log: &[(String, String)], level: &str, message: &str| {
        log.iter().any(|(l, m)| l == level && m == message)
    };
    let log = service_log(&refused).await;
    assert!(logged(&log, "warn", "Payout test/L-1 refused by Swish (RF07)"), "{log:?}");
    let log = service_log(&unreachable).await;
    assert!(logged(&log, "warn", "Payout test/L-2 could not be sent"), "{log:?}");
    let log = service_log(&sent).await;
    assert!(!log.iter().any(|(_, message)| message.starts_with("Payout test/L-3")), "{log:?}");
}

// Swish answering nothing usable is one warning per check, not one per question: one payout whose
// follow-up asked eight times wrote eight warnings, and its check two more.
#[tokio::test(flavor = "current_thread")]
async fn a_silent_swish_is_one_warning_per_check() {
    use swisha::store::PayoutStore;
    let captured = Captured::default();
    let _logging = tracing::subscriber::set_default(
        tracing_subscriber::fmt().with_writer(captured.clone()).with_ansi(false).finish(),
    );
    let r = rig(MockSwish::new().accepts().answers_gets_with(429), true).await;
    let uuid = "AAAABBBBCCCCDDDDEEEEFFFF0000CCCC";
    let payout = swisha::domain::payout::NewPayout {
        payer: common::PAYER,
        reference: "L-4",
        payer_alias: common::PAYER_NUMBER,
        payee_alias: "46701234567",
        payee_ssn: Some(SSN),
        amount: 100.0,
        message: "L-4",
        swish_ref: uuid,
        callback_identifier: None,
    };
    r.state.store.claim(&payout).await.expect("claim");
    let warnings = || String::from_utf8(captured.0.lock().expect("capture").clone()).expect("utf-8").matches(" WARN ").count();

    let sender = r.state.payer(common::PAYER).expect("loaded");
    let answer = swisha::swish::submit::lookup_payout_status(&sender, uuid, &r.state).await;
    assert_eq!(answer, Err(swisha::swish::submit::NoAnswer::Unusable));
    assert_eq!(warnings(), 0, "one question is not worth a warning of its own");
    swisha::swish::reconcile::reconcile(common::PAYER.into(), "L-4".into(), false, r.state.clone()).await;
    assert_eq!(warnings(), 1, "a check that got no answer says so once");
}
