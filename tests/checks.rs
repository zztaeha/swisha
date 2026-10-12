#![cfg(feature = "http")]

// Every number is its own account at Swish. Each test gives one number trouble, a backlog or a
// Swish that never answers, and measures that another number's checks still run on time.

mod common;

use std::time::{Duration, Instant};

use common::{MockSwish, SSN};
use swisha::domain::payout::{CheckSchedule, NewPayout};
use swisha::state::{PayerState, SharedState};
use swisha::store::PayoutStore;
use swisha::swish::reconcile::{BETWEEN_CHECKS, CHECK_TICK, run, sweep};

// Due almost at once, and never again within a test.
const SCHEDULE: CheckSchedule = CheckSchedule {
    first_after: Duration::from_millis(50),
    every: Duration::from_secs(3600),
    window: Duration::from_secs(3600),
};

fn number(payer: &str, alias: &str, client: reqwest::Client) -> PayerState {
    PayerState { client, ..common::payer_state(&common::payer(payer, alias)) }
}

async fn stage(state: &SharedState, payer: &str, reference: &str, uuid: &str) {
    state
        .store
        .claim(&NewPayout {
            payer,
            reference,
            payer_alias: "1230000000",
            payee_alias: "46701234567",
            payee_ssn: Some(SSN),
            amount: 100.0,
            message: "checks",
            swish_ref: uuid,
            callback_identifier: None,
        })
        .await
        .expect("claim");
    assert!(common::stage(state, payer, reference, "PENDING").await);
}

fn uuid(n: usize) -> String {
    format!("AAAABBBBCCCCDDDDEEEEFFFF{n:08}")
}

// Forty payouts of one number are due at once, as after an outage at its bank. The other
// number's single payout is asked about within one tick, long before the backlog is through.
#[tokio::test]
async fn one_numbers_backlog_never_delays_another() {
    let swish = MockSwish::new().resolves_to("CREATED");
    let mut config = common::config();
    config.swish_base_url = swish.clone().start().await;
    let pool = common::pool_with_schema(&config).await;
    let numbers = vec![
        number("busy", "1230000001", reqwest::Client::new()),
        number("quiet", "1230000002", reqwest::Client::new()),
    ];
    let state = common::state_over(pool, config, numbers).await;

    for n in 0..40 {
        stage(&state, "busy", &format!("B-{n}"), &uuid(n)).await;
    }
    stage(&state, "quiet", "Q-1", &uuid(999)).await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    let started = Instant::now();
    let checks = tokio::spawn(run(state.clone(), SCHEDULE));
    let quiet = uuid(999);
    let asked = |id: &str| swish.gets_by().iter().any(|(asked, _)| asked == id);
    while !asked(&quiet) {
        assert!(started.elapsed() < CHECK_TICK + Duration::from_secs(3), "the quiet number waited");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let waited = started.elapsed();
    let busy_asked = (0..40).filter(|n| asked(&uuid(*n))).count();
    checks.abort();
    assert!(busy_asked < 40, "the busy number's backlog was still being worked through: {busy_asked}");
    eprintln!("   the quiet number was asked after {waited:?}, with the busy one {busy_asked} of 40 in");
}

// One number's connection to Swish hangs, 4 seconds a question. Another's payout, due while that
// goes on, still settles at the next tick: nothing of the first one's sits in its way.
#[tokio::test]
async fn a_number_whose_swish_never_answers_never_holds_up_another() {
    // A server that accepts connections and never says a word.
    let tarpit = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = tarpit.local_addr().expect("address");
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((socket, _)) = tarpit.accept().await {
            held.push(socket);
        }
    });
    let hung = reqwest::Client::builder()
        .proxy(reqwest::Proxy::all(format!("http://{address}")).expect("proxy"))
        .build()
        .expect("client");

    let swish = MockSwish::new().resolves_to("PAID");
    let mut config = common::config();
    config.swish_base_url = swish.clone().start().await;
    let pool = common::pool_with_schema(&config).await;
    let numbers = vec![number("hung", "1230000003", hung), number("fine", "1230000004", reqwest::Client::new())];
    let state = common::state_over(pool, config, numbers).await;

    for n in 0..3 {
        stage(&state, "hung", &format!("H-{n}"), &uuid(n)).await;
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    let checks = tokio::spawn(run(state.clone(), SCHEDULE));

    // The hung number's questions are under way. Only now does the other number have one due.
    tokio::time::sleep(Duration::from_millis(500)).await;
    stage(&state, "fine", "F-1", &uuid(500)).await;
    let staged = Instant::now();
    loop {
        let status = state.store.snapshot("fine", "F-1").await.expect("store").and_then(|s| s.status);
        if status.as_deref() == Some("PAID") {
            break;
        }
        assert!(staged.elapsed() < CHECK_TICK + Duration::from_secs(2), "the fine number waited on the hung one");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let settled = staged.elapsed();
    let hung_status = state.store.snapshot("hung", "H-0").await.expect("store").and_then(|s| s.status);
    checks.abort();
    assert_eq!(hung_status.as_deref(), Some("PENDING"), "the hung number learned nothing, and wrote nothing");
    eprintln!("   the fine number settled {settled:?} after its payout came due");
}

// One number asks Swish at most once a second, however many of its payouts are due: a backlog
// never reaches Swish as a burst.
#[tokio::test]
async fn a_number_asks_swish_at_most_once_a_second() {
    let swish = MockSwish::new().resolves_to("CREATED");
    let mut config = common::config();
    config.swish_base_url = swish.clone().start().await;
    let state = common::state_with(config).await;
    for n in 0..5 {
        stage(&state, common::PAYER, &format!("S-{n}"), &uuid(n)).await;
    }
    tokio::time::sleep(Duration::from_millis(100)).await;

    let started = Instant::now();
    assert_eq!(sweep(&state, common::PAYER, &SCHEDULE).await.expect("sweep"), 5);
    let took = started.elapsed();
    assert_eq!(swish.get_count(), 5, "one question each");
    assert!(took >= BETWEEN_CHECKS * 5 - Duration::from_millis(50), "five questions in {took:?}");
}
