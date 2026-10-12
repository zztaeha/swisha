#![cfg(feature = "http")]

// How swisha holds up as the number count grows: each test pins one way it scales, measured at
// up to 10,000 numbers.

mod common;

use std::time::{Duration, Instant};

use common::MockSwish;
use swisha::domain::payer::PayerRecord;
use swisha::domain::request::NewPayerRequest;
use swisha::state::{Entry, Roster, SharedState};

fn record(i: u64) -> PayerRecord {
    PayerRecord {
        name: format!("n{i}"),
        nickname: String::new(),
        number: (1_230_000_000 + i).to_string(),
        added_by: String::new(),
        created_at: 0,
        updated_at: 0,
        disabled: false,
    }
}

// Requests, callbacks and reloads all look numbers up, a reload each of them: a linear search made a
// reload of 50,000 numbers over a billion comparisons, every 10 seconds and on every add.
#[test]
fn fifty_thousand_numbers_are_each_found_at_once() {
    let count = 50_000;
    let entries = (0..count).map(|i| Entry { record: record(i), loaded: Err("not loaded".into()) }).collect();
    let roster = Roster::new(entries);

    let started = Instant::now();
    for i in 0..count {
        assert!(roster.find(&format!("n{i}")).is_some());
        assert_eq!(roster.name_of(&(1_230_000_000 + i).to_string()), Some(format!("n{i}").as_str()));
    }
    assert!(roster.find("missing").is_none() && roster.name_of("1239999999").is_none());
    assert!(started.elapsed() < Duration::from_secs(1), "{count} lookups each way took {:?}", started.elapsed());
}

// Numbers added as the API adds them, with real certificates in the folder, so a reload loads them.
async fn numbers_added(count: u64, swish: &MockSwish) -> SharedState {
    let mut config = common::config();
    config.swish_base_url = swish.clone().start().await;
    let state = common::state_with_payers(config, Vec::new()).await;
    let pem = common::bundle("scale test", "365");
    for i in 0..count {
        let request = NewPayerRequest {
            name: format!("p{i}"),
            nickname: format!("Customer {i}"),
            number: (1_231_000_000 + i).to_string(),
            tls: pem.clone(),
            signing: pem.clone(),
            by: "test".into(),
        };
        swisha::payers::add(&state, &request, "").await.expect("add a number");
    }
    state
}

// A cold start with many numbers loads them on every core, and comes out as one thread made it: in
// the stored order, each loaded or held aside with its reason.
#[tokio::test]
async fn a_cold_start_with_many_numbers_loads_each_in_order() {
    let state = numbers_added(100, &MockSwish::new()).await;
    let broken = ["p7", "p50", "p93"];
    for name in broken {
        let number = state.payers.snapshot().find(name).expect("added").record.number.clone();
        let file = state.config.cert_dir.join(&number).join(format!("tls-{number}-{name}.pem"));
        std::fs::write(file, "not a certificate").expect("broken while the instance was down");
    }

    let cold = common::state_with_payers(state.config.clone(), Vec::new()).await;
    swisha::payers::refresh(&cold).await.expect("load");
    let names = |state: &SharedState| {
        state.payers.snapshot().entries().iter().map(|entry| entry.record.name.clone()).collect::<Vec<_>>()
    };
    assert_eq!(names(&cold), names(&state), "the stored order");
    for entry in cold.payers.snapshot().entries() {
        let name = entry.record.name.as_str();
        match &entry.loaded {
            Ok(_) => assert!(!broken.contains(&name), "{name} loaded from a broken file"),
            Err(reason) => assert!(broken.contains(&name) && reason.contains("TLS certificate"), "{name}: {reason}"),
        }
    }
}

fn probes(swish: &MockSwish) -> usize {
    swish.gets_by().iter().filter(|(asked, _)| asked == "ping").count()
}

// One copy asks Swish about one number a second, whatever the count. Asking each one every 30
// seconds would be 10,000 requests in one second from each copy at 10,000 numbers.
#[tokio::test]
async fn swish_is_asked_about_one_number_a_second_however_many_there_are() {
    let swish = MockSwish::new();
    let state = numbers_added(60, &swish).await;
    tokio::spawn(swisha::payers::watch(state.clone()));
    tokio::time::sleep(Duration::from_millis(3500)).await;
    let asked = probes(&swish);
    assert!((1..=4).contains(&asked), "{asked} probes in 3.5 seconds");
}

fn fast(fresh: Duration, retry: Duration) -> swisha::payers::ProbeSchedule {
    swisha::payers::ProbeSchedule { pace: Duration::from_millis(20), fresh, retry }
}

// Each number is asked once before any is asked twice, and one Swish answered about a payout is
// passed by: that answer proved it already.
#[tokio::test]
async fn each_number_is_asked_in_turn_unless_a_payout_just_proved_it() {
    let swish = MockSwish::new();
    let state = numbers_added(30, &swish).await;
    state.payer("p7").expect("loaded").answered();
    let proved = state.payer("p7").expect("loaded").probe.read().await.expect("answered").at;

    let schedule = fast(Duration::from_secs(60), Duration::from_secs(60));
    let probing = tokio::spawn(swisha::payers::probe(state.clone(), schedule));
    tokio::time::sleep(Duration::from_millis(1500)).await;
    probing.abort();

    assert_eq!(probes(&swish), 29, "each of the other 29 once, and nothing more until they are due");
    for i in (0..30).filter(|i| *i != 7) {
        let last = state.payer(&format!("p{i}")).expect("loaded").probe.read().await.expect("asked");
        assert!(last.reachable, "p{i}");
    }
    let p7 = state.payer("p7").expect("loaded").probe.read().await.expect("answered");
    assert_eq!(p7.at, proved, "p7 was asked although a payout had just proved it");
}

// A number Swish did not answer is asked again after `retry`, so one blip never leaves health
// degraded until the number's turn comes round hours later.
#[tokio::test]
async fn a_number_swish_did_not_answer_is_asked_again_soon() {
    let swish = MockSwish::new();
    let state = numbers_added(3, &swish).await;
    let unreachable = {
        let mut config = state.config.clone();
        config.swish_base_url = "http://127.0.0.1:1".into();
        let pool = common::pool_with_schema(&config).await;
        let moved = common::state_over(pool, config, Vec::new()).await;
        moved.payers.replace(Roster::new(state.payers.snapshot().entries().to_vec()));
        moved
    };

    let schedule = fast(Duration::from_secs(60), Duration::from_millis(200));
    let probing = tokio::spawn(swisha::payers::probe(unreachable.clone(), schedule));
    tokio::time::sleep(Duration::from_millis(1200)).await;
    probing.abort();

    for i in 0..3 {
        let last = unreachable.payer(&format!("p{i}")).expect("loaded").probe.read().await.expect("asked");
        assert!(!last.reachable, "p{i}");
        let age = last.at.elapsed();
        assert!(age < Duration::from_millis(600), "p{i} was last asked {age:?} ago, not again after 200 ms");
    }
}

// A payout Swish answered proves its number reaches Swish, so the probes pass that number by.
#[tokio::test]
async fn a_payout_swish_answered_counts_as_a_probe() {
    let swish = MockSwish::new().accepts().resolves_to("PAID");
    let mut config = common::config();
    config.swish_base_url = swish.clone().start().await;
    let state = common::state_with(config).await;
    let base = common::serve(swisha::http::internal_router().with_state(state.clone())).await;
    let payer = state.payer(common::PAYER).expect("loaded");
    assert!(payer.probe.read().await.is_none());

    let body = serde_json::json!({
        "reference": "SCALE-1", "payee_alias": common::PHONE, "payee_ssn": common::SSN, "amount": 100.0,
    });
    let answer = reqwest::Client::new()
        .post(format!("{base}/swish/payout"))
        .header("x-api-secret", common::SECRET)
        .header(common::KEY_HEADER, common::key(common::PAYER))
        .json(&body)
        .send()
        .await
        .expect("request");
    assert_eq!(answer.status().as_u16(), 202);
    let proved = payer.probe.read().await.expect("the submit's answer counts");
    assert!(proved.reachable);
    assert_eq!(probes(&swish), 0);
}

// An add asks whether its number ever paid under another name. With no index, that read every
// payout: 1.1 seconds and 2 GB from disk per add at 10 million, under the lock that orders adds.
#[tokio::test]
async fn an_add_finds_a_numbers_past_payouts_through_an_index() {
    let config = common::config();
    let (_, pool) = common::state_and_pool(config.clone()).await;
    let table = &config.table_payouts;
    let seed = format!(
        "INSERT INTO {table} (payer, reference, payer_alias, payee_alias, amount, message, status)
         SELECT 'n' || (i % 500), 'R-' || i, (1230000000 + i % 500)::text, '46701234567', 100, 'm', 'PAID'
           FROM generate_series(1, 50000) AS i"
    );
    sqlx::query(sqlx::AssertSqlSafe(seed)).execute(&pool).await.expect("seed");
    sqlx::query(sqlx::AssertSqlSafe(format!("ANALYZE {table}"))).execute(&pool).await.expect("analyze");

    let explain = format!("EXPLAIN SELECT payer FROM {table} WHERE payer_alias = $1 AND payer <> $2 LIMIT 1");
    let plan: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(explain))
        .bind("1238888888")
        .bind("new")
        .fetch_all(&pool)
        .await
        .expect("plan");
    assert!(plan.iter().any(|line| line.contains("Index")), "{plan:#?}");
}

// Payouts Swish left at ERROR or DEBITED leave the index read every 5 seconds once their last
// check is recorded: 150,000 left in it would make that read 58 ms at 10 million payouts.
#[tokio::test]
async fn payouts_past_their_last_check_never_slow_the_question_of_what_is_due() {
    use swisha::store::PayoutStore;
    let checks = swisha::swish::reconcile::CHECKS;
    let config = common::config();
    let (state, pool) = common::state_and_pool(config.clone()).await;
    let table = &config.table_payouts;
    // Each one past its 48 hours, with its last check recorded, as the checks leave it.
    let seed = format!(
        "INSERT INTO {table} (payer, reference, payer_alias, payee_alias, amount, message, swish_ref, status,
                              checked_at, checks_ended, created_at, updated_at)
         SELECT 'old' || (i % 5), 'OLD-' || i, '1231111111', '46701234567', 100, 'm', upper(md5(i::text)), 'ERROR',
                NOW() - interval '1 day', TRUE, NOW() - interval '3 days', NOW() - interval '3 days'
           FROM generate_series(1, 300000) AS i"
    );
    sqlx::query(sqlx::AssertSqlSafe(seed)).execute(&pool).await.expect("seed");
    let fresh = format!(
        "INSERT INTO {table} (payer, reference, payer_alias, payee_alias, amount, message, swish_ref, status,
                              created_at, updated_at)
         VALUES ('fresh', 'NEW-1', '1232222222', '46701234567', 100, 'm', 'ABC', 'PENDING',
                 NOW() - interval '1 hour', NOW() - interval '1 hour')"
    );
    sqlx::query(sqlx::AssertSqlSafe(fresh)).execute(&pool).await.expect("fresh");
    sqlx::query(sqlx::AssertSqlSafe(format!("VACUUM ANALYZE {table}"))).execute(&pool).await.expect("analyze");

    let mut fastest = Duration::MAX;
    for _ in 0..3 {
        let started = Instant::now();
        let due = state.store.payers_with_due_checks(&checks).await.expect("due");
        fastest = fastest.min(started.elapsed());
        assert_eq!(due, vec!["fresh".to_string()]);
    }
    assert!(fastest < Duration::from_millis(5), "the question read every old payout: {fastest:?}");
}

// Numbers asked during an outage read offline until asked again, so they go before numbers never
// asked: behind those, 121 numbers would still read offline 45 seconds after a two-minute outage.
#[tokio::test]
async fn a_number_that_failed_is_asked_again_before_any_never_asked() {
    let swish = MockSwish::new();
    let state = numbers_added(40, &swish).await;
    let failed = swisha::state::SwishProbe {
        reachable: false,
        at: std::time::Instant::now() - Duration::from_secs(120),
    };
    for i in 0..5 {
        *state.payer(&format!("p{i}")).expect("loaded").probe.write().await = Some(failed);
    }

    let probing = tokio::spawn(swisha::payers::probe(state.clone(), fast(Duration::from_secs(60), Duration::from_secs(30))));
    tokio::time::sleep(Duration::from_millis(300)).await;
    probing.abort();

    for i in 0..5 {
        let last = state.payer(&format!("p{i}")).expect("loaded").probe.read().await.expect("asked");
        assert!(last.reachable, "p{i} was left offline while numbers never asked went first");
    }
}

// A follow-up Swish answered proves its number just as a submit does.
#[tokio::test]
async fn a_follow_up_swish_answered_counts_as_a_probe() {
    let swish = MockSwish::new().accepts().resolves_to("PAID");
    let mut config = common::config();
    config.swish_base_url = swish.clone().start().await;
    let state = common::state_with(config).await;
    let payer = state.payer(common::PAYER).expect("loaded");

    let answer = swisha::swish::submit::lookup_payout_status(&payer, "7CBA599425BC499C93D6F0E7310085C3", &state).await;
    assert_eq!(answer.expect("answered").status, "PAID");
    assert!(payer.probe.read().await.expect("the lookup's answer counts").reachable);
    assert_eq!(probes(&swish), 0);
}
