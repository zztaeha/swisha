#![cfg(feature = "http")]

// The numbers swisha pays from, added and changed through the API. Is a number swisha could not
// pay with refused before anything is kept, and can any change let a reference pay twice?

mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use common::{MockSwish, PHONE, SECRET, SSN, bundle};
use serde_json::{Value, json};
use swisha::state::SharedState;
use swisha::store::PayoutStore;

const NUMBER: &str = "1234679304";

// Two distinct pairs for the test number, as Swish issues: one to authenticate, one to sign.
fn tls() -> String {
    bundle(NUMBER, "30")
}

fn signing() -> String {
    bundle(NUMBER, "31")
}

struct Rig {
    base: String,
    callback: String,
    state: SharedState,
    swish: MockSwish,
}

// No number loaded yet: every one a test pays from, it adds through the API.
async fn rig() -> Rig {
    let swish = MockSwish::new().accepts().resolves_to("CREATED");
    let mut config = common::config();
    config.swish_base_url = swish.clone().start().await;
    let state = common::state_with_payers(config, Vec::new()).await;
    Rig {
        base: common::serve(swisha::http::internal_router().with_state(state.clone())).await,
        callback: common::serve(swisha::http::callback_router().with_state(state.clone())).await,
        state,
        swish,
    }
}

async fn call(r: &Rig, method: reqwest::Method, path: &str, key: Option<&str>, body: Option<Value>) -> (u16, Value) {
    let mut request = reqwest::Client::new()
        .request(method, format!("{}{path}", r.base))
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

fn new_payer(name: &str, number: &str) -> Value {
    json!({ "name": name, "nickname": "Stockholm refunds", "number": number, "tls": tls(), "signing": signing(), "by": "Anna" })
}

async fn add(r: &Rig, body: Value) -> (u16, Value) {
    call(r, reqwest::Method::POST, "/swish/payers", None, Some(body)).await
}

// Adds sthlm and answers its key.
async fn added(r: &Rig) -> String {
    let (code, answer) = add(r, new_payer("sthlm", NUMBER)).await;
    assert_eq!(code, 201, "{answer}");
    answer["key"].as_str().expect("the key").to_string()
}

async fn pay(r: &Rig, key: &str, payer: &str, reference: &str) -> (u16, Value) {
    let body = json!({ "payer": payer, "reference": reference, "payee_alias": PHONE, "payee_ssn": SSN, "amount": 100.0 });
    call(r, reqwest::Method::POST, "/swish/payout", Some(key), Some(body)).await
}

fn folder(r: &Rig) -> &Path {
    &r.state.config.cert_dir
}

fn entries(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .expect("read the folder")
        .map(|entry| entry.expect("entry").file_name().to_string_lossy().to_string())
        .collect();
    names.sort();
    names
}

// Where swisha keeps the number's files: a folder named for its Swish number, each file named for
// what it is, the number and the name.
fn tls_file(r: &Rig) -> PathBuf {
    folder(r).join(NUMBER).join(format!("tls-{NUMBER}-sthlm.pem"))
}

fn signing_file(r: &Rig) -> PathBuf {
    folder(r).join(NUMBER).join(format!("signing-{NUMBER}-sthlm.pem"))
}

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).expect("metadata").permissions().mode() & 0o777
}

async fn logs(r: &Rig) -> Vec<String> {
    sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT message FROM {} ORDER BY id", r.state.config.table_logs)))
        .fetch_all(r.state.store.pool())
        .await
        .expect("logs")
}

// Adding

// A caller who gives up waiting cannot stop an add halfway: the number is stored, its files put in
// place, the change logged and Swish probed, as for a caller who waited. Only the answer is lost.
#[tokio::test]
async fn an_add_whose_caller_leaves_is_finished_all_the_same() {
    // Swish takes every connection and never answers, so the add's probe waits its whole 5 seconds.
    let silent = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let mut config = common::config();
    config.swish_base_url = format!("http://{}", silent.local_addr().expect("addr"));
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((socket, _)) = silent.accept().await {
            held.push(socket);
        }
    });
    let state = common::state_with_payers(config, Vec::new()).await;
    let base = common::serve(swisha::http::internal_router().with_state(state.clone())).await;

    let left = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(1))
        .build()
        .expect("client")
        .post(format!("{base}/swish/payers"))
        .header("x-api-secret", SECRET)
        .json(&new_payer("sthlm", NUMBER))
        .send()
        .await;
    assert!(left.is_err(), "the caller left before the answer");

    let since = std::time::Instant::now();
    while !(match state.payer("sthlm") {
        Some(payer) => payer.probe.read().await.is_some(),
        None => false,
    }) {
        assert!(since.elapsed() < std::time::Duration::from_secs(8), "the add stopped with its caller");
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(state.config.cert_dir.join(NUMBER).join(format!("tls-{NUMBER}-sthlm.pem")).exists());
    let logged: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT message FROM {}",
        state.config.table_logs
    )))
    .fetch_all(state.store.pool())
    .await
    .expect("logs");
    assert!(logged.contains(&"Payer sthlm added by Anna".to_string()), "{logged:?}");
}

#[tokio::test]
async fn a_number_is_added_kept_in_its_own_folder_and_pays() {
    let r = rig().await;
    let (code, body) = add(&r, new_payer("sthlm", NUMBER)).await;
    assert_eq!(code, 201, "{body}");
    let key = body["key"].as_str().expect("the key, once").to_string();
    for (field, expected) in [
        ("name", json!("sthlm")),
        ("nickname", json!("Stockholm refunds")),
        ("number", json!(NUMBER)),
        ("disabled", json!(false)),
        ("usable", json!(true)),
        ("problem", Value::Null),
        ("added_by", json!("Anna")),
        ("swish_online", json!(true)),
    ] {
        assert_eq!(body[field], expected, "{field}: {body}");
    }
    assert!(body["tls_expires_in_days"].as_i64().is_some_and(|days| (28..=30).contains(&days)), "{body}");
    assert!(body["signing_expires"].as_str().is_some_and(|date| date.len() == 10), "{body}");

    let own = folder(&r).join(NUMBER);
    assert_eq!(entries(folder(&r)), [NUMBER], "one folder per Swish number, nothing left over");
    let expected = [format!("signing-{NUMBER}-sthlm.pem"), format!("tls-{NUMBER}-sthlm.pem")];
    assert_eq!(entries(&own), expected, "two files, each a certificate and its key");
    assert_eq!(mode(&own), 0o700, "the number's folder is swisha's alone");
    assert_eq!(mode(&tls_file(&r)), 0o600);
    assert_eq!(mode(&signing_file(&r)), 0o600);
    assert_eq!(std::fs::read_to_string(tls_file(&r)).expect("read"), tls());
    assert_eq!(std::fs::read_to_string(signing_file(&r)).expect("read"), signing());

    // Days left are counted at each read, so a second can move them across a day. The key is in
    // the add's answer alone.
    let (code, mut read) = call(&r, reqwest::Method::GET, "/swish/payers/sthlm", Some(&key), None).await;
    assert_eq!(code, 200);
    let mut added = body.clone();
    added.as_object_mut().expect("object").remove("key");
    for field in ["tls_expires_in_days", "signing_expires_in_days"] {
        added[field] = Value::Null;
        read[field] = Value::Null;
    }
    assert_eq!(read, added, "a read shows what the add answered, without the key");

    assert_eq!(pay(&r, &key, "sthlm", "INV-1").await.0, 202);
    let sent = common::payload(&r.swish.posted()[0]);
    assert_eq!(sent["payerAlias"], NUMBER);
    let serial = swisha::swish::cert::read_cert_serial(&signing()).expect("serial");
    assert_eq!(sent["signingCertificateSerialNumber"], serial, "signed with the uploaded signing key's serial");
    assert!(logs(&r).await.contains(&"Payer sthlm added by Anna".to_string()));
}

// The key is shown once, and every later request about the number needs it. A number stored but
// not read back is still answered with its key, or it could only be reached again on the server.
#[tokio::test]
async fn a_stored_number_that_cannot_be_read_back_still_gets_its_key() {
    let r = rig().await;
    let (pool, payers) = (r.state.store.pool().clone(), r.state.config.table_payers.clone());
    let rename = |from: &str, to: &str| {
        let (pool, statement) = (pool.clone(), format!("ALTER TABLE {payers} RENAME COLUMN {from} TO {to}"));
        async move { sqlx::query(sqlx::AssertSqlSafe(statement)).execute(&pool).await.expect("alter") }
    };
    rename("disabled_at", "unreadable").await;

    let (code, body) = add(&r, new_payer("sthlm", NUMBER)).await;
    assert_eq!(code, 201, "the number was stored: {body}");
    assert_eq!((body["name"].as_str(), body["usable"].as_bool()), (Some("sthlm"), Some(false)), "{body}");
    let key = body["key"].as_str().expect("the key, once").to_string();

    rename("unreadable", "disabled_at").await;
    let (code, read) = call(&r, reqwest::Method::GET, "/swish/payers/sthlm", Some(&key), None).await;
    assert_eq!((code, read["usable"].as_bool()), (200, Some(true)), "{read}");
}

// A leftover folder that cannot be moved aside: the number is stored without usable files, and its
// key uploads them again. macOS only, since a flag that stops a rename needs root on Linux.
#[cfg(target_os = "macos")]
#[tokio::test]
async fn a_stored_number_whose_files_could_not_be_put_in_place_still_gets_its_key() {
    struct Unpin(std::path::PathBuf);
    impl Drop for Unpin {
        fn drop(&mut self) {
            let _ = std::process::Command::new("chflags").arg("nouchg").arg(&self.0).status();
        }
    }
    let r = rig().await;
    let leftover = folder(&r).join(NUMBER);
    std::fs::create_dir(&leftover).expect("leftover folder");
    let pinned = Unpin(leftover.clone());
    let flagged = std::process::Command::new("chflags").arg("uchg").arg(&leftover).status().expect("chflags");
    assert!(flagged.success());

    let (code, body) = add(&r, new_payer("sthlm", NUMBER)).await;
    assert_eq!(code, 201, "the number was stored: {body}");
    assert_eq!(body["usable"], json!(false), "{body}");
    assert!(body["problem"].as_str().is_some_and(|p| p.contains("PUT /swish/payers/sthlm/certificates")), "{body}");
    let key = body["key"].as_str().expect("the key, once").to_string();

    drop(pinned);
    let files = json!({ "tls": tls(), "signing": signing(), "by": "Anna" });
    let (code, read) = call(&r, reqwest::Method::PUT, "/swish/payers/sthlm/certificates", Some(&key), Some(files)).await;
    assert_eq!((code, read["usable"].as_bool()), (200, Some(true)), "{read}");
}

// The other half: the folder there moves aside, but the new one cannot take its place, so the old
// one goes back. The add waits on a lock between staging and moving, so the staged folder is pinned.
#[cfg(target_os = "macos")]
#[tokio::test]
async fn a_folder_moved_aside_is_put_back_when_the_new_one_cannot_take_its_place() {
    let r = rig().await;
    let target = folder(&r).join(NUMBER);
    std::fs::create_dir(&target).expect("a folder in the number's place");
    std::fs::write(target.join("marker"), "there before").expect("marker");
    let mut hold = r.state.store.pool().begin().await.expect("a transaction");
    sqlx::query(sqlx::AssertSqlSafe(format!("LOCK TABLE {} IN EXCLUSIVE MODE", r.state.config.table_payers)))
        .execute(&mut *hold)
        .await
        .expect("lock");

    let (url, body) = (format!("{}/swish/payers", r.base), new_payer("sthlm", NUMBER));
    let adding = tokio::spawn(async move {
        let response = reqwest::Client::new().post(url).header("x-api-secret", SECRET).json(&body).send().await;
        response.expect("answered").json::<Value>().await.expect("json")
    });
    let since = std::time::Instant::now();
    let staged = loop {
        let found = entries(folder(&r)).into_iter().find(|name| name.starts_with(".incoming-"));
        if let Some(name) = found.filter(|name| entries(&folder(&r).join(name)).len() == 2) {
            break folder(&r).join(name);
        }
        assert!(since.elapsed() < std::time::Duration::from_secs(5), "nothing was staged");
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    };
    assert!(std::process::Command::new("chflags").arg("uchg").arg(&staged).status().expect("chflags").success());
    hold.rollback().await.expect("the lock released");
    let answer = adding.await.expect("join");
    let _ = std::process::Command::new("chflags").arg("nouchg").arg(&staged).status();
    let _ = std::fs::remove_dir_all(&staged);

    assert!(answer["problem"].as_str().is_some_and(|p| p.contains("could not be put in place")), "{answer}");
    assert!(target.join("marker").exists(), "the folder moved aside was put back");
    assert!(!entries(folder(&r)).iter().any(|name| name.starts_with(".replaced-")), "and nothing is left aside");
}

// An older certificate does both jobs. The same file for both roles keeps it working.
#[tokio::test]
async fn one_file_may_serve_both_roles() {
    let r = rig().await;
    let mut body = new_payer("sthlm", NUMBER);
    body["signing"] = json!(tls());
    let (code, answer) = add(&r, body).await;
    assert_eq!(code, 201, "{answer}");
}

// Whatever swisha could not pay with is refused before anything is kept: no row, no file.
#[tokio::test]
async fn every_field_and_certificate_is_checked_before_anything_is_kept() {
    let r = rig().await;
    let pair = tls();
    let (cert_only, key_only) = pair.split_at(pair.find("-----BEGIN PRIVATE KEY-----").expect("key"));
    let other = bundle("1239999999", "30");
    let mismatched = format!("{cert_only}{}", &other[other.find("-----BEGIN PRIVATE KEY-----").expect("key")..]);
    let expired = bundle(NUMBER, "expired");
    let long = "a".repeat(33);

    let mut cases: Vec<(&str, Value, u16, &str)> = Vec::new();
    for name in ["Sthlm", "../sthlm", "a/b", "", long.as_str()] {
        cases.push(("name", json!(name), 400, "name must be"));
    }
    for number in ["123 467 93 04", "123467930", "+461234679304", ""] {
        cases.push(("number", json!(number), 400, "number must be"));
    }
    cases.push(("nickname", json!("  "), 400, "nickname must be"));
    cases.push(("nickname", json!("Stockholm\nrefunds"), 400, "nickname must be"));
    cases.push(("nickname", json!("Stockholm\u{2028}refunds"), 400, "invisible characters"));
    cases.push(("by", json!("Anna\u{202E}gnaB"), 400, "invisible characters"));
    cases.push(("by", json!(""), 400, "by must be"));
    cases.push(("tls", json!(cert_only), 400, "tls: no private key"));
    cases.push(("signing", json!(key_only), 400, "signing: no CERTIFICATE"));
    cases.push(("tls", json!(mismatched), 400, "not a pair"));
    cases.push(("signing", json!(expired), 400, "expired on 2020-01-02"));
    cases.push(("tls", json!(other), 400, "belongs to Swish number 1239999999"));
    cases.push(("status", json!("ready"), 422, "unknown field"));

    for (field, value, code, says) in cases {
        let mut body = new_payer("sthlm", NUMBER);
        body[field] = value.clone();
        let (got, answer) = add(&r, body).await;
        assert_eq!(got, code, "{field} = {value}: {answer}");
        let text = answer["error"].as_str().unwrap_or_default();
        assert!(text.contains(says), "{field} = {value}: {text}");
    }
    assert!(r.state.store.payers().await.expect("store").is_empty(), "nothing was stored");
    assert!(entries(folder(&r)).is_empty(), "nothing was written: {:?}", entries(folder(&r)));
}

// The name is the double-payout key and the number is how a callback finds its payer: each once.
#[tokio::test]
async fn a_name_or_a_number_is_stored_once() {
    let r = rig().await;
    assert_eq!(add(&r, new_payer("sthlm", NUMBER)).await.0, 201);
    let before = std::fs::read_to_string(tls_file(&r)).expect("read");

    let mut again = new_payer("sthlm", "1239999999");
    again["tls"] = json!(bundle("1239999999", "30"));
    again["signing"] = json!(bundle("1239999999", "30"));
    let (code, answer) = add(&r, again).await;
    assert_eq!(code, 409, "{answer}");
    assert!(answer["error"].as_str().unwrap_or_default().contains("already stored under the name sthlm"), "{answer}");

    let (code, answer) = add(&r, new_payer("gbg", NUMBER)).await;
    assert_eq!(code, 409, "{answer}");
    let error = answer["error"].as_str().unwrap_or_default();
    assert!(error.contains("already stored, under another name") && !error.contains("sthlm"), "{error}");

    assert_eq!(std::fs::read_to_string(tls_file(&r)).expect("read"), before, "untouched");
    assert_eq!(entries(folder(&r)), [NUMBER], "the refused adds left nothing behind");
}

// INV-1 paid as sthlm from this number. Added under any other name, INV-1 would pay again, so the
// number is only ever added as sthlm, and INV-1 stays spent.
#[tokio::test]
async fn a_number_that_paid_under_one_name_is_only_ever_added_under_it() {
    let r = rig().await;
    r.state
        .store
        .claim(&swisha::domain::payout::NewPayout {
            payer: "sthlm",
            reference: "INV-1",
            payer_alias: NUMBER,
            payee_alias: "46701234567",
            payee_ssn: Some(SSN),
            amount: 100.0,
            message: "INV-1",
            swish_ref: "AAAABBBBCCCCDDDDEEEEFFFF00000001",
            callback_identifier: None,
        })
        .await
        .expect("claim");

    let (code, answer) = add(&r, new_payer("stockholm", NUMBER)).await;
    assert_eq!(code, 409, "{answer}");
    let error = answer["error"].as_str().unwrap_or_default();
    assert!(error.contains("paid out under another name") && error.contains("double-payout"), "{error}");
    assert!(!error.contains("sthlm"), "the answer never names another number: {error}");
    assert!(entries(folder(&r)).is_empty(), "nothing was written");

    let key = added(&r).await;
    let (code, answer) = pay(&r, &key, "sthlm", "INV-1").await;
    assert_eq!(code, 409, "INV-1 is still spent: {answer}");
    assert!(r.swish.posted().is_empty(), "and nothing was sent");
}

#[tokio::test]
async fn two_adds_of_one_name_at_once_store_one() {
    let r = rig().await;
    let numbers = ["1230000001", "1230000002", "1230000003", "1230000004", "1230000005", "1230000006"];
    let mut presses = Vec::new();
    for number in numbers {
        let mut body = new_payer("sthlm", number);
        body["tls"] = json!(bundle(number, "30"));
        body["signing"] = json!(bundle(number, "30"));
        let url = format!("{}/swish/payers", r.base);
        presses.push(tokio::spawn(async move {
            reqwest::Client::new()
                .post(url)
                .header("x-api-secret", SECRET)
                .json(&body)
                .send()
                .await
                .expect("request")
                .status()
                .as_u16()
        }));
    }
    let mut codes = Vec::new();
    for press in presses {
        codes.push(press.await.expect("join"));
    }
    codes.sort_unstable();
    assert_eq!(codes, [201, 409, 409, 409, 409, 409]);
    let stored = r.state.store.payers().await.expect("store");
    assert_eq!(stored.len(), 1);
    assert_eq!(entries(folder(&r)), [stored[0].number.as_str()], "one folder, and no write left behind");
}

// Changing

// Who changed a number, and from where, is written with the change or the change is not made: a
// key replaced or a number disabled with no line saying by whom could never be accounted for.
#[tokio::test]
async fn a_change_to_a_number_is_written_with_its_log_line_or_not_at_all() {
    let r = rig().await;
    let key = added(&r).await;
    let table = r.state.config.table_logs.clone();
    let by = json!({ "by": "Mallory" });

    common::refuse(&r.state, &table, "TRUE").await;
    // What failed goes to the server's log, never to the caller: the answer names no table.
    let unnamed = |answer: &Value| {
        let error = answer["error"].as_str().unwrap_or_default();
        !error.is_empty() && !error.contains(&table) && !error.contains("violates")
    };
    let (code, answer) = call(&r, reqwest::Method::POST, "/swish/payers/sthlm/key", Some(&key), Some(by.clone())).await;
    assert_eq!(code, 503, "{answer}");
    assert!(unnamed(&answer), "{answer}");
    let disable = "/swish/payers/sthlm/disable";
    let (code, answer) = call(&r, reqwest::Method::POST, disable, Some(&key), Some(by.clone())).await;
    assert_eq!(code, 503, "{answer}");
    assert!(unnamed(&answer), "{answer}");
    let (code, read) = call(&r, reqwest::Method::GET, "/swish/payers/sthlm", Some(&key), None).await;
    assert_eq!((code, read["disabled"].clone()), (200, json!(false)), "the old key works, and nothing changed: {read}");
    let renewed = json!({ "tls": tls(), "signing": bundle(NUMBER, "60"), "by": "Mallory" });
    let path = "/swish/payers/sthlm/certificates";
    let (code, answer) = call(&r, reqwest::Method::PUT, path, Some(&key), Some(renewed)).await;
    assert_eq!(code, 503, "{answer}");
    assert!(unnamed(&answer), "{answer}");
    let on_disk = std::fs::read_to_string(signing_file(&r)).expect("the signing file");
    assert_eq!(on_disk, signing(), "the files are the ones the number had");
    assert_eq!(entries(&folder(&r).join(NUMBER)).len(), 2, "and nothing else is left in its folder");
    let other = json!({
        "name": "gbg", "nickname": "gbg", "number": "1239999999",
        "tls": bundle("1239999999", "30"), "signing": bundle("1239999999", "31"), "by": "Mallory",
    });
    let (code, answer) = add(&r, other).await;
    assert_eq!(code, 503, "{answer}");
    assert!(unnamed(&answer), "{answer}");
    assert!(r.state.store.payers().await.expect("store").iter().all(|payer| payer.name != "gbg"), "nothing stored");

    common::allow(&r.state, &table).await;
    assert_eq!(call(&r, reqwest::Method::POST, disable, Some(&key), Some(by.clone())).await.0, 200);
    let (code, answer) = call(&r, reqwest::Method::POST, "/swish/payers/sthlm/key", Some(&key), Some(by)).await;
    assert_eq!(code, 200, "{answer}");
    let lines = logs(&r).await;
    for line in ["Payer sthlm disabled by Mallory", "Payer sthlm key replaced by Mallory"] {
        assert_eq!(lines.iter().filter(|written| *written == line).count(), 1, "{line}: {lines:?}");
    }
}

#[tokio::test]
async fn certificates_are_replaced_and_the_new_ones_used() {
    let r = rig().await;
    let key = added(&r).await;
    let replace = |path: &'static str, body: Value| call(&r, reqwest::Method::PUT, path, Some(&key), Some(body));
    let renewed = bundle(NUMBER, "60");
    let body = json!({ "tls": tls(), "signing": renewed, "by": "Bo" });
    let (code, answer) = replace("/swish/payers/sthlm/certificates", body).await;
    assert_eq!(code, 200, "{answer}");
    assert!(answer["signing_expires_in_days"].as_i64().is_some_and(|days| days >= 58), "{answer}");
    assert_eq!(std::fs::read_to_string(signing_file(&r)).expect("read"), renewed);
    assert_eq!(entries(folder(&r)), [NUMBER], "the old files are gone, and nothing is left over");
    assert_eq!(entries(&folder(&r).join(NUMBER)).len(), 2, "replaced in place, under the same names");

    assert_eq!(pay(&r, &key, "sthlm", "INV-2").await.0, 202);
    let serial = swisha::swish::cert::read_cert_serial(&renewed).expect("serial");
    assert_eq!(common::payload(&r.swish.posted()[0])["signingCertificateSerialNumber"], serial, "the new key signs");
    assert!(logs(&r).await.contains(&"Payer sthlm certificates replaced by Bo".to_string()));

    let other = bundle("1239999999", "30");
    let wrong = json!({ "tls": other, "signing": other, "by": "Bo" });
    let (code, answer) = replace("/swish/payers/sthlm/certificates", wrong).await;
    assert_eq!(code, 400, "another number's certificate: {answer}");
    assert_eq!(std::fs::read_to_string(signing_file(&r)).expect("read"), renewed, "kept");

    // sthlm's key reaches no other name, a path that would climb out of the folder included.
    let body = json!({ "tls": tls(), "signing": tls(), "by": "Bo" });
    assert_eq!(replace("/swish/payers/gbg/certificates", body.clone()).await.0, 403);
    assert_eq!(replace("/swish/payers/..%2Fsthlm/certificates", body).await.0, 403);
}

#[tokio::test]
async fn a_nickname_changes_and_the_name_and_number_never_do() {
    let r = rig().await;
    let key = added(&r).await;
    let rename = |body: Value| call(&r, reqwest::Method::PATCH, "/swish/payers/sthlm", Some(&key), Some(body));

    let (code, answer) = rename(json!({ "nickname": "Stockholm, city 1", "by": "Anna" })).await;
    assert_eq!(code, 200, "{answer}");
    assert_eq!(answer["nickname"], "Stockholm, city 1");
    assert_eq!((answer["name"].clone(), answer["number"].clone()), (json!("sthlm"), json!(NUMBER)));

    for (body, code) in [
        (json!({ "nickname": "x", "by": "Anna", "name": "stockholm" }), 422),
        (json!({ "nickname": "x", "by": "Anna", "number": "1239999999" }), 422),
        (json!({ "nickname": "", "by": "Anna" }), 400),
        (json!({ "nickname": "x", "by": "" }), 400),
    ] {
        assert_eq!(rename(body.clone()).await.0, code, "{body}");
    }
    let other = call(&r, reqwest::Method::PATCH, "/swish/payers/gbg", Some(&key), Some(json!({ "nickname": "x", "by": "A" })));
    assert_eq!(other.await.0, 403);
    assert!(logs(&r).await.contains(&"Payer sthlm nickname changed to Stockholm, city 1 by Anna".to_string()));
}

// Disabling stops new payouts at once. The payouts already made still settle: their callbacks and
// checks need the number's certificate for as long as Swish may answer.
#[tokio::test]
async fn a_disabled_number_takes_no_payouts_but_settles_the_ones_it_made() {
    let r = rig().await;
    let key = added(&r).await;
    let (_, made) = pay(&r, &key, "sthlm", "INV-1").await;
    let uuid = made["swish_ref"].as_str().expect("uuid").to_string();

    let switch = |path: &'static str, body: Value| call(&r, reqwest::Method::POST, path, Some(&key), Some(body));
    let (code, answer) = switch("/swish/payers/sthlm/disable", json!({ "by": "Anna", "note": "Customer left" })).await;
    assert_eq!(code, 200, "{answer}");
    assert_eq!(answer["disabled"], true);

    let (code, answer) = pay(&r, &key, "sthlm", "INV-2").await;
    assert_eq!(code, 400, "{answer}");
    assert!(answer["error"].as_str().unwrap_or_default().contains("disabled"), "{answer}");
    assert!(r.state.store.snapshot("sthlm", "INV-2").await.expect("store").is_none(), "nothing stored");

    let echoed = common::callback_identifier(&r.state, "sthlm", "INV-1").await;
    let settled = reqwest::Client::new()
        .post(format!("{}/swish/callback", r.callback))
        .header("callbackIdentifier", echoed)
        .json(&json!({ "payerAlias": NUMBER, "payerPaymentReference": "INV-1", "payoutInstructionUUID": uuid, "status": "PAID" }))
        .send()
        .await
        .expect("callback")
        .status()
        .as_u16();
    assert_eq!(settled, 200);
    let status = r.state.store.snapshot("sthlm", "INV-1").await.expect("store").and_then(|s| s.status);
    assert_eq!(status.as_deref(), Some("PAID"), "a disabled number's payout still settles");

    let (code, answer) = switch("/swish/payers/sthlm/enable", json!({ "by": "Anna" })).await;
    assert_eq!(code, 200, "{answer}");
    assert_eq!(pay(&r, &key, "sthlm", "INV-2").await.0, 202, "the reference was never spent");

    let trail = logs(&r).await;
    assert!(trail.contains(&"Payer sthlm disabled by Anna".to_string()), "{trail:?}");
    assert!(trail.contains(&"Payer sthlm enabled by Anna".to_string()), "{trail:?}");
    assert_eq!(switch("/swish/payers/gbg/disable", json!({ "by": "Anna" })).await.0, 403);
}
