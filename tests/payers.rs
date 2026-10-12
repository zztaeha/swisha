#![cfg(feature = "http")]

// Many Swish numbers in one process, fifteen here, each with its own key, serial and client. Every
// test here asks one thing: can anything of one city's leak into another's payout?

mod common;

use std::sync::{Arc, OnceLock};

use common::{MockSwish, PHONE, SECRET, SSN};
use ring::signature::{RSA_PKCS1_2048_8192_SHA512, RsaKeyPair, UnparsedPublicKey};
use serde_json::{Value, json};
use swisha::state::{PayerState, SharedState};
use swisha::store::PayoutStore;

const PAYERS: usize = 15;

fn name(i: usize) -> String {
    format!("city{i:02}")
}

fn number(i: usize) -> String {
    format!("12300000{i:02}")
}

fn serial(i: usize) -> String {
    format!("5E1A{i:02}")
}

// Fifteen distinct keys of the size Swish issues, generated in parallel once per run.
fn keys() -> &'static Vec<Arc<RsaKeyPair>> {
    static KEYS: OnceLock<Vec<Arc<RsaKeyPair>>> = OnceLock::new();
    KEYS.get_or_init(|| {
        let workers: Vec<_> = (0..PAYERS)
            .map(|_| {
                std::thread::spawn(|| {
                    let out = std::process::Command::new("openssl")
                        .args(["genpkey", "-algorithm", "RSA", "-pkeyopt", "rsa_keygen_bits:4096"])
                        .output()
                        .expect("openssl");
                    assert!(out.status.success(), "openssl genpkey failed");
                    let pem = String::from_utf8(out.stdout).expect("PEM");
                    Arc::new(swisha::swish::sign::rsa_key_pair(&pem).expect("a 4096-bit key"))
                })
            })
            .collect();
        workers.into_iter().map(|w| w.join().expect("keygen thread")).collect()
    })
}

struct Rig {
    base: String,
    callback: String,
    state: SharedState,
    swish: MockSwish,
}

async fn rig() -> Rig {
    rig_with(MockSwish::new().accepts().resolves_to("PAID")).await
}

async fn rig_with(swish: MockSwish) -> Rig {
    let url = swish.clone().start().await;
    let mut config = common::config();
    config.swish_base_url = url;

    // Each client carries a header naming its payer, so the mock can tell whose client called.
    let payers: Vec<PayerState> = (0..PAYERS)
        .map(|i| {
            let mut payer = common::payer(&name(i), &number(i));
            payer.signing = common::certificate(&serial(i));
            let mut headers = reqwest::header::HeaderMap::new();
            headers.insert("x-test-payer", name(i).parse().unwrap());
            PayerState {
                client: reqwest::Client::builder().default_headers(headers).build().unwrap(),
                signing_key: keys()[i].clone(),
                ..common::payer_state(&payer)
            }
        })
        .collect();

    let pool = common::pool_with_schema(&config).await;
    let state = common::state_over(pool, config, payers).await;
    Rig {
        base: common::serve(swisha::http::internal_router().with_state(state.clone())).await,
        callback: common::serve(swisha::http::callback_router().with_state(state.clone())).await,
        state,
        swish,
    }
}

// With the key of the payer the body names.
async fn pay(r: &Rig, body: Value) -> (u16, Value) {
    let payer = body["payer"].as_str().expect("a body naming its payer").to_string();
    pay_with(r, &payer, body).await
}

async fn pay_with(r: &Rig, key_of: &str, body: Value) -> (u16, Value) {
    let response = reqwest::Client::new()
        .post(format!("{}/swish/payout", r.base))
        .header("x-api-secret", SECRET)
        .header(common::KEY_HEADER, common::key(key_of))
        .json(&body)
        .send()
        .await
        .expect("request");
    let code = response.status().as_u16();
    (code, response.json().await.unwrap_or(Value::Null))
}

fn payout(payer: Option<&str>, reference: &str) -> Value {
    let mut body = json!({ "reference": reference, "payee_alias": PHONE, "payee_ssn": SSN, "amount": 100.0 });
    if let Some(payer) = payer {
        body["payer"] = json!(payer);
    }
    body
}

// Every payer pays the same reference at once. Each instruction must carry its payer's number
// and serial, be signed with its payer's key alone, and be sent by its payer's own client.
#[tokio::test]
async fn fifteen_payers_each_pay_from_their_own_number_with_their_own_key() {
    let r = rig().await;

    let mut tasks = Vec::new();
    for i in 0..PAYERS {
        let body = payout(Some(&name(i)), "INV-1042");
        let base = r.base.clone();
        let key = common::key(&name(i));
        tasks.push(tokio::spawn(async move {
            reqwest::Client::new()
                .post(format!("{base}/swish/payout"))
                .header("x-api-secret", SECRET)
                .header(common::KEY_HEADER, key)
                .json(&body)
                .send()
                .await
                .expect("request")
                .status()
                .as_u16()
        }));
    }
    for t in tasks {
        assert_eq!(t.await.expect("join"), 202, "the same reference under each payer is its own payout");
    }

    #[derive(serde::Deserialize)]
    struct Sent<'a> {
        #[serde(borrow)]
        payload: &'a serde_json::value::RawValue,
        signature: String,
    }

    let raw = r.swish.raw_posts();
    let by = r.swish.posts_by();
    assert_eq!(raw.len(), PAYERS, "one instruction per payer");
    let mut seen = std::collections::HashSet::new();
    for (body, client) in raw.iter().zip(&by) {
        let sent: Sent = serde_json::from_str(body).expect("request body");
        let payload: Value = serde_json::from_str(sent.payload.get()).expect("payload");
        let i = (0..PAYERS)
            .find(|&i| payload["payerAlias"] == number(i))
            .unwrap_or_else(|| panic!("unknown payerAlias in {payload}"));
        assert!(seen.insert(i), "payer {i} sent twice");

        assert_eq!(payload["signingCertificateSerialNumber"], serial(i), "its own serial");
        assert_eq!(client.as_deref(), Some(name(i).as_str()), "sent by its own client");

        let signature = swisha::util::base64::decode(&sent.signature).expect("base64");
        let pre_hash = ring::digest::digest(&ring::digest::SHA512, sent.payload.get().as_bytes());
        let verify = |key: &RsaKeyPair| {
            UnparsedPublicKey::new(&RSA_PKCS1_2048_8192_SHA512, key.public().as_ref())
                .verify(pre_hash.as_ref(), &signature)
                .is_ok()
        };
        assert!(verify(&keys()[i]), "payer {i}'s payout must be signed with its own key");
        let other = (i + 1) % PAYERS;
        assert!(!verify(&keys()[other]), "and must not verify under payer {other}'s key");
    }

    // The poll that follows asks Swish with the same payer's client, never a neighbor's.
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    for i in 0..PAYERS {
        let snapshot = r.state.store.snapshot(&name(i), "INV-1042").await.expect("store").expect("row");
        assert_eq!(snapshot.status.as_deref(), Some("PAID"), "payer {i} settled");
        let uuid = snapshot.swish_ref.expect("uuid");
        let asked: Vec<_> = r.swish.gets_by().into_iter().filter(|(u, _)| *u == uuid).collect();
        assert!(!asked.is_empty(), "payer {i}'s payout was polled");
        assert!(
            asked.iter().all(|(_, by)| by.as_deref() == Some(name(i).as_str())),
            "payer {i}'s payout must be polled with its own certificate: {asked:?}"
        );
    }

    // Each reference is spent within its own payer: a repeat is refused and nothing more is sent.
    for i in 0..PAYERS {
        assert_eq!(pay(&r, payout(Some(&name(i)), "INV-1042")).await.0, 409, "payer {i}");
    }
    assert_eq!(r.swish.raw_posts().len(), PAYERS, "no payer sent a second instruction");
}

// The key says which number pays, so a payout naming none is never a guess. One naming another
// number than its key's is refused, whether that number exists or not, and nothing is sent.
#[tokio::test]
async fn a_payout_pays_from_the_number_whose_key_it_carries() {
    let r = rig().await;
    for named in [Some(name(5)), Some("atlantis".to_string())] {
        let (code, body) = pay_with(&r, &name(4), payout(named.as_deref(), "INV-1")).await;
        assert_eq!(code, 403, "{named:?}: {body}");
        assert!(body["error"].as_str().unwrap_or_default().contains("another number"), "{body}");
    }
    assert!(r.swish.posted().is_empty(), "neither may reach Swish");

    let (code, body) = pay_with(&r, &name(4), payout(None, "INV-1")).await;
    assert_eq!(code, 202, "{body}");
    assert_eq!(body["payer"], name(4));
    let sent = common::payload(&r.swish.posted()[0]);
    assert_eq!(sent["payerAlias"], number(4), "sent from the key's number");
}

#[tokio::test]
async fn each_payer_reads_its_own_status_and_no_other() {
    let r = rig().await;
    let (_, first) = pay(&r, payout(Some(&name(3)), "INV-7")).await;
    let (_, second) = pay(&r, payout(Some(&name(9)), "INV-7")).await;
    assert_ne!(first["swish_ref"], second["swish_ref"], "two payouts, two instructions");

    for (i, sent) in [(3, &first), (9, &second)] {
        let seen: Value = reqwest::Client::new()
            .get(format!("{}/swish/status/{}/INV-7", r.base, name(i)))
            .header("x-api-secret", SECRET)
            .header(common::KEY_HEADER, common::key(&name(i)))
            .send()
            .await
            .expect("request")
            .json()
            .await
            .expect("json");
        assert_eq!(seen["payer"], name(i));
        assert_eq!(seen["swish_ref"], sent["swish_ref"], "payer {i} sees its own payout");
    }

    let read = |path: String, key_of: String| {
        let url = format!("{}{path}", r.base);
        async move {
            let response = reqwest::Client::new()
                .get(url)
                .header("x-api-secret", SECRET)
                .header(common::KEY_HEADER, common::key(&key_of))
                .send()
                .await
                .expect("request");
            (response.status().as_u16(), response.json::<Value>().await.unwrap_or(Value::Null))
        }
    };
    let (code, seen) = read("/swish/status/INV-7".into(), name(9)).await;
    assert_eq!(code, 200, "the short path reads the key's number");
    assert_eq!(seen["swish_ref"], second["swish_ref"]);
    let (code, _) = read(format!("/swish/status/{}/INV-7", name(9)), name(3)).await;
    assert_eq!(code, 403, "one number's key never reads another's payout");
}

// A city's screen subscribes to its own channel. Nothing another payer does may arrive on it.
#[tokio::test]
async fn each_payer_announces_only_on_its_own_channels() {
    let r = rig().await;
    let mut streams = [0, 1].map(|i| r.state.stream.subscribe(&name(i)).expect("room for a stream"));
    pay(&r, payout(Some(&name(0)), "INV-1")).await;
    pay(&r, payout(Some(&name(1)), "INV-1")).await;
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;

    for (i, stream) in streams.iter_mut().enumerate() {
        let mut seen = 0;
        while let Ok(event) = stream.try_recv() {
            seen += 1;
            assert_eq!(event.payer, name(i), "{} on {}'s stream", event.channel, name(i));
            assert!(event.channel.starts_with(&format!("swisha:{}:", name(i))), "{}", event.channel);
            let payload: Value = serde_json::from_str(&event.payload).expect("json");
            assert_eq!(payload["payer"], name(i), "the payload names the same payer as the channel");
        }
        assert!(seen >= 2, "{} announced its payout, saw {seen}", name(i));
    }
}

// The callback names the number that paid. A callback from payer 7's number settles payer 7's
// payout and leaves the same reference under every other payer alone.
#[tokio::test]
async fn a_callback_settles_only_the_payout_of_the_number_that_paid() {
    // A Swish that never resolves, so only the callback can settle anything.
    let r = rig_with(MockSwish::new().accepts().resolves_to("CREATED")).await;
    let mut uuids = Vec::new();
    for i in [6, 7, 8] {
        let (code, body) = pay(&r, payout(Some(&name(i)), "INV-55")).await;
        assert_eq!(code, 202);
        uuids.push(body["swish_ref"].as_str().expect("uuid").to_string());
    }

    let echoed = common::callback_identifier(&r.state, &name(7), "INV-55").await;
    let code = reqwest::Client::new()
        .post(format!("{}/swish/callback", r.callback))
        .header("callbackIdentifier", echoed)
        .json(&json!({
            "payerAlias": number(7),
            "payerPaymentReference": "INV-55",
            "payoutInstructionUUID": uuids[1],
            "status": "PAID",
        }))
        .send()
        .await
        .expect("request")
        .status()
        .as_u16();
    assert_eq!(code, 200);

    let status = |i: usize| {
        let state = r.state.clone();
        async move {
            state.store.snapshot(&name(i), "INV-55").await.expect("store").and_then(|s| s.status)
        }
    };
    assert_eq!(status(7).await.as_deref(), Some("PAID"), "payer 7's payout is settled");
    assert_ne!(status(6).await.as_deref(), Some("PAID"), "payer 6's is untouched");
    assert_ne!(status(8).await.as_deref(), Some("PAID"), "payer 8's is untouched");
}

// Recovery asks Swish about a payout with the certificate of the payer that sent it. A neighbor's
// certificate could be refused, or worse, read another merchant's instruction.
#[tokio::test]
async fn recovery_asks_with_the_certificate_of_the_payer_that_sent_it() {
    let r = rig().await;
    let uuid = "AAAABBBBCCCCDDDDEEEEFFFF00009999";
    r.state
        .store
        .claim(&swisha::domain::payout::NewPayout {
            payer: &name(9),
            reference: "INV-77",
            payer_alias: &number(9),
            payee_alias: "46701234567",
            payee_ssn: Some(SSN),
            amount: 100.0,
            message: "x",
            swish_ref: uuid,
            callback_identifier: None,
        })
        .await
        .expect("claim");

    swisha::swish::reconcile::reconcile(name(9), "INV-77".into(), false, r.state.clone()).await;

    let asked: Vec<_> = r.swish.gets_by().into_iter().filter(|(u, _)| u == uuid).collect();
    assert!(!asked.is_empty(), "the payout was asked about");
    assert!(
        asked.iter().all(|(_, by)| by.as_deref() == Some(name(9).as_str())),
        "only payer 9's certificate may ask about payer 9's payout: {asked:?}"
    );
    let snapshot = r.state.store.snapshot(&name(9), "INV-77").await.expect("store").expect("row");
    assert_eq!(snapshot.status.as_deref(), Some("PAID"));
}
