//! Swish's own status page, read so health can tell a problem at Swish from a problem here.
//!
//! The page is an Atlassian Statuspage. Only its `Payout` component and the maintenance windows
//! announced for it matter to swisha. What it says is reported, never acted on: a stale or wrong
//! page must not be able to stop a payout.

use std::time::{Duration, Instant};

use serde_json::Value;

use crate::error::{Context, Result, err};
use crate::state::{Maintenance, SharedState, SwishStatus};

/// Where Swish publishes its status summary.
pub const SUMMARY_URL: &str = "https://status.swish.nu/api/v2/summary.json";

// The Payout component's id on that page, with its name as the fallback should the id change.
const PAYOUT_ID: &str = "67rlmr7vkql2";
const PAYOUT_NAME: &str = "Payout";

/// Reads the summary at `url` and picks out what concerns payouts.
///
/// # Errors
///
/// When the page cannot be reached within 10 seconds, answers with anything but success, a
/// redirect included, which is never followed, or is not JSON. A page that is JSON but lists no
/// `Payout` component is `Ok`, with nothing known.
pub async fn fetch(url: &str) -> Result<SwishStatus> {
    // reqwest is built without a crypto provider, and this client takes the process default: ring,
    // which signs payloads too. Err only means it is installed already.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .context("Failed to build the status page client")?;
    let response = client.get(url).send().await.context("Swish's status page did not answer")?;
    if !response.status().is_success() {
        return Err(err(format!("Swish's status page answered {}", response.status())));
    }
    let summary: Value = response
        .json()
        .await
        .context("Swish's status page answered with something other than JSON")?;
    Ok(read(&summary))
}

/// Fetches the page and stores what it says. A failure stores nothing known, rather than an
/// answer that is getting older without saying so.
pub async fn refresh(state: &SharedState) {
    let status = match fetch(&state.config.swish_status_url).await {
        Ok(status) => Some(status),
        Err(e) => {
            tracing::warn!("[status-page] {e}");
            None
        }
    };
    *state.swish_status.write().await = status;
}

/// What a status summary says about payouts: the `Payout` component's status, and the earliest
/// maintenance announced for it that has not finished.
///
/// ```
/// let summary = serde_json::json!({
///     "components": [{ "id": "67rlmr7vkql2", "name": "Payout", "status": "operational" }],
///     "scheduled_maintenances": [],
/// });
/// let status = swisha::swish::status_page::read(&summary);
/// assert_eq!(status.payout.as_deref(), Some("operational"));
/// assert!(status.next_maintenance.is_none());
/// ```
pub fn read(summary: &Value) -> SwishStatus {
    let payout = summary["components"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|component| is_payout(component))
        .and_then(|component| component["status"].as_str())
        .map(str::to_string);

    let next_maintenance = summary["scheduled_maintenances"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|m| matches!(m["status"].as_str(), Some("scheduled" | "in_progress" | "verifying")))
        .filter(|m| m["components"].as_array().into_iter().flatten().any(is_payout))
        .filter_map(|m| {
            let from = m["scheduled_for"].as_str()?;
            Some((instant(from)?, Maintenance {
                name: m["name"].as_str().unwrap_or_default().to_string(),
                from: from.to_string(),
                until: m["scheduled_until"].as_str().unwrap_or_default().to_string(),
            }))
        })
        .min_by_key(|(starts, _)| *starts)
        .map(|(_, maintenance)| maintenance);

    SwishStatus { payout, next_maintenance, at: Instant::now() }
}

fn is_payout(component: &Value) -> bool {
    component["id"] == PAYOUT_ID
        || (component["name"] == PAYOUT_NAME && component["group"].as_bool() != Some(true))
}

// Statuspage writes times as 2026-11-03T02:00:00.000+01:00. The offset changes across daylight
// saving, so windows are ordered by the instant they start, not by their text.
fn instant(text: &str) -> Option<i64> {
    if !text.is_ascii() || text.len() < 20 {
        return None;
    }
    let field = |start: usize, len: usize| text.get(start..start + len)?.parse::<u32>().ok();
    let local = crate::util::time::unix_from_utc(
        i32::try_from(field(0, 4)?).ok()?,
        field(5, 2)?,
        field(8, 2)?,
        field(11, 2)?,
        field(14, 2)?,
        field(17, 2)?,
    );
    if text.ends_with('Z') {
        return Some(local);
    }
    let offset = text.get(text.len() - 6..)?;
    let sign = match offset.as_bytes()[0] {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let (hours, minutes) = (field(text.len() - 5, 2)?, field(text.len() - 2, 2)?);
    let seconds = i64::from(hours) * 3600 + i64::from(minutes) * 60;
    Some(local - sign * seconds)
}
