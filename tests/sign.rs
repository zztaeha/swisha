// Swish hashes the payload twice: swisha pre-hashes with SHA-512 and ring hashes again before
// signing. Getting that wrong produces a signature Swish rejects, and nothing else notices.

use ring::signature::{RsaKeyPair, UnparsedPublicKey, RSA_PKCS1_2048_8192_SHA512};
use swisha::swish::sign::{private_key_der, rsa_key_pair, sign_payload};
use swisha::util::base64;

fn key() -> RsaKeyPair {
    let out = std::process::Command::new("openssl")
        .args(["genpkey", "-algorithm", "RSA", "-pkeyopt", "rsa_keygen_bits:2048"])
        .output()
        .expect("openssl is required to generate a throwaway signing key");
    assert!(out.status.success(), "openssl genpkey failed");
    let pem = String::from_utf8(out.stdout).expect("PEM is UTF-8");
    RsaKeyPair::from_pkcs8(&private_key_der(&pem).expect("decode PEM")).expect("parse key")
}

// The signature is verified against SHA512(payload), because ring applies the second hash. That
// is the whole double-hash contract, checked rather than described.
#[test]
fn the_signature_verifies_over_the_pre_hashed_payload() {
    let pair = key();
    let payload = r#"{"amount":"100.00","currency":"SEK"}"#;
    let signature = base64::decode(&sign_payload(payload, &pair).expect("sign")).expect("base64");

    let pre_hash = ring::digest::digest(&ring::digest::SHA512, payload.as_bytes());
    let public = UnparsedPublicKey::new(&RSA_PKCS1_2048_8192_SHA512, pair.public().as_ref());
    public
        .verify(pre_hash.as_ref(), &signature)
        .expect("the signature must verify over the pre-hashed payload");
}

#[test]
fn a_signature_does_not_verify_over_the_raw_payload() {
    let pair = key();
    let payload = "hello";
    let signature = base64::decode(&sign_payload(payload, &pair).expect("sign")).expect("base64");

    let public = UnparsedPublicKey::new(&RSA_PKCS1_2048_8192_SHA512, pair.public().as_ref());
    assert!(
        public.verify(payload.as_bytes(), &signature).is_err(),
        "signing the raw payload once would be the wrong contract"
    );
}

#[test]
fn a_changed_payload_invalidates_the_signature() {
    let pair = key();
    let signature = base64::decode(&sign_payload("original", &pair).expect("sign")).expect("base64");

    let tampered = ring::digest::digest(&ring::digest::SHA512, b"tampered");
    let public = UnparsedPublicKey::new(&RSA_PKCS1_2048_8192_SHA512, pair.public().as_ref());
    assert!(public.verify(tampered.as_ref(), &signature).is_err());
}

#[test]
fn the_signature_is_base64_of_the_full_modulus() {
    let pair = key();
    let encoded = sign_payload("payload", &pair).expect("sign");
    let raw = base64::decode(&encoded).expect("the signature is base64");
    assert_eq!(raw.len(), pair.public().modulus_len(), "2048-bit key, 256-byte signature");
    assert!(!encoded.contains(char::is_whitespace), "no line wrapping");
}

// A certificate file carries the certificate, often its chain, and the key, in any order. The
// key is found by its label, so the certificates around it are never read as part of it.
#[test]
fn the_key_is_found_among_the_certificates_around_it() {
    let key = "-----BEGIN PRIVATE KEY-----\nAAECAwQ=\n-----END PRIVATE KEY-----\n";
    let cert = "-----BEGIN CERTIFICATE-----\nBQYHCAk=\n-----END CERTIFICATE-----\n";
    for bundle in [format!("{cert}{cert}{key}"), format!("{key}{cert}"), key.to_string()] {
        assert_eq!(private_key_der(&bundle).expect("decode"), vec![0, 1, 2, 3, 4], "{bundle}");
    }
    assert!(private_key_der("-----BEGIN PRIVATE KEY-----\nnot base64!\n-----END PRIVATE KEY-----").is_err());
}

// Each refusal names its fix, because the alternative is ring's bare "InvalidEncoding" on a file
// that looks fine to the person reading it.
#[test]
fn a_key_swisha_cannot_use_is_refused_with_its_fix() {
    let cert = "-----BEGIN CERTIFICATE-----\nBQYHCAk=\n-----END CERTIFICATE-----\n";
    let key = "-----BEGIN PRIVATE KEY-----\nAAECAwQ=\n-----END PRIVATE KEY-----\n";
    for (pem, fix) in [
        (format!("{cert}-----BEGIN RSA PRIVATE KEY-----\nAAAA\n-----END RSA PRIVATE KEY-----\n"), "openssl pkcs8 -topk8"),
        (format!("{cert}-----BEGIN ENCRYPTED PRIVATE KEY-----\nAAAA\n-----END ENCRYPTED PRIVATE KEY-----\n"), "openssl pkcs8 -topk8"),
        (cert.to_string(), "cat cert.pem cert.key"),
        (format!("{cert}{key}{key}"), "one certificate and its own key"),
        (format!("{cert}-----BEGIN PRIVATE KEY-----\nAAAA\n"), "unterminated"),
    ] {
        let error = private_key_der(&pem).expect_err(&pem).to_string();
        assert!(error.contains(fix), "{pem}\nshould name {fix:?}, got: {error}");
    }
}

// Swish's certificate system issues 4096-bit keys only, so the largest key it hands out has to
// load and sign. ring's ceiling for a private key is exactly 4096 bits.
#[test]
fn a_4096_bit_key_loads_and_signs() {
    let out = std::process::Command::new("openssl")
        .args(["genpkey", "-algorithm", "RSA", "-pkeyopt", "rsa_keygen_bits:4096"])
        .output()
        .expect("openssl is required to generate a throwaway key");
    assert!(out.status.success(), "openssl genpkey failed");
    let pair = rsa_key_pair(&String::from_utf8(out.stdout).expect("PEM")).expect("a 4096-bit key loads");
    assert_eq!(pair.public().modulus_len() * 8, 4096);

    let signature = base64::decode(&sign_payload("payload", &pair).expect("sign")).expect("base64");
    let pre_hash = ring::digest::digest(&ring::digest::SHA512, b"payload");
    UnparsedPublicKey::new(&RSA_PKCS1_2048_8192_SHA512, pair.public().as_ref())
        .verify(pre_hash.as_ref(), &signature)
        .expect("a 4096-bit signature verifies");
}
