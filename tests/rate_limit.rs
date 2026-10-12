#![cfg(feature = "http")]

use std::net::IpAddr;
use swisha::http::rate_limit::allow;

// The limiter keys on caller and payer and lives in a process-wide map, so each test uses its own
// address.
fn ip(last: u8) -> IpAddr {
    format!("203.0.113.{last}").parse().unwrap()
}

#[test]
fn allows_exactly_thirty_per_window() {
    let addr = ip(10);
    for n in 1..=30 {
        assert!(allow(addr, "test", 30), "request {n} of 30 should be allowed");
    }
    assert!(!allow(addr, "test", 30), "the 31st request must be refused");
    assert!(!allow(addr, "test", 30), "and it stays refused within the window");
}

#[test]
fn separate_addresses_have_separate_budgets() {
    let a = ip(20);
    let b = ip(21);
    for _ in 0..30 {
        assert!(allow(a, "test", 30));
    }
    assert!(!allow(a, "test", 30), "a is exhausted");
    assert!(allow(b, "test", 30), "b must be unaffected by a");
}

#[test]
fn ipv6_is_tracked_independently_of_ipv4() {
    let v4: IpAddr = "203.0.113.30".parse().unwrap();
    let v6: IpAddr = "2001:db8::30".parse().unwrap();
    for _ in 0..30 {
        assert!(allow(v4, "test", 30));
    }
    assert!(!allow(v4, "test", 30));
    assert!(allow(v6, "test", 30));
}

// One backend paying for several cities sends every payout from the same address. The budget is
// per caller and payer, so one busy city cannot spend the others' 30 a minute.
#[test]
fn each_payer_has_its_own_budget_from_one_caller() {
    let caller: IpAddr = "203.0.113.77".parse().unwrap();
    for _ in 0..30 {
        assert!(allow(caller, "sthlm", 30));
    }
    assert!(!allow(caller, "sthlm", 30), "sthlm is exhausted");
    assert!(allow(caller, "gbg", 30), "gbg must be unaffected by sthlm");
}

// The budget is a setting: a backend paying out in bulk raises it, and it still holds exactly and
// still per payer.
#[test]
fn a_raised_budget_holds_exactly_and_per_payer() {
    let caller = ip(90);
    for n in 1..=600 {
        assert!(allow(caller, "bulk", 600), "request {n} of 600 should be allowed");
    }
    assert!(!allow(caller, "bulk", 600), "the 601st request must be refused");
    assert!(allow(caller, "other", 600), "another payer's budget is untouched");
}

// The prune is the limiter's memory bound. With time handed in, a minute passes at once: callers
// whose window has gone by are dropped at the next prune, and only the current one is kept.
#[test]
fn callers_past_their_window_are_pruned() {
    use std::time::{Duration, Instant};
    use swisha::http::rate_limit::Limiter;
    let (start, window) = (Instant::now(), Duration::from_secs(60));
    let mut limiter = Limiter::default();
    for n in 0..50 {
        limiter.allow(ip(n), "test", 30, start, window);
    }
    assert_eq!(limiter.len(), 50);

    let later = start + window + Duration::from_secs(1);
    for _ in 0..1000 {
        limiter.allow(ip(200), "test", 100_000, later, window);
    }
    assert_eq!(limiter.len(), 1, "the fifty whose window passed are gone");
}

// A spent budget comes back once its window has passed, and not a moment before.
#[test]
fn a_spent_budget_comes_back_once_its_window_has_passed() {
    use std::time::{Duration, Instant};
    use swisha::http::rate_limit::Limiter;
    let (start, window) = (Instant::now(), Duration::from_secs(60));
    let mut limiter = Limiter::default();
    let spent: Vec<bool> = (0..4).map(|_| limiter.allow(ip(1), "test", 3, start, window)).collect();
    assert_eq!(spent, [true, true, true, false]);
    let almost = start + window - Duration::from_millis(1);
    assert!(!limiter.allow(ip(1), "test", 3, almost, window), "not a moment early");
    assert!(limiter.allow(ip(1), "test", 3, start + window, window), "back once the window has passed");
}
