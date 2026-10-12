#![cfg(feature = "http")]

// The statements a payout runs, by kind, on each road. Each holds a pool connection for a round
// trip, so a change here is a change in what an instance carries. Its own binary: it hears them all.

mod common;

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::Duration;

use common::{MockSwish, PAYER, PHONE, SECRET, SSN};
use serde_json::{Value, json};
use swisha::state::SharedState;

static STATEMENTS: Mutex<Vec<String>> = Mutex::new(Vec::new());

// Every statement sqlx runs is a tracing event under sqlx::query: its text in db.statement, or in
// summary alone when it is short.
struct Counter;

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Counter {
    fn enabled(&self, metadata: &tracing::Metadata<'_>, _: tracing_subscriber::layer::Context<'_, S>) -> bool {
        metadata.target() == "sqlx::query"
    }

    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        #[derive(Default)]
        struct Text {
            statement: String,
            summary: String,
        }
        impl tracing::field::Visit for Text {
            fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
                match field.name() {
                    "db.statement" => self.statement = value.to_string(),
                    "summary" => self.summary = value.to_string(),
                    _ => {}
                }
            }
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                self.record_str(field, &format!("{value:?}"));
            }
        }
        let mut text = Text::default();
        event.record(&mut text);
        let sql = if text.statement.trim().is_empty() { text.summary } else { text.statement };
        STATEMENTS.lock().unwrap().push(sql);
    }
}

fn kind(sql: &str) -> String {
    let kinds = [
        ("pg_notify", "notify"),
        ("ON CONFLICT (payer, reference) DO UPDATE", "claim"),
        ("WITH written AS", "status and its event"),
        ("LEFT JOIN LATERAL", "status view"),
        ("SELECT status, swish_ref, callback_identifier", "snapshot"),
        ("key_hash = $1", "key"),
        ("INSERT INTO", "event"),
    ];
    match kinds.iter().find(|(text, _)| sql.contains(text)) {
        Some((_, kind)) => kind.to_string(),
        None => format!("other: {}", sql.split_whitespace().take(8).collect::<Vec<_>>().join(" ")),
    }
}

fn counted() -> BTreeMap<String, usize> {
    let mut counts = BTreeMap::new();
    for sql in std::mem::take(&mut *STATEMENTS.lock().unwrap()) {
        *counts.entry(kind(&sql)).or_insert(0) += 1;
    }
    counts
}

fn expect(kinds: &[(&str, usize)]) -> BTreeMap<String, usize> {
    kinds.iter().map(|(kind, n)| (kind.to_string(), *n)).collect()
}

struct Rig {
    internal: String,
    callback: String,
    state: SharedState,
}

async fn rig(swish: MockSwish) -> Rig {
    let mut config = common::config();
    config.swish_base_url = swish.start().await;
    let state = common::state_with(config).await;
    Rig {
        internal: common::serve(swisha::http::internal_router().with_state(state.clone())).await,
        callback: common::serve(swisha::http::callback_router().with_state(state.clone())).await,
        state,
    }
}

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

async fn callback(r: &Rig, reference: &str, uuid: &str, status: &str, echoed: &str) {
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

async fn status(r: &Rig, reference: &str) -> Option<String> {
    swisha::store::PayoutStore::snapshot(&r.state.store, PAYER, reference).await.expect("store")?.status
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_payout_runs_the_statements_counted_here() {
    let _ = tracing::subscriber::set_global_default(
        <tracing_subscriber::Registry as tracing_subscriber::layer::SubscriberExt>::with(
            tracing_subscriber::registry(),
            Counter,
        ),
    );

    // Swish's callbacks bring both steps before the follow-up would ask.
    let r = rig(MockSwish::new().accepts().resolves_to("PAID")).await;
    counted();
    let uuid = pay(&r, "S-1").await;
    let paid = counted();
    // What Swish would echo, read by the test itself and left out of the count.
    let echoed = common::callback_identifier(&r.state, PAYER, "S-1").await;
    counted();
    callback(&r, "S-1", &uuid, "DEBITED", &echoed).await;
    callback(&r, "S-1", &uuid, "PAID", &echoed).await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    let mut by_callback = counted();
    for (kind, n) in paid {
        *by_callback.entry(kind).or_insert(0) += n;
    }
    assert_eq!(status(&r, "S-1").await.as_deref(), Some("PAID"));
    assert_eq!(
        by_callback,
        expect(&[
            ("claim", 1),
            ("event", 1),
            ("key", 1),
            ("notify", 4),
            ("snapshot", 2),
            ("status and its event", 2),
            ("status view", 2),
        ]),
        "callback road"
    );

    // No callback comes: the follow-up asks, and Swish answers DEBITED, then PAID.
    let r = rig(MockSwish::new().accepts().resolves_through(&["DEBITED", "PAID"])).await;
    counted();
    pay(&r, "S-2").await;
    tokio::time::sleep(Duration::from_secs(4)).await;
    let by_asking = counted();
    assert_eq!(status(&r, "S-2").await.as_deref(), Some("PAID"));
    assert_eq!(
        by_asking,
        expect(&[
            ("claim", 1),
            ("event", 1),
            ("key", 1),
            ("notify", 4),
            ("status and its event", 2),
            ("status view", 2),
        ]),
        "asking road"
    );
}
