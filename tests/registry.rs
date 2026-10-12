#![cfg(feature = "http")]

// How a running swisha holds its numbers: loaded from the store and the certificate folder, kept
// current as they change, and never stopped by one number whose certificates fail.

mod common;

use std::os::unix::fs::PermissionsExt;
use std::time::{Duration, Instant};

use common::{MockSwish, PHONE, SECRET, SSN, bundle};
use serde_json::{Value, json};
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

// One instance over a fresh database and certificate folder. More instances join it with
// `another`, sharing both, as copies behind a load balancer do.
async fn rig() -> Rig {
    let swish = MockSwish::new().accepts().resolves_to("CREATED");
    let mut config = common::config();
    config.swish_base_url = swish.clone().start().await;
    let (state, pool) = common::state_and_pool_with(config.clone(), Vec::new()).await;
    let base = common::serve(swisha::http::internal_router().with_state(state.clone())).await;
    Rig { a: Instance { base, state }, pool, config, swish }
}

async fn another(r: &Rig) -> Instance {
    let state = common::state_over(r.pool.clone(), r.config.clone(), Vec::new()).await;
    let base = common::serve(swisha::http::internal_router().with_state(state.clone())).await;
    Instance { base, state }
}

async fn call(
    at: &Instance,
    method: reqwest::Method,
    path: &str,
    key: Option<&str>,
    body: Option<Value>,
) -> (u16, Value) {
    let mut request = reqwest::Client::new()
        .request(method, format!("{}{path}", at.base))
        .header("x-api-secret", SECRET);
    if let Some(key) = key {
        request = request.header(common::KEY_HEADER, key);
    }
    if let Some(body) = body {
        request = request.json(&body);
    }
    let response = request.send().await.expect("request");
    let code = response.status().as_u16();
    let text = response.text().await.expect("body");
    (code, serde_json::from_str(&text).unwrap_or(Value::String(text)))
}

// Answers the number's key.
async fn add(at: &Instance, name: &str, number: &str) -> String {
    let body = json!({
        "name": name, "nickname": name, "number": number,
        "tls": bundle(number, "30"), "signing": bundle(number, "31"), "by": "Anna",
    });
    let (code, answer) = call(at, reqwest::Method::POST, "/swish/payers", None, Some(body)).await;
    assert_eq!(code, 201, "{answer}");
    answer["key"].as_str().expect("the key").to_string()
}

async fn pay(at: &Instance, key: &str, payer: &str, reference: &str) -> (u16, Value) {
    let body = json!({ "payer": payer, "reference": reference, "payee_alias": PHONE, "payee_ssn": SSN, "amount": 100.0 });
    call(at, reqwest::Method::POST, "/swish/payout", Some(key), Some(body)).await
}

// A restarted or second instance knows nothing until it reads the store, and then everything.
#[tokio::test]
async fn a_new_instance_loads_every_number_from_the_store_and_its_folder() {
    let r = rig().await;
    add(&r.a, "sthlm", STHLM).await;
    let gbg = add(&r.a, "gbg", GBG).await;

    let b = another(&r).await;
    assert!(b.state.payer("sthlm").is_none(), "nothing is known before the first read");
    swisha::payers::refresh(&b.state).await.expect("refresh");
    for name in ["sthlm", "gbg"] {
        assert!(b.state.payer(name).is_some(), "{name} is loaded");
    }
    assert_eq!(pay(&b, &gbg, "gbg", "INV-1").await.0, 202);
    let sent = common::payload(&r.swish.posted()[0]);
    assert_eq!(sent["payerAlias"], GBG, "paid from the number loaded from the store");
}

// One customer's broken certificate file must never stop the others' payouts. Its own payouts are
// refused before anything is stored, health counts it, and the number recovers once it is fixed.
#[tokio::test]
async fn a_number_whose_files_fail_is_held_aside_while_the_others_pay() {
    let r = rig().await;
    let sthlm = add(&r.a, "sthlm", STHLM).await;
    let gbg = add(&r.a, "gbg", GBG).await;
    let file = r.config.cert_dir.join(STHLM).join(format!("tls-{STHLM}-sthlm.pem"));
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).expect("chmod");

    let b = another(&r).await;
    swisha::payers::refresh(&b.state).await.expect("one bad number is not a failed refresh");
    let (code, answer) = pay(&b, &sthlm, "sthlm", "INV-1").await;
    assert_eq!(code, 503, "{answer}");
    let error = answer["error"].as_str().unwrap_or_default();
    assert!(error.contains("could not be loaded") && error.contains("chmod 600"), "{error}");
    assert!(b.state.store.snapshot("sthlm", "INV-1").await.expect("store").is_none(), "nothing stored");
    assert_eq!(pay(&b, &gbg, "gbg", "INV-1").await.0, 202, "the other number pays");

    let (_, health) = call(&b, reqwest::Method::GET, "/system/health", None, None).await;
    assert_eq!(health["status"], "degraded", "{health}");
    assert_eq!(health["payers"]["unusable"], 1, "{health}");
    let (_, own) = call(&b, reqwest::Method::GET, "/swish/payers/sthlm", Some(&sthlm), None).await;
    assert_eq!(own["usable"], false);
    assert!(own["problem"].as_str().is_some_and(|p| p.contains("chmod 600")), "{own}");

    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    swisha::payers::refresh(&b.state).await.expect("refresh");
    assert_eq!(pay(&b, &sthlm, "sthlm", "INV-1").await.0, 202, "fixed, it pays, and INV-1 was never spent");
}

// A change on one instance reaches every other within one refresh, with nothing restarted.
#[tokio::test]
async fn another_instances_change_arrives_within_a_refresh() {
    let r = rig().await;
    let b = another(&r).await;
    tokio::spawn(swisha::payers::watch(b.state.clone()));
    tokio::time::sleep(Duration::from_millis(200)).await;

    let key = add(&r.a, "sthlm", STHLM).await;
    let added = Instant::now();
    while b.state.payer("sthlm").is_none() {
        assert!(added.elapsed() < swisha::payers::REFRESH + Duration::from_secs(2), "the add never arrived");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(pay(&b, &key, "sthlm", "INV-1").await.0, 202);

    let body = json!({ "nickname": "Stockholm", "by": "Anna" });
    assert_eq!(call(&r.a, reqwest::Method::PATCH, "/swish/payers/sthlm", Some(&key), Some(body)).await.0, 200);
    let renamed = Instant::now();
    while b.state.payer("sthlm").is_none_or(|payer| payer.nickname != "Stockholm") {
        assert!(renamed.elapsed() < swisha::payers::REFRESH + Duration::from_secs(2), "the change never arrived");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

// Reloading every ten seconds must not rebuild what did not change: the client, the key and the
// last probe stay as they were. A change rebuilds that number alone.
#[tokio::test]
async fn a_number_that_did_not_change_is_kept_as_it_was() {
    let r = rig().await;
    add(&r.a, "sthlm", STHLM).await;
    let key = add(&r.a, "gbg", GBG).await;
    let (sthlm, gbg) = (r.a.state.payer("sthlm").expect("loaded"), r.a.state.payer("gbg").expect("loaded"));

    swisha::payers::refresh(&r.a.state).await.expect("refresh");
    assert!(std::sync::Arc::ptr_eq(&sthlm, &r.a.state.payer("sthlm").expect("loaded")), "unchanged, kept");

    let body = json!({ "nickname": "Göteborg", "by": "Anna" });
    assert_eq!(call(&r.a, reqwest::Method::PATCH, "/swish/payers/gbg", Some(&key), Some(body)).await.0, 200);
    let renamed = r.a.state.payer("gbg").expect("loaded");
    assert!(!std::sync::Arc::ptr_eq(&gbg, &renamed), "the changed number is rebuilt");
    assert_eq!(renamed.nickname, "Göteborg");
    assert!(std::sync::Arc::ptr_eq(&sthlm, &r.a.state.payer("sthlm").expect("loaded")), "the other one is not");
}

// A reload that finds nothing changed keeps the roster as it is, rather than rebuilding it: at 10,000
// numbers that was 60,000 allocations every ten seconds. A change still replaces it.
#[tokio::test]
async fn a_refresh_that_finds_nothing_changed_keeps_the_roster() {
    let r = rig().await;
    let key = add(&r.a, "sthlm", STHLM).await;
    let held = r.a.state.payers.snapshot();
    swisha::payers::refresh(&r.a.state).await.expect("refresh");
    assert!(std::sync::Arc::ptr_eq(&held, &r.a.state.payers.snapshot()), "nothing changed, nothing rebuilt");

    let body = json!({ "nickname": "Stockholm", "by": "Anna" });
    assert_eq!(call(&r.a, reqwest::Method::PATCH, "/swish/payers/sthlm", Some(&key), Some(body)).await.0, 200);
    let held = r.a.state.payers.snapshot();
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE {} SET nickname = 'Sthlm', updated_at = NOW() WHERE name = 'sthlm'",
        r.a.state.config.table_payers
    )))
    .execute(r.a.state.store.pool())
    .await
    .expect("a change made elsewhere");
    swisha::payers::refresh(&r.a.state).await.expect("refresh");
    assert!(!std::sync::Arc::ptr_eq(&held, &r.a.state.payers.snapshot()), "a change is read");
    assert_eq!(r.a.state.payer("sthlm").expect("loaded").nickname, "Sthlm");
}

// Swish calls back on whichever instance it reaches. One that has not read the store since another
// added the number still finds it there and then, rather than dropping Swish's word with a 200.
#[tokio::test]
async fn a_callback_for_a_number_added_on_another_instance_lands_at_once() {
    let r = rig().await;
    let b = another(&r).await;
    swisha::payers::refresh(&b.state).await.expect("refresh");
    let key = add(&r.a, "sthlm", STHLM).await;
    assert_eq!(pay(&r.a, &key, "sthlm", "INV-1").await.0, 202);
    assert!(b.state.payers.snapshot().name_of(STHLM).is_none(), "b has not read the store since the add");

    let uuid = r.a.state.store.snapshot("sthlm", "INV-1").await.expect("store").and_then(|s| s.swish_ref);
    let echoed = common::callback_identifier(&r.a.state, "sthlm", "INV-1").await;
    let callback = common::serve(swisha::http::callback_router().with_state(b.state.clone())).await;
    let code = reqwest::Client::new()
        .post(format!("{callback}/swish/callback"))
        .header("callbackIdentifier", echoed)
        .json(&json!({
            "payerAlias": STHLM,
            "payerPaymentReference": "INV-1",
            "payoutInstructionUUID": uuid,
            "status": "PAID",
        }))
        .send()
        .await
        .expect("b answered")
        .status()
        .as_u16();
    assert_eq!(code, 200);
    let status = r.a.state.store.snapshot("sthlm", "INV-1").await.expect("store").and_then(|s| s.status);
    assert_eq!(status.as_deref(), Some("PAID"), "Swish's word landed on the instance it reached");
}

// The callback listener is the internet's, so a payerAlias that is no Swish number costs no read of
// every number: only one shaped as a number is looked for in the store.
#[tokio::test]
async fn a_callback_naming_no_swish_number_reads_nothing() {
    let r = rig().await;
    let b = another(&r).await;
    swisha::payers::refresh(&b.state).await.expect("refresh");
    stored_without_files(&r, "late", GBG).await;
    let before = b.state.payers.snapshot();

    let callback = common::serve(swisha::http::callback_router().with_state(b.state.clone())).await;
    let code = reqwest::Client::new()
        .post(format!("{callback}/swish/callback"))
        .json(&json!({
            "payerAlias": "not-a-number",
            "payerPaymentReference": "R-1",
            "payoutInstructionUUID": "x",
            "status": "PAID",
        }))
        .send()
        .await
        .expect("b answered")
        .status()
        .as_u16();
    assert_eq!(code, 200);
    assert!(std::sync::Arc::ptr_eq(&before, &b.state.payers.snapshot()), "no number was read");
}

// A number added on one instance pays at once on another, without waiting for its next refresh:
// its key proves it is stored, so the instance that has not loaded it yet loads it there and then.
#[tokio::test]
async fn a_number_added_on_one_instance_pays_at_once_on_another() {
    let r = rig().await;
    let b = another(&r).await;
    swisha::payers::refresh(&b.state).await.expect("refresh");
    let key = add(&r.a, "sthlm", STHLM).await;
    assert!(b.state.payer("sthlm").is_none(), "b has not read the store since the add");

    let (code, answer) = pay(&b, &key, "sthlm", "INV-1").await;
    assert_eq!(code, 202, "{answer}");
    let (code, own) = call(&b, reqwest::Method::GET, "/swish/payers/sthlm", Some(&key), None).await;
    assert_eq!((code, own["usable"].clone()), (200, json!(true)), "{own}");
}

// A number stored straight into the store, as an add stores it a moment before its files are in
// place. Answers its key.
async fn stored_without_files(r: &Rig, name: &str, number: &str) -> String {
    let key = common::key(name);
    let hash = swisha::payers::key_hash(&key).expect("a test key");
    let row = swisha::domain::payer::NewPayer { name, nickname: name, number, added_by: "Anna", key_hash: &hash };
    r.a.state.store.add_payer(&row, &common::logged()).await.expect("store");
    key
}

// The files an add then puts in place: a folder of the number's own, closed to other users.
fn place_files(r: &Rig, name: &str, number: &str) {
    let folder = r.config.cert_dir.join(number);
    std::fs::create_dir(&folder).expect("the number's folder");
    std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o700)).expect("chmod");
    for (role, days) in [("tls", "30"), ("signing", "31")] {
        let file = folder.join(format!("{role}-{number}-{name}.pem"));
        std::fs::write(&file, bundle(number, days)).expect("write");
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    }
}

// An add stores its row a moment before its files are in place. An instance that read the row in
// that moment found no files, and asked for the number now reads it again rather than refusing it.
#[tokio::test]
async fn a_number_read_before_its_files_landed_is_read_again_when_asked_for() {
    let r = rig().await;
    let b = another(&r).await;
    let key = stored_without_files(&r, "sthlm", STHLM).await;
    swisha::payers::refresh(&b.state).await.expect("refresh");
    assert!(b.state.payer("sthlm").is_none(), "read before its files were in place");

    place_files(&r, "sthlm", STHLM);
    let (code, answer) = pay(&b, &key, "sthlm", "INV-1").await;
    assert_eq!(code, 202, "{answer}");
}

// Only a number changed that recently is read again. One that failed for longer is answered as it
// stands, so a broken number never costs a read of every number on each request for it.
#[tokio::test]
async fn an_older_failure_is_answered_without_reading_every_number_again() {
    let r = rig().await;
    let b = another(&r).await;
    let key = stored_without_files(&r, "sthlm", STHLM).await;
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE {} SET updated_at = NOW() - interval '1 minute' WHERE name = 'sthlm'",
        r.config.table_payers
    )))
    .execute(&r.pool)
    .await
    .expect("age the row");
    swisha::payers::refresh(&b.state).await.expect("refresh");

    let before = b.state.payers.snapshot();
    assert_eq!(pay(&b, &key, "sthlm", "INV-1").await.0, 503);
    assert!(std::sync::Arc::ptr_eq(&before, &b.state.payers.snapshot()), "no number was read again");
}

// A replacement cut off between its two moves leaves the number with no folder. The cleanup puts
// back the one moved aside, the files the row still names, and a restarted instance pays again.
#[tokio::test]
async fn a_replacement_cut_off_between_its_two_moves_is_put_back() {
    let r = rig().await;
    let key = add(&r.a, "sthlm", STHLM).await;
    let folder = r.config.cert_dir.clone();
    let tls = format!("tls-{STHLM}-sthlm.pem");
    let original = std::fs::read_to_string(folder.join(STHLM).join(&tls)).expect("the original");
    let now = swisha::util::time::unix_now();
    std::fs::rename(folder.join(STHLM), folder.join(format!(".replaced-{now}-cut"))).expect("moved aside");
    let incoming = folder.join(format!(".incoming-{now}-cut"));
    std::fs::create_dir(&incoming).expect("staged");
    std::fs::set_permissions(&incoming, std::fs::Permissions::from_mode(0o700)).expect("chmod");
    std::fs::write(incoming.join(&tls), bundle(STHLM, "60")).expect("the new file");

    let b = another(&r).await;
    tokio::spawn(swisha::payers::watch(b.state.clone()));
    let since = Instant::now();
    while !folder.join(STHLM).exists() {
        assert!(since.elapsed() < Duration::from_secs(5), "the folder moved aside never came back");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let restored = std::fs::read_to_string(folder.join(STHLM).join(&tls)).expect("the file restored");
    assert_eq!(restored, original, "the files the row still names, not the new ones");
    assert!(incoming.exists(), "what was staged is left for the hour, as any other leftover");

    swisha::payers::refresh(&b.state).await.expect("refresh");
    assert_eq!(pay(&b, &key, "sthlm", "INV-1").await.0, 202);
}

// A number whose files hold something swisha will not pay with is not read again every refresh
// while they stay as they are. Once they are put right, the next refresh loads it.
#[tokio::test]
async fn a_number_whose_files_were_wrong_loads_once_they_are_put_right() {
    let r = rig().await;
    let key = add(&r.a, "sthlm", STHLM).await;
    let file = r.config.cert_dir.join(STHLM).join(format!("tls-{STHLM}-sthlm.pem"));
    std::fs::write(&file, bundle(STHLM, "expired")).expect("an expired certificate in place");

    let b = another(&r).await;
    for _ in 0..3 {
        swisha::payers::refresh(&b.state).await.expect("refresh");
        let (_, own) = call(&b, reqwest::Method::GET, "/swish/payers/sthlm", Some(&key), None).await;
        assert!(own["problem"].as_str().is_some_and(|p| p.contains("expired")), "{own}");
    }

    std::fs::write(&file, bundle(STHLM, "30")).expect("a valid certificate in place");
    swisha::payers::refresh(&b.state).await.expect("refresh");
    assert_eq!(pay(&b, &key, "sthlm", "INV-1").await.0, 202, "put right, it pays");
}

// Every path is built from a number's Swish number and its name, each checked for its shape, so a
// row changed in the database by hand can never send a read outside the certificate folder.
#[tokio::test]
async fn a_stored_row_that_could_name_another_path_is_held_aside() {
    let r = rig().await;
    let key = add(&r.a, "sthlm", STHLM).await;
    add(&r.a, "gbg", GBG).await;
    for change in ["number = '..' WHERE name = 'sthlm'", "name = '..' WHERE name = 'gbg'"] {
        sqlx::query(sqlx::AssertSqlSafe(format!("UPDATE {} SET {change}", r.config.table_payers)))
            .execute(&r.pool)
            .await
            .expect("a row changed by hand");
    }

    let b = another(&r).await;
    swisha::payers::refresh(&b.state).await.expect("bad rows are not a failed refresh");
    let roster = b.state.payers.snapshot();
    for name in ["sthlm", ".."] {
        let problem = roster.find(name).expect("held").loaded.as_ref().err();
        assert!(problem.is_some_and(|p| p.contains("no file is read for it")), "{name}: {problem:?}");
    }
    assert_eq!(pay(&b, &key, "sthlm", "INV-1").await.0, 503, "it pays with nothing");
}

// A write cut short, by a crash or a power cut, leaves its folder of keys behind. Once it is an
// hour old it is removed; anything newer may be another instance's write still in progress.
#[tokio::test]
async fn a_write_cut_short_is_removed_once_it_is_an_hour_old() {
    let r = rig().await;
    let key = add(&r.a, "sthlm", STHLM).await;
    let folder = &r.config.cert_dir;
    let now = swisha::util::time::unix_now();
    let two_hours_ago = now - 2 * 60 * 60;
    let left = [".incoming", ".replaced"].map(|kind| folder.join(format!("{kind}-{two_hours_ago}-1")));
    let kept = [
        folder.join(format!(".incoming-{now}-2")),
        folder.join(".incoming-x"),
        folder.join(format!(".other-{two_hours_ago}-3")),
    ];
    for dir in left.iter().chain(&kept) {
        std::fs::create_dir(dir).expect("a leftover");
        std::fs::write(dir.join(format!("tls-{STHLM}-sthlm.pem")), bundle(STHLM, "30")).expect("a key");
    }
    // Named as a leftover, a link goes, and never what it points to.
    let link = folder.join(format!(".incoming-{two_hours_ago}-4"));
    std::os::unix::fs::symlink(folder.join(STHLM), &link).expect("a link");

    tokio::spawn(swisha::payers::watch(r.a.state.clone()));
    let started = Instant::now();
    while left.iter().chain([&link]).any(|path| std::fs::symlink_metadata(path).is_ok()) {
        assert!(started.elapsed() < Duration::from_secs(5), "the leftovers were never removed");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    for dir in &kept {
        assert!(dir.exists(), "{} was removed", dir.display());
    }
    assert!(folder.join(STHLM).join(format!("tls-{STHLM}-sthlm.pem")).exists(), "the number's own files stay");
    assert_eq!(pay(&r.a, &key, "sthlm", "INV-1").await.0, 202);
}

// The add waits on a lock on the numbers' table, between writing its files and storing its row.
// Its folder is dated now, so a cleanup running meanwhile leaves it be, and the add completes.
#[tokio::test]
async fn a_write_in_progress_is_dated_and_left_alone() {
    let r = rig().await;
    let folder = r.config.cert_dir.clone();
    let mut hold = r.pool.begin().await.expect("a transaction");
    sqlx::query(sqlx::AssertSqlSafe(format!("LOCK TABLE {} IN EXCLUSIVE MODE", r.config.table_payers)))
        .execute(&mut *hold)
        .await
        .expect("the table locked");

    let body = json!({
        "name": "sthlm", "nickname": "sthlm", "number": STHLM,
        "tls": bundle(STHLM, "30"), "signing": bundle(STHLM, "31"), "by": "Anna",
    });
    let url = format!("{}/swish/payers", r.a.base);
    let adding = tokio::spawn(async move {
        let request = reqwest::Client::new().post(url).header("x-api-secret", SECRET).json(&body);
        request.send().await.expect("request").status().as_u16()
    });
    let staged = || {
        std::fs::read_dir(&folder)
            .expect("the folder")
            .map(|entry| entry.expect("entry").file_name().to_string_lossy().to_string())
            .find(|name| name.starts_with(".incoming-"))
    };
    let started = Instant::now();
    let name = loop {
        if let Some(name) = staged() {
            break name;
        }
        assert!(started.elapsed() < Duration::from_secs(5), "the add never staged its files");
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    let dated = name.strip_prefix(".incoming-").and_then(|rest| rest.split_once('-')?.0.parse::<i64>().ok());
    let now = swisha::util::time::unix_now();
    assert!(dated.is_some_and(|at| (now - 60..=now).contains(&at)), "{name} is dated now");

    tokio::spawn(swisha::payers::watch(r.a.state.clone()));
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(staged().as_deref(), Some(name.as_str()), "the cleanup left the write in progress alone");

    hold.rollback().await.expect("the lock released");
    assert_eq!(adding.await.expect("the add"), 201);
    assert!(folder.join(STHLM).join(format!("signing-{STHLM}-sthlm.pem")).exists(), "its files are in place");
    assert_eq!(staged(), None, "and nothing is left staged");
}
