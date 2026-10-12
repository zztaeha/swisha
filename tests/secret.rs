#![cfg(feature = "http")]

use axum::http::{HeaderMap, HeaderValue};
use swisha::http::secret::{HEADER, matches};

fn headers(value: Option<&str>) -> HeaderMap {
    let mut h = HeaderMap::new();
    if let Some(v) = value {
        h.insert(HEADER, HeaderValue::from_str(v).unwrap());
    }
    h
}

const SECRET: &str = "0012fdeb1969d6f0dbda66daf737baf3";

#[test]
fn accepts_the_exact_secret() {
    assert!(matches(&headers(Some(SECRET)), SECRET));
}

#[test]
fn rejects_wrong_secrets() {
    assert!(!matches(&headers(Some("0012fdeb1969d6f0dbda66daf737baf4")), SECRET)); // last byte
    assert!(!matches(&headers(Some("1012fdeb1969d6f0dbda66daf737baf3")), SECRET)); // first byte
    assert!(!matches(&headers(Some(&SECRET[..31])), SECRET)); // truncated
    assert!(!matches(&headers(Some(&format!("{SECRET}x"))), SECRET)); // extended
    assert!(!matches(&headers(Some("")), SECRET));
    assert!(!matches(&headers(None), SECRET));
}

// An empty configured secret must never authorize anyone. Both sides being empty compares
// equal byte for byte, so without the explicit guard this is an open endpoint.
#[test]
fn an_empty_configured_secret_authorizes_nobody() {
    assert!(!matches(&headers(None), ""));
    assert!(!matches(&headers(Some("")), ""));
    assert!(!matches(&headers(Some("anything")), ""));
}
