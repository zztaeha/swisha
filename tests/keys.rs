#![cfg(feature = "http")]

// Every number has its own key. The API secret reaches swisha and adds numbers; only a number's
// key reaches that number, and a replaced key stops working at once, on every instance.

mod common;

use std::time::{Duration, Instant};

use common::{KEY_HEADER, MockSwish, PHONE, SECRET, SSN, bundle, key};
use reqwest::Method;
use serde_json::{Value, json};
use swisha::domain::request::PersonRequest;
use swisha::payers::PayerError;
use swisha::state::SharedState;
use swisha::store::PayoutStore;

const STHLM: &str = "1234679304";
const GBG: &str = "1239999999";

struct Instance {
    base: String,
    state: SharedState,
}

struct Rig {
    a: Instance,
    pool: sqlx::PgPool,
    config: swisha::config::Config,
    swish: MockSwish,
}

fn numbers() -> Vec<swisha::payers::Payer> {
    vec![common::payer("sthlm", STHLM), common::payer("gbg", GBG)]
}

async fn rig() -> Rig {
    let swish = MockSwish::new().accepts().resolves_to("CREATED");
    let mut config = common::config();
    config.swish_base_url = swish.clone().start().await;
    let (state, pool) = common::state_and_pool_with(config.clone(), numbers()).await;
    let base = common::serve(swisha::http::internal_router().with_state(state.clone())).await;
    Rig { a: Instance { base, state }, pool, config, swish }
}

// A second instance over the same database, as a copy behind a load balancer.
async fn another(r: &Rig) -> Instance {
    let ready = numbers().iter().map(common::payer_state).collect();
    let state = common::state_over(r.pool.clone(), r.config.clone(), ready).await;
    let base = common::serve(swisha::http::internal_router().with_state(state.clone())).await;
    Instance { base, state }
}

async fn call(at: &Instance, method: Method, path: &str, key: Option<&str>, body: Option<Value>) -> (u16, Value) {
    let mut request = reqwest::Client::new()
        .request(method, format!("{}{path}", at.base))
        .header("x-api-secret", SECRET);
    if let Some(key) = key {
        request = request.header(KEY_HEADER, key);
    }
    if let Some(body) = body {
        request = request.json(&body);
    }
    let response = request.send().await.expect("request");
    let code = response.status().as_u16();
    // An event stream never ends on its own, so only its status is read.
    let streaming = response.headers().get("content-type").is_some_and(|kind| kind == "text/event-stream");
    if streaming {
        return (code, Value::Null);
    }
    let text = response.text().await.expect("body");
    (code, serde_json::from_str(&text).unwrap_or(Value::String(text)))
}

fn payout(payer: &str, reference: &str) -> Value {
    json!({ "payer": payer, "reference": reference, "payee_alias": PHONE, "payee_ssn": SSN, "amount": 100.0 })
}

fn person() -> PersonRequest {
    PersonRequest { by: "taeha on the server".into(), note: None }
}

fn new_number(name: &str, number: &str) -> Value {
    json!({
        "name": name, "nickname": name, "number": number,
        "tls": bundle(number, "30"), "signing": bundle(number, "31"), "by": "Anna",
    })
}

async fn logs(r: &Rig) -> Vec<String> {
    let table = &r.config.table_logs;
    let sql = format!("SELECT message || ' ' || COALESCE(context, '') FROM {table} ORDER BY id");
    sqlx::query_scalar(sqlx::AssertSqlSafe(sql)).fetch_all(&r.pool).await.expect("logs")
}

// Every route about sthlm, with a body each would accept. The key route is last: it replaces the
// key the others are sent with.
fn routes() -> Vec<(Method, &'static str, Option<Value>)> {
    let certificates = json!({ "tls": bundle(STHLM, "30"), "signing": bundle(STHLM, "31"), "by": "Anna" });
    vec![
        (Method::POST, "/swish/payout", Some(payout("sthlm", "INV-1"))),
        (Method::POST, "/swish/payout/sthlm/INV-1/confirm", Some(json!({ "by": "Anna", "note": "On the statement" }))),
        (Method::POST, "/swish/payout/sthlm/INV-1/redo", Some(json!({ "reference": "INV-1-B", "by": "Anna" }))),
        (Method::GET, "/swish/status/sthlm/INV-1", None),
        (Method::GET, "/events?payer=sthlm", None),
        (Method::GET, "/swish/payers/sthlm", None),
        (Method::PATCH, "/swish/payers/sthlm", Some(json!({ "nickname": "Stockholm", "by": "Anna" }))),
        (Method::PUT, "/swish/payers/sthlm/certificates", Some(certificates)),
        (Method::POST, "/swish/payers/sthlm/disable", Some(json!({ "by": "Anna" }))),
        (Method::POST, "/swish/payers/sthlm/enable", Some(json!({ "by": "Anna" }))),
        (Method::POST, "/swish/payers/sthlm/key", Some(json!({ "by": "Anna" }))),
    ]
}

// No key, a malformed one or one no number holds is 401, and another number's key is 403, on
// every route about a number. None of them stores, sends or changes anything.
#[tokio::test]
async fn every_route_about_a_number_needs_that_numbers_key() {
    let r = rig().await;
    let (sthlm, gbg, nobody) = (key("sthlm"), key("gbg"), key("nobody"));
    let malformed = sthlm.to_lowercase();
    for (method, path, body) in routes() {
        for (label, sent) in [("no key", None), ("a malformed key", Some(&malformed)), ("a key no number holds", Some(&nobody))] {
            let (code, answer) = call(&r.a, method.clone(), path, sent.map(String::as_str), body.clone()).await;
            assert_eq!(code, 401, "{method} {path} with {label}: {answer}");
            assert!(answer["error"].as_str().is_some_and(|e| e.contains("x-payer-key")), "{path}: {answer}");
        }
        let (code, answer) = call(&r.a, method.clone(), path, Some(&gbg), body.clone()).await;
        assert_eq!(code, 403, "{method} {path} with another number's key: {answer}");
        assert!(!answer["error"].as_str().unwrap_or_default().contains("sthlm"), "{answer}");
    }

    assert!(r.a.state.store.snapshot("sthlm", "INV-1").await.expect("store").is_none(), "nothing stored");
    assert!(r.swish.posted().is_empty(), "nothing sent");
    let stored = r.a.state.store.payers().await.expect("store");
    let row = stored.iter().find(|payer| payer.name == "sthlm").expect("stored");
    assert_eq!((row.nickname.as_str(), row.disabled), ("sthlm", false), "nothing changed");
    let hash = swisha::payers::key_hash(&sthlm).expect("a key");
    assert!(r.a.state.store.payer_by_key(&hash).await.expect("store").is_some(), "the key was not replaced");
    assert!(std::fs::read_dir(&r.config.cert_dir).expect("folder").next().is_none(), "no file written");

    for (method, path, body) in routes() {
        let (code, answer) = call(&r.a, method.clone(), path, Some(&sthlm), body).await;
        assert!(code != 401 && code != 403, "{method} {path} with its own key: {code} {answer}");
    }
    let (code, answer) = call(&r.a, Method::GET, "/swish/status/INV-1", Some(&gbg), None).await;
    assert_eq!(code, 404, "the short path reads the key's own number, which has no INV-1: {answer}");
}

// The secret alone adds a number and reads how many there are. It names none, and lists none.
#[tokio::test]
async fn the_secret_alone_adds_numbers_and_reads_counts_but_reaches_no_number() {
    let r = rig().await;
    let (code, added) = call(&r.a, Method::POST, "/swish/payers", None, Some(new_number("malmo", "1231111111"))).await;
    assert_eq!(code, 201, "{added}");

    let (code, health) = call(&r.a, Method::GET, "/system/health", None, None).await;
    assert_eq!(code, 200);
    assert_eq!(health["payers"]["stored"], 3, "{health}");
    let text = health.to_string();
    for named in ["sthlm", "gbg", "malmo", STHLM, GBG, "1231111111"] {
        assert!(!text.contains(named), "health names {named}: {text}");
    }
    assert_eq!(call(&r.a, Method::GET, "/swish/payers", None, None).await.0, 405, "no list");

    let anonymous = reqwest::Client::new()
        .post(format!("{}/swish/payers", r.a.base))
        .json(&new_number("lund", "1232222222"))
        .send()
        .await
        .expect("request");
    assert_eq!(anonymous.status().as_u16(), 401, "without the secret, not even an add");
}

// The key exists in the add's answer and nowhere else: not in the row, a read or the log.
#[tokio::test]
async fn a_key_is_shown_once_and_only_its_hash_is_kept() {
    let r = rig().await;
    let (code, added) = call(&r.a, Method::POST, "/swish/payers", None, Some(new_number("malmo", "1231111111"))).await;
    assert_eq!(code, 201, "{added}");
    let made = added["key"].as_str().expect("the key").to_string();
    assert!(made.starts_with("swk_") && made.len() == 68, "{made}");

    let table = &r.config.table_payers;
    let sql = format!("SELECT key_hash, row_to_json(p)::text FROM {table} p WHERE name = 'malmo'");
    let (hash, row): (String, String) = sqlx::query_as(sqlx::AssertSqlSafe(sql)).fetch_one(&r.pool).await.expect("row");
    assert_eq!(Some(hash), swisha::payers::key_hash(&made), "the row holds the key's SHA-256");
    assert!(!row.contains(&made[4..]), "the row never holds the key: {row}");

    let (code, read) = call(&r.a, Method::GET, "/swish/payers/malmo", Some(&made), None).await;
    assert_eq!(code, 200, "{read}");
    assert!(read.get("key").is_none() && !read.to_string().contains(&made[4..]), "{read}");
    assert!(logs(&r).await.iter().all(|line| !line.contains(&made[4..])), "the log never holds the key");
}

// A key replaced with itself, or on the server, stops working on every instance at once, and the
// log says who replaced it without ever holding a key.
#[tokio::test]
async fn a_replaced_key_stops_working_at_once_on_every_instance() {
    let r = rig().await;
    let b = another(&r).await;
    let old = key("sthlm");
    assert_eq!(call(&b, Method::GET, "/swish/payers/sthlm", Some(&old), None).await.0, 200);

    let body = json!({ "by": "Anna", "note": "Pasted in a ticket" });
    let (code, answer) = call(&r.a, Method::POST, "/swish/payers/sthlm/key", Some(&old), Some(body)).await;
    assert_eq!(code, 200, "{answer}");
    assert_eq!(answer["payer"], "sthlm");
    let new = answer["key"].as_str().expect("the new key").to_string();
    assert_ne!(new, old);
    assert_eq!(call(&b, Method::GET, "/swish/payers/sthlm", Some(&old), None).await.0, 401, "the old key, at once");
    assert_eq!(call(&b, Method::GET, "/swish/payers/sthlm", Some(&new), None).await.0, 200);

    let issued = swisha::payers::replace_key(&b.state, "sthlm", None, &person(), "").await.expect("issued");
    assert_eq!(call(&r.a, Method::GET, "/swish/payers/sthlm", Some(&new), None).await.0, 401);
    assert_eq!(call(&r.a, Method::GET, "/swish/payers/sthlm", Some(&issued), None).await.0, 200);
    assert_eq!(call(&r.a, Method::GET, "/swish/payers/gbg", Some(&key("gbg")), None).await.0, 200, "gbg's own key is untouched");
    let missing = swisha::payers::replace_key(&b.state, "nobody", None, &person(), "").await;
    assert!(matches!(missing, Err(PayerError::NotFound)), "{missing:?}");

    let trail = logs(&r).await;
    assert!(trail.iter().any(|line| line.starts_with("Payer sthlm key replaced by Anna ")), "{trail:?}");
    assert!(trail.iter().any(|line| line.starts_with("Payer sthlm key replaced by taeha on the server ")), "{trail:?}");
    for made in [&old, &new, &issued] {
        assert!(trail.iter().all(|line| !line.contains(&made[4..])), "a key in the log");
    }
}

// Of several replacements made with one key, one wins. The others find it gone, and only the
// winner's key works.
#[tokio::test]
async fn replacements_made_with_one_key_leave_one_working_key() {
    let r = rig().await;
    let old = key("sthlm");
    let mut presses = Vec::new();
    for _ in 0..4 {
        let (url, old) = (format!("{}/swish/payers/sthlm/key", r.a.base), old.clone());
        presses.push(tokio::spawn(async move {
            let response = reqwest::Client::new()
                .post(url)
                .header("x-api-secret", SECRET)
                .header(KEY_HEADER, old)
                .json(&json!({ "by": "Anna" }))
                .send()
                .await
                .expect("request");
            (response.status().as_u16(), response.json::<Value>().await.expect("json"))
        }));
    }
    let mut won = Vec::new();
    for press in presses {
        let (code, answer) = press.await.expect("join");
        match code {
            200 => won.push(answer["key"].as_str().expect("key").to_string()),
            401 | 409 => {}
            other => panic!("{other}: {answer}"),
        }
    }
    assert_eq!(won.len(), 1, "exactly one replacement wins");
    assert_eq!(call(&r.a, Method::GET, "/swish/payers/sthlm", Some(&won[0]), None).await.0, 200);
    assert_eq!(call(&r.a, Method::GET, "/swish/payers/sthlm", Some(&old), None).await.0, 401);
}

// Disabling is read with the key, so another instance refuses the number's next payout at once,
// not at its next refresh. The server's command acts the same way.
#[tokio::test]
async fn a_disable_reaches_every_instance_at_once() {
    let r = rig().await;
    let b = another(&r).await;
    let sthlm = key("sthlm");
    let body = json!({ "by": "Anna" });
    assert_eq!(call(&r.a, Method::POST, "/swish/payers/sthlm/disable", Some(&sthlm), Some(body.clone())).await.0, 200);

    let (code, answer) = call(&b, Method::POST, "/swish/payout", Some(&sthlm), Some(payout("sthlm", "INV-1"))).await;
    assert_eq!(code, 400, "{answer}");
    assert!(answer["error"].as_str().unwrap_or_default().contains("disabled"), "{answer}");
    assert!(b.state.store.snapshot("sthlm", "INV-1").await.expect("store").is_none(), "nothing stored");

    assert_eq!(call(&r.a, Method::POST, "/swish/payers/sthlm/enable", Some(&sthlm), Some(body)).await.0, 200);
    assert_eq!(call(&b, Method::POST, "/swish/payout", Some(&sthlm), Some(payout("sthlm", "INV-1"))).await.0, 202);

    swisha::payers::set_disabled(&b.state, "sthlm", true, &person(), "").await.expect("disabled on the server");
    let (code, answer) = call(&r.a, Method::POST, "/swish/payout", Some(&sthlm), Some(payout("sthlm", "INV-2"))).await;
    assert_eq!(code, 400, "the server's disable, at once: {answer}");
}

// A stream carries its key's number and nothing else, and ends once that key is replaced.
#[tokio::test]
async fn an_event_stream_is_one_numbers_and_ends_with_its_key() {
    let r = rig().await;
    let (sthlm, gbg) = (key("sthlm"), key("gbg"));
    assert_eq!(call(&r.a, Method::GET, "/events?payer=gbg", Some(&sthlm), None).await.0, 403);
    let (code, answer) = call(&r.a, Method::GET, "/events?channel=logs", Some(&sthlm), None).await;
    assert_eq!(code, 403, "the log spans every number: {answer}");
    // A kind that does not exist would open a stream that never carries anything.
    let (code, answer) = call(&r.a, Method::GET, "/events?channel=event", Some(&sthlm), None).await;
    assert_eq!(code, 400, "{answer}");
    assert!(answer.to_string().contains("events or updates"), "{answer}");

    let mut stream = reqwest::Client::new()
        .get(format!("{}/events", r.a.base))
        .header("x-api-secret", SECRET)
        .header(KEY_HEADER, &sthlm)
        .send()
        .await
        .expect("subscribe");
    assert_eq!(stream.status().as_u16(), 200);
    tokio::time::sleep(Duration::from_millis(200)).await;

    assert_eq!(call(&r.a, Method::POST, "/swish/payout", Some(&gbg), Some(payout("gbg", "INV-1"))).await.0, 202);
    assert_eq!(call(&r.a, Method::POST, "/swish/payout", Some(&sthlm), Some(payout("sthlm", "INV-2"))).await.0, 202);
    let mut seen = String::new();
    let _ = tokio::time::timeout(Duration::from_millis(1500), async {
        while let Ok(Some(chunk)) = stream.chunk().await {
            seen.push_str(&String::from_utf8_lossy(&chunk));
        }
    })
    .await;
    assert!(seen.contains("INV-2") && seen.contains(r#""payer":"sthlm""#), "{seen}");
    assert!(!seen.contains("gbg") && !seen.contains("INV-1"), "another number's events arrived: {seen}");

    let body = json!({ "by": "Anna" });
    assert_eq!(call(&r.a, Method::POST, "/swish/payers/sthlm/key", Some(&sthlm), Some(body)).await.0, 200);
    let replaced = Instant::now();
    // Replaced on this instance, the stream ends at once, without waiting for the sweep.
    let ended = tokio::time::timeout(Duration::from_secs(2), async {
        while let Ok(Some(_)) = stream.chunk().await {}
    })
    .await;
    assert!(ended.is_ok(), "the stream outlived its key");
    eprintln!("   the stream ended {:?} after its key was replaced", replaced.elapsed());
}

// A key replaced on another instance, or by the server command, reaches this instance's streams
// through the sweep, which checks every open stream's key in one question each refresh.
#[tokio::test]
async fn a_key_replaced_elsewhere_ends_this_instances_streams_within_a_refresh() {
    let r = rig().await;
    let b = another(&r).await;
    let open = |key: String| {
        reqwest::Client::new().get(format!("{}/events", r.a.base)).header("x-api-secret", SECRET).header(KEY_HEADER, key).send()
    };
    let mut sthlm = open(key("sthlm")).await.expect("subscribe");
    let mut gbg = open(key("gbg")).await.expect("subscribe");
    assert_eq!((sthlm.status().as_u16(), gbg.status().as_u16()), (200, 200));
    // The sweep's first round runs as the first stream opens, and must leave held keys alone.
    let open_still = tokio::time::timeout(Duration::from_secs(1), sthlm.chunk()).await;
    assert!(open_still.is_err(), "a stream whose key is still held was ended: {open_still:?}");

    let body = json!({ "by": "Anna" });
    assert_eq!(call(&b, Method::POST, "/swish/payers/sthlm/key", Some(&key("sthlm")), Some(body)).await.0, 200);
    let replaced = Instant::now();
    let ended = tokio::time::timeout(swisha::payers::REFRESH + Duration::from_secs(3), async {
        while let Ok(Some(_)) = sthlm.chunk().await {}
    })
    .await;
    assert!(ended.is_ok(), "the stream outlived its key, replaced on another instance");
    eprintln!("   the stream ended {:?} after its key was replaced elsewhere", replaced.elapsed());

    // Keep-alive comments still arrive; only the end of the stream counts.
    let ended = tokio::time::timeout(swisha::payers::REFRESH + Duration::from_secs(1), async {
        while let Ok(Some(_)) = gbg.chunk().await {}
    })
    .await;
    assert!(ended.is_err(), "another number's stream must carry on through a sweep");
}

// What an operator reads with --list-payers: each number, whether it can pay, and its dates.
#[tokio::test]
async fn the_server_list_shows_each_number_and_whether_it_can_pay() {
    let r = rig().await;
    assert_eq!(swisha::payers::listing(&swisha::state::Roster::default(), 0), "No Swish number is stored.\n");
    for (name, number) in [("malmo", "1231111111"), ("lund", "1232222222")] {
        let (code, answer) = call(&r.a, Method::POST, "/swish/payers", None, Some(new_number(name, number))).await;
        assert_eq!(code, 201, "{answer}");
    }
    swisha::payers::set_disabled(&r.a.state, "lund", true, &person(), "").await.expect("disable");

    let listing = swisha::payers::listing(&r.a.state.payers.snapshot(), swisha::util::time::unix_now());
    assert!(listing.contains("malmo (malmo), 1231111111: ready\n  TLS certificate expires "), "{listing}");
    assert!(listing.contains("lund (lund), 1232222222: disabled\n"), "{listing}");
    assert!(listing.contains("sthlm (sthlm), 1234679304: cannot pay: "), "no files of its own: {listing}");
    assert!(listing.contains("  signing certificate expires "), "{listing}");
}

// The binary's commands are taken exactly, so a typo never starts the service by accident.
#[test]
fn the_binary_takes_its_commands_exactly() {
    let run = |args: &[&str]| {
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_swisha")).args(args).output().expect("run swisha");
        let text = |bytes: &[u8]| String::from_utf8_lossy(bytes).to_string();
        (out.status.code(), text(&out.stdout), text(&out.stderr))
    };
    for args in [
        &["--bogus"][..],
        &["serve"],
        &["--new-key"],
        &["--disable"],
        &["--enable"],
        &["--print-schema", "extra"],
        &["--list-payers", "sthlm"],
    ] {
        let (code, stdout, stderr) = run(args);
        assert_eq!(code, Some(2), "{args:?}: {stderr}");
        assert!(stderr.contains("usage: swisha") && stdout.is_empty(), "{args:?}: {stderr}");
    }
    let (code, stdout, _) = run(&["--print-schema"]);
    assert_eq!(code, Some(0));
    assert!(stdout.contains("CREATE TABLE IF NOT EXISTS swisha_payers"), "{stdout}");
}

// Every request looks its key up, so the lookup must use the key's unique index however many
// numbers there are. A CHAR column compared with the text a driver sends falls back to a scan.
#[tokio::test]
async fn a_key_is_found_through_its_index() {
    let r = rig().await;
    let table = &r.config.table_payers;
    let seed = format!(
        "INSERT INTO {table} (name, nickname, number, added_by, key_hash)
         SELECT 'n' || i, 'n', (1230000000 + i)::text, 'test', upper(encode(sha256(('k' || i)::bytea), 'hex'))
           FROM generate_series(1, 5000) AS i"
    );
    sqlx::query(sqlx::AssertSqlSafe(seed)).execute(&r.pool).await.expect("seed");
    sqlx::query(sqlx::AssertSqlSafe(format!("ANALYZE {table}"))).execute(&r.pool).await.expect("analyze");

    let hash = swisha::payers::key_hash(&key("sthlm")).expect("a key");
    let explain = format!("EXPLAIN SELECT name, disabled_at IS NOT NULL FROM {table} WHERE key_hash = $1");
    let plan: Vec<String> =
        sqlx::query_scalar(sqlx::AssertSqlSafe(explain)).bind(&hash).fetch_all(&r.pool).await.expect("plan");
    assert!(plan.iter().any(|line| line.contains("Index Scan")), "{plan:#?}");
}
