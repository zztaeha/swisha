//! `POST /swish/callback`, the public listener's only route.

use std::{collections::HashSet, net::SocketAddr, sync::OnceLock};

use axum::{
    extract::{ConnectInfo, FromRequestParts, State},
    http::{HeaderMap, StatusCode, request::Parts},
    Json,
};
use serde::Deserialize;

use crate::events::{Change, SwishLogFields, record};
use crate::store::PayoutStore;
use crate::{
    http::{client_ip::client_ip, secret::constant_time_eq},
    state::SharedState,
};

#[derive(Debug, Deserialize)]
/// What Swish posts back when a payout resolves.
///
/// Every field is optional because the body is whatever Swish sends, not whatever swisha hopes
/// for. A missing reference is dropped rather than guessed at.
pub struct CallbackBody {
    /// The payout's new status.
    pub status:                                     Option<String>,
    #[serde(rename = "payerAlias")]
    /// The Swish number that paid, which says which payer the payout belongs to.
    pub payer_alias:                                Option<String>,
    #[serde(rename = "payerPaymentReference")]
    /// The reference swisha originally supplied.
    pub payer_payment_reference:                    Option<String>,
    #[serde(rename = "payoutInstructionUUID")]
    /// The instruction this concerns. Checked against the stored one, so a forged callback
    /// cannot settle a payout without guessing it.
    pub payout_instruction_uuid:                    Option<String>,
    #[serde(rename = "errorCode")]
    /// Swish's error code, on failures.
    pub error_code:                                 Option<String>,
    #[serde(rename = "errorMessage")]
    /// Swish's error text, on failures.
    pub error_message:                              Option<String>,
}

static SWISH_IPS: OnceLock<HashSet<&'static str>> = OnceLock::new();

fn swish_ips() -> &'static HashSet<&'static str> {
    SWISH_IPS.get_or_init(|| {
        HashSet::from([
            "89.46.83.171",
            "213.132.115.90",
            "213.132.115.86",
            "193.53.81.202",
            "193.53.81.203",
            "193.53.81.204",
            "193.53.81.205",
            "213.132.115.94",
        ])
    })
}

// A value from the callback body as the log shows it: quoted and escaped, so a line break cannot
// start a line of its own, and cut to 64 characters, so a long one cannot flood the log.
fn shown(value: &str) -> String {
    let cut: String = value.chars().take(64).collect();
    format!("{cut:?}")
}

/// Swish's published callback addresses, sorted. Enforced only when `SWISH_ENV=production`.
pub fn active_callback_ips() -> Vec<&'static str> {
    let mut ips: Vec<&'static str> = swish_ips().iter().copied().collect();
    ips.sort();
    ips
}

/// The caller's address, refused with `403` in production unless it is one of Swish's. Taken from
/// the request's head, so a caller outside the allowlist is refused before its body is read.
pub struct Origin(pub String);

impl FromRequestParts<SharedState> for Origin {
    type Rejection = StatusCode;

    async fn from_request_parts(parts: &mut Parts, state: &SharedState) -> Result<Self, StatusCode> {
        let ConnectInfo(addr) = ConnectInfo::<SocketAddr>::from_request_parts(parts, state)
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        let ip = client_ip(&parts.headers, addr, &state.config.trusted_proxies).to_string();
        if !swish_ips().contains(ip.as_str()) {
            if state.config.swish_env == "production" {
                tracing::warn!("[swish/callback] rejected request from unknown IP: {ip}");
                return Err(StatusCode::FORBIDDEN);
            }
            tracing::warn!("[swish/callback] non-production: accepting callback from non-allowlisted IP: {ip}");
        }
        Ok(Origin(ip))
    }
}

/// `POST /swish/callback`. Records the outcome Swish reports.
///
/// Refuses anything outside the allowlist in production, before its body is read, ignores a
/// callback from a number swisha does not pay from or whose instruction UUID or
/// `callbackIdentifier` header does not match the stored one, writes only a status Swish reports,
/// and never moves a payout back. Answers `500` when it cannot read the payout or its number, or
/// record the callback, never `200` for what it did not record; the checks ask Swish about the
/// payout either way.
pub async fn callback(
    State(state): State<SharedState>,
    Origin(ip): Origin,
    headers: HeaderMap,
    Json(body): Json<CallbackBody>,
) -> StatusCode {
    let reference = match body.payer_payment_reference.as_deref().filter(|s| !s.is_empty()) {
        Some(k) => k.to_string(),
        None => {
            tracing::warn!("[swish/callback] received callback with missing payer_payment_reference from {ip}");
            return StatusCode::OK;
        }
    };
    let callback_ref = body.payout_instruction_uuid.clone().unwrap_or_default();
    let shown_reference = shown(&reference);

    // References are unique per payer, so the paying number decides whose payout this is: loaded
    // or not, or added moments ago on another instance. One swisha does not pay from owns none.
    let alias = body.payer_alias.as_deref().unwrap_or("");
    let payer = match crate::payers::name_of(&state, alias).await {
        Ok(Some(payer)) => payer,
        Ok(None) => {
            tracing::warn!(
                "[swish/callback] payerAlias {} for {shown_reference} is not a configured payer",
                shown(alias)
            );
            return StatusCode::OK;
        }
        Err(e) => {
            tracing::warn!("[swish/callback] store error looking up payerAlias {}: {e}", shown(alias));
            return StatusCode::INTERNAL_SERVER_ERROR;
        }
    };

    // Only a status Swish reports is written. Anything else is still recorded below as it
    // arrived, but in the column it would be a status no guard was written for.
    let status = match body.status.as_deref() {
        Some(s @ ("PAID" | "DEBITED" | "DECLINED" | "ERROR" | "PENDING")) => Some(s.to_string()),
        other => {
            tracing::warn!(
                "[swish/callback] status {} for {payer}/{shown_reference} is not one Swish reports, recorded but not applied",
                other.map_or_else(|| "(missing)".to_string(), shown)
            );
            None
        }
    };

    let stored = match state.store.snapshot(&payer, &reference).await {
        Ok(row) => row,
        Err(e) => {
            tracing::warn!("[swish/callback] store error fetching payout for {payer}/{shown_reference}: {e}");
            return StatusCode::INTERNAL_SERVER_ERROR;
        }
    };

    match stored {
        None => {
            tracing::warn!("[swish/callback] no payout for {payer}/{shown_reference}");
            return StatusCode::OK;
        }
        Some(snapshot) if snapshot.swish_ref.as_deref() != Some(&callback_ref) => {
            tracing::warn!(
                "[swish/callback] UUID mismatch – payout: {payer}/{reference}, received: {}, stored: {}",
                shown(&callback_ref),
                snapshot.swish_ref.as_deref().unwrap_or("none")
            );
            return StatusCode::OK;
        }
        // Swish echoes the callbackIdentifier sent with the instruction as a header. A row stored
        // without one, which only a library caller can make, has none to compare.
        Some(snapshot)
            if snapshot.callback_identifier.as_deref().is_some_and(|expected| {
                let echoed = headers.get("callbackIdentifier").and_then(|v| v.to_str().ok()).unwrap_or("");
                !constant_time_eq(echoed.as_bytes(), expected.as_bytes())
            }) =>
        {
            tracing::warn!("[swish/callback] callbackIdentifier mismatch for {payer}/{reference} from {ip}");
            return StatusCode::OK;
        }
        Some(_) => {}
    }

    // The status guard decides what is written, and the status and its event are one write. Only
    // a callback recorded is acknowledged: MSS sent none again after a 500, so the checks ask.
    let fields = SwishLogFields {
        payer:         payer.clone(),
        reference:     reference.clone(),
        swish_ref:     Some(callback_ref),
        status:        body.status,
        amount:        None,
        payee_alias:   None,
        error_code:    body.error_code,
        error_message: body.error_message,
        ip:            Some(ip),
        actor:         None,
        note:          None,
    };
    let change = status.as_deref().map_or(Change::None, Change::Status);
    let recorded = match record("CALLBACK", fields, change, &state).await {
        Ok(recorded) => recorded,
        Err(e) => {
            tracing::warn!("[swish/callback] could not record the callback for {payer}/{reference}: {e}");
            return StatusCode::INTERNAL_SERVER_ERROR;
        }
    };

    // pg_notify has no replay, so even a refused write is announced, as the status endpoint answers
    // it, with the code this callback brought.
    tokio::spawn(async move { recorded.announce(&payer, &reference, &state).await });

    StatusCode::OK
}
