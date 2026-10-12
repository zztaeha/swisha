#![cfg(feature = "http")]

use axum::http::{HeaderMap, HeaderName, HeaderValue};
use std::net::{IpAddr, SocketAddr};
use swisha::http::client_ip::client_ip;

fn peer(addr: &str) -> SocketAddr {
    format!("{addr}:40000").parse().unwrap()
}

fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
    let mut h = HeaderMap::new();
    for (k, v) in pairs {
        h.insert(
            HeaderName::from_bytes(k.as_bytes()).unwrap(),
            HeaderValue::from_str(v).unwrap(),
        );
    }
    h
}

fn trusted() -> Vec<IpAddr> {
    vec!["127.0.0.1".parse().unwrap()]
}

// An untrusted peer cannot claim to be someone else, whatever it sends.
#[test]
fn forwarded_headers_are_ignored_from_an_untrusted_peer() {
    let h = headers(&[("x-forwarded-for", "8.8.8.8")]);
    let got = client_ip(&h, peer("203.0.113.9"), &trusted());
    assert_eq!(got.to_string(), "203.0.113.9");
}

#[test]
fn a_trusted_peer_is_believed() {
    let h = headers(&[("x-forwarded-for", "8.8.8.8")]);
    assert_eq!(client_ip(&h, peer("127.0.0.1"), &trusted()).to_string(), "8.8.8.8");
}

#[test]
fn only_the_first_x_forwarded_for_entry_is_used() {
    let h = headers(&[("x-forwarded-for", "8.8.8.8, 10.0.0.1, 172.16.0.9")]);
    assert_eq!(client_ip(&h, peer("127.0.0.1"), &trusted()).to_string(), "8.8.8.8");
}

// The shipped nginx block overwrites X-Forwarded-For and never touches X-Real-IP, so whatever
// X-Real-IP holds came from the caller. Read after a private X-Forwarded-For, it passed the
// callback allowlist.
#[test]
fn x_real_ip_is_never_believed() {
    for forwarded in [None, Some("10.0.0.1"), Some("not-an-ip"), Some(""), Some("127.0.0.1")] {
        let mut pairs = vec![("x-real-ip", "89.46.83.171")];
        if let Some(forwarded) = forwarded {
            pairs.push(("x-forwarded-for", forwarded));
        }
        assert_eq!(
            client_ip(&headers(&pairs), peer("127.0.0.1"), &trusted()).to_string(),
            "127.0.0.1",
            "x-forwarded-for={forwarded:?}"
        );
    }
}

#[test]
fn falls_back_to_the_peer_when_nothing_usable_is_forwarded() {
    for h in [
        headers(&[]),
        headers(&[("x-forwarded-for", "192.168.1.1")]),
        headers(&[("x-forwarded-for", "junk")]),
    ] {
        assert_eq!(client_ip(&h, peer("127.0.0.1"), &trusted()).to_string(), "127.0.0.1");
    }
}

// A header value that is not a valid public IP must never reach the logs verbatim.
#[test]
fn never_returns_unvalidated_header_text() {
    let h = headers(&[("x-forwarded-for", "1.1.1.1'; DROP TABLE x--")]);
    assert_eq!(client_ip(&h, peer("127.0.0.1"), &trusted()).to_string(), "127.0.0.1");
}

#[test]
fn ipv6_is_handled() {
    let h = headers(&[("x-forwarded-for", "2001:db8::1")]);
    assert_eq!(client_ip(&h, peer("127.0.0.1"), &trusted()).to_string(), "2001:db8::1");
}

// A listener bound to [::] sees an IPv4 connection as ::ffff:a.b.c.d. That is the same address, so
// it is matched and reported as IPv4: as written, a trusted proxy would go unrecognized.
#[test]
fn an_ipv4_mapped_address_is_read_as_ipv4() {
    let h = headers(&[("x-forwarded-for", "89.46.83.171")]);
    assert_eq!(client_ip(&h, peer("[::ffff:127.0.0.1]"), &trusted()).to_string(), "89.46.83.171");
    assert_eq!(client_ip(&headers(&[]), peer("[::ffff:203.0.113.9]"), &trusted()).to_string(), "203.0.113.9");
    let h = headers(&[("x-forwarded-for", "::ffff:89.46.83.171")]);
    assert_eq!(client_ip(&h, peer("127.0.0.1"), &trusted()).to_string(), "89.46.83.171");
}
