#![cfg(feature = "http")]

// The rate limiter keeps one process-wide map, so these are in a binary of their own: any HTTP
// test sharing the process would move the number being measured. A mutex serializes them for
// the same reason.

use std::net::IpAddr;
use std::sync::Mutex;
use swisha::http::rate_limit::{allow, tracked};

static SERIAL: Mutex<()> = Mutex::new(());

fn probe_ip(n: u32) -> IpAddr {
    format!("192.0.2.{}", n % 256).parse().unwrap()
}

fn wide_ip(n: u32) -> IpAddr {
    format!("2001:db8:{:x}:{:x}::1", n >> 16, n & 0xffff).parse().unwrap()
}

#[test]
fn a_repeated_caller_occupies_one_slot() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let before = tracked();
    let ip = probe_ip(1);
    for _ in 0..100 {
        allow(ip, "test", 30);
    }
    assert!(
        tracked() <= before + 1,
        "one address must not grow the map by more than one entry"
    );
}

// The map is keyed on caller and payer, so it grows with distinct callers. That is expected; what
// must not happen is growth without any bound at all, which the prune is there to prevent.
#[test]
fn distinct_callers_each_take_a_slot() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let before = tracked();
    for n in 0..50 {
        allow(probe_ip(100 + n), "test", 30);
    }
    assert!(
        tracked().saturating_sub(before) <= 50,
        "50 callers must not create more than 50 entries"
    );
}

// A flood from a fixed set of addresses costs a fixed amount of memory however long it lasts. The
// prune that drops callers past their window is tested in rate_limit.rs, with time passed in.
#[test]
fn the_map_grows_with_callers_not_with_requests() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let before = tracked();
    for _ in 0..20 {
        for n in 0..200 {
            allow(wide_ip(30_000 + n), "test", 30);
        }
    }
    assert!(
        tracked().saturating_sub(before) <= 200,
        "4000 requests from 200 callers must cost 200 entries, not 4000"
    );
}

#[test]
fn a_refused_caller_does_not_take_another_slot() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let ip = probe_ip(200);
    for _ in 0..30 {
        allow(ip, "test", 30);
    }
    let after_budget = tracked();
    for _ in 0..500 {
        assert!(!allow(ip, "test", 30), "the budget is spent");
    }
    assert!(
        tracked() <= after_budget,
        "a flood from one refused caller must not grow the map"
    );
}
