// Polling is how a payout's outcome is decided, so the connection that carries the answer has to
// be authenticated. If swisha could be made to trust an impostor, anything on the network path
// could answer "PAID" for a payout that never happened.
//
// Opt-in, because it needs a server standing in for Swish with a certificate nothing trusts:
//
//   SWISHA_TEST_IMPOSTOR=https://127.0.0.1:8470 \
//   SWISHA_TEST_IMPOSTOR_CA=/path/to/fake.crt \
//   SWISH_TEST_CERT_DIR=~/Downloads/client_cert \
//     cargo test --test tls -- --nocapture

use swisha::swish::client::{build_swish_client, swish_get};

// The test bundle's TLS certificate and key, as the one file swisha reads in production.
fn tls_file() -> Option<String> {
    let dir = std::env::var("SWISH_TEST_CERT_DIR").ok()?;
    let read = |f: &str| std::fs::read_to_string(format!("{dir}/{f}")).ok();
    let cert = read("Swish_Merchant_TestCertificate_1234679304.pem")?;
    let key = read("Swish_Merchant_TestCertificate_1234679304.key")?;
    Some(format!("{cert}\n{key}"))
}

fn impostor() -> Option<String> {
    std::env::var("SWISHA_TEST_IMPOSTOR").ok()
}

// The attack: something on the network answers as Swish, with its own certificate.
#[tokio::test]
async fn swisha_refuses_a_server_impersonating_swish() {
    let (Some(url), Some(tls)) = (impostor(), tls_file()) else {
        eprintln!("skipped: SWISHA_TEST_IMPOSTOR and SWISH_TEST_CERT_DIR are not both set");
        return;
    };
    let client = build_swish_client(&tls).expect("client");

    let result = swish_get(&format!("{url}/swish-cpcapi/api/v1/payouts/ABC"), &client).await;
    assert!(
        result.is_err(),
        "swisha accepted an untrusted certificate and would have believed its answer"
    );
    eprintln!("   refused with: {}", result.unwrap_err());
}

// The control. Without it, the refusal above could just mean the server was unreachable, and the
// test would pass for entirely the wrong reason. swisha has no setting to trust another CA, so a
// plain client that trusts the impostor explicitly stands in.
#[tokio::test]
async fn the_same_server_answers_a_client_that_trusts_it() {
    let Some(ca_path) = std::env::var("SWISHA_TEST_IMPOSTOR_CA").ok() else {
        eprintln!("skipped: SWISHA_TEST_IMPOSTOR_CA is not set");
        return;
    };
    let (Some(url), Some(tls)) = (impostor(), tls_file()) else {
        eprintln!("skipped: SWISHA_TEST_IMPOSTOR and SWISH_TEST_CERT_DIR are not both set");
        return;
    };
    let _ = rustls::crypto::ring::default_provider().install_default();
    let ca = std::fs::read(&ca_path).expect("read the impostor's certificate");
    let client = reqwest::Client::builder()
        .identity(reqwest::Identity::from_pem(tls.as_bytes()).expect("identity"))
        .tls_certs_merge([reqwest::Certificate::from_pem(&ca).expect("impostor certificate")])
        .build()
        .expect("client");

    let result = swish_get(&format!("{url}/swish-cpcapi/api/v1/payouts/ABC"), &client).await;
    assert!(
        result.is_ok(),
        "the harness itself is broken: the server is unreachable even when trusted"
    );
    let body = result.unwrap().text().await.expect("body");
    assert!(body.contains("PAID"), "the impostor answers PAID, so the refusal above was real");
    eprintln!("   trusted, and it answered: {body}");
}
