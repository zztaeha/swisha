//! `GET /system/health`: the database, the numbers' reach to Swish and certificates as counts, and
//! what Swish's own status page says.

use axum::{extract::State, http::HeaderMap, Json};
use serde_json::{json, Value};
use std::time::UNIX_EPOCH;

use crate::store::PayoutStore;
use crate::{error::ApiError, state::{SharedState, SwishProbe}};

/// `GET /system/health`. Asks the database live, and reports how many numbers are stored,
/// disabled, unusable, offline, near expiry or waiting, what Swish's own status page says about
/// payouts, and how many payouts Swish reported paid after a person redid them.
///
/// Counts only, never names: the API secret alone shows nothing about any one number. Which
/// numbers are meant is `swisha --list-payers` on the server, or each number's own
/// `GET /swish/payers/{name}` with its key.
///
/// `degraded` when the database is down, or Swish reports payouts as anything but operational, or
/// a number that takes payouts cannot reach Swish, could not load its certificates, or has one
/// that expires within [`CERTIFICATE_WARNING_DAYS`](crate::config::CERTIFICATE_WARNING_DAYS). So
/// is any number, disabled or not, with payouts due that this instance cannot ask Swish about. A
/// disabled number is otherwise counted but never degrades health, and neither does a payout paid
/// twice: swisha cannot know one was dealt with, so monitoring alerts when the count grows. Always
/// `200`: degraded is information, and nothing here stops a payout.
///
/// # Errors
///
/// [`ApiError::Unauthorized`] without the secret, and nothing else.
pub async fn get(
    State(state): State<SharedState>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    if !crate::http::secret::authorized(&headers, state.config.api_secret.as_ref()) {
        return Err(ApiError::Unauthorized);
    }

    // The database is asked on every request, the count being its ping. A health check reporting a
    // database as reachable while it is down is the one answer worse than having none.
    let paid_twice = state.store.paid_twice().await.ok();
    let db_ok = paid_twice.is_some();

    let now = crate::util::time::unix_now();
    let days_left = |expires: i64| (expires - now).div_euclid(86_400);
    let roster = state.payers.snapshot();
    let mut probes = Vec::with_capacity(roster.entries().len());
    let (mut disabled, mut unusable, mut expiring) = (0, 0, 0);
    for entry in roster.entries() {
        if entry.record.disabled {
            disabled += 1;
            continue;
        }
        match &entry.loaded {
            Ok(payer) => {
                probes.push(*payer.probe.read().await);
                let soonest = days_left(payer.tls_expires).min(days_left(payer.signing_expires));
                if soonest < crate::config::CERTIFICATE_WARNING_DAYS {
                    expiring += 1;
                }
            }
            Err(_) => unusable += 1,
        }
    }
    let offline = probes.iter().filter(|p| p.is_some_and(|p| !p.reachable)).count();
    let waiting = state.checks_waiting.lock().unwrap_or_else(|e| e.into_inner()).len();

    // Offline if any number is, unknown until one has answered. One not asked yet says nothing: at a
    // probe a second, the last of 10,000 is first asked hours after a start.
    let swish_ok = if offline > 0 {
        Some(false)
    } else if probes.iter().all(Option::is_none) {
        None
    } else {
        Some(true)
    };
    let checked_ago = probes.iter().filter_map(|&p| seconds_ago(p)).min();

    // What Swish says about itself, beside what swisha sees: offline here while Swish reports
    // operational points at this side; Swish reporting trouble points at theirs.
    let swish_status = state.swish_status.read().await.clone();
    let payout_status = swish_status.as_ref().and_then(|s| s.payout.clone());
    let maintenance = swish_status.as_ref().and_then(|s| s.next_maintenance.as_ref()).map(|m| {
        json!({ "name": m.name, "from": m.from, "until": m.until })
    });

    // Swish being unreachable is degraded too: a payout cannot complete without them. So is a
    // certificate about to expire, which stops a number outright the day it does.
    let degraded = !db_ok
        || unusable > 0
        || waiting > 0
        || swish_ok == Some(false)
        || payout_status.as_deref().is_some_and(|status| status != "operational")
        || expiring > 0;

    Ok(Json(json!({
        "status":       if degraded { "degraded" } else { "ok" },
        "db":           db_ok,
        "swish_online": swish_ok,
        "swish_checked_seconds_ago": checked_ago,
        "swish_payout_status": payout_status,
        "swish_status_checked_seconds_ago": swish_status.map(|s| s.at.elapsed().as_secs()),
        "swish_next_maintenance": maintenance,
        "payers": {
            "stored":   roster.entries().len(),
            "disabled": disabled,
            "unusable": unusable,
            "offline":  offline,
            "expiring": expiring,
            "waiting":  waiting,
        },
        "payouts_paid_twice": paid_twice,
        "started_at":   state.started_at.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs(),
        "timestamp":    crate::util::time::now().rfc3339(),
        "version":      env!("CARGO_PKG_VERSION"),
    })))
}

fn seconds_ago(probe: Option<SwishProbe>) -> Option<u64> {
    probe.map(|p| p.at.elapsed().as_secs())
}
