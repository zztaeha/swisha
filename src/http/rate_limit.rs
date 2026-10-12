//! Per-caller rate limiting on the payout and redo endpoints.

use std::{
    collections::HashMap,
    net::IpAddr,
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

const WINDOW: Duration = Duration::from_secs(60);
const PRUNE_EVERY: u64 = 1000;

// Keyed by caller and payer, so one backend paying from many numbers has a budget per number.
// Each bucket holds the requests counted in its window and when that window began.
type Caller = (IpAddr, String);
type Bucket = (u32, Instant);
static LIMITER: OnceLock<Mutex<Limiter>> = OnceLock::new();

/// Budgets of requests per caller and payer, each counted over a window of its own.
///
/// Every thousandth request drops the buckets whose window has passed, which is what keeps the map
/// bounded by recent callers rather than by every caller ever seen. The service keeps one, behind
/// [`allow`].
#[derive(Default)]
pub struct Limiter {
    buckets: HashMap<Caller, Bucket>,
    seen: u64,
}

impl Limiter {
    /// Whether `addr` may make another request for `payer` at `now`: `per_minute` per `window`.
    pub fn allow(&mut self, addr: IpAddr, payer: &str, per_minute: u32, now: Instant, window: Duration) -> bool {
        // Prune periodically so the map stays bounded without a sweep on every request.
        if self.seen.is_multiple_of(PRUNE_EVERY) {
            self.buckets.retain(|_, (_, seen)| now.duration_since(*seen) < window);
        }
        self.seen += 1;

        let entry = self.buckets.entry((addr, payer.to_string())).or_insert((0, now));
        if now.duration_since(entry.1) >= window {
            *entry = (1, now);
            true
        } else if entry.0 < per_minute {
            entry.0 += 1;
            true
        } else {
            false
        }
    }

    /// How many callers it tracks.
    pub fn len(&self) -> usize {
        self.buckets.len()
    }

    /// Whether it tracks no caller at all.
    pub fn is_empty(&self) -> bool {
        self.buckets.is_empty()
    }
}

/// How many callers are currently tracked.
///
/// The map is bounded by recent activity rather than by a fixed size, so this is the number
/// that would grow without limit if pruning ever stopped.
pub fn tracked() -> usize {
    LIMITER
        .get()
        .map(|limiter| limiter.lock().unwrap_or_else(|e| e.into_inner()).len())
        .unwrap_or(0)
}

/// Whether this caller may make another payout request for this payer: `per_minute` per 60
/// seconds, the service's [`payouts_per_minute`](crate::config::Config::payouts_per_minute).
///
/// In-process, so the limit is per instance. Running more than one instance behind a load
/// balancer multiplies the effective limit by the instance count.
pub fn allow(addr: IpAddr, payer: &str, per_minute: u32) -> bool {
    let limiter = LIMITER.get_or_init(|| Mutex::new(Limiter::default()));
    let mut limiter = limiter.lock().unwrap_or_else(|e| e.into_inner());
    limiter.allow(addr, payer, per_minute, Instant::now(), WINDOW)
}
