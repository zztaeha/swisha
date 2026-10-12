//! `GET /swish/status/{payer}/{reference}`, and its short form, which the key's number fills in.

use axum::{
    Json,
    extract::{Path, State, rejection::PathRejection},
    http::HeaderMap,
};
use serde_json::Value;

use crate::events::current_payload;
use crate::http::access;
use crate::{error::ApiError, state::SharedState};

/// `GET /swish/status/{payer}/{reference}`. The current state of one payout, shaped exactly
/// like an `updates` event.
///
/// How a caller that missed a live notification recovers, since `pg_notify` has no replay: the
/// same shape means a reconnecting client feeds it through the same handler.
///
/// # Errors
///
/// `401` without the secret or the payer's key, `403` with another number's key, `404` for a
/// reference the payer never claimed, `503` when the database cannot be read.
pub async fn status(
    State(state): State<SharedState>,
    headers: HeaderMap,
    path: Result<Path<(String, String)>, PathRejection>,
) -> Result<Json<Value>, ApiError> {
    let holder = access::number(&state, &headers).await?;
    let Path((payer, reference)) = path?;
    access::covers(&holder, &payer)?;
    answer(&state, &payer, &reference).await
}

/// `GET /swish/status/{reference}`. As [`status`], for the number whose key the request carries.
///
/// # Errors
///
/// As [`status`].
pub async fn status_by_reference(
    State(state): State<SharedState>,
    headers: HeaderMap,
    path: Result<Path<String>, PathRejection>,
) -> Result<Json<Value>, ApiError> {
    let holder = access::number(&state, &headers).await?;
    let Path(reference) = path?;
    answer(&state, &holder.name, &reference).await
}

async fn answer(state: &SharedState, payer: &str, reference: &str) -> Result<Json<Value>, ApiError> {
    super::payout_reference(reference)?;
    let payload = current_payload(payer, reference, state).await.map_err(|e| {
        tracing::warn!("[swish/status] store error for {payer}/{reference}: {e}");
        ApiError::ServiceUnavailable("Could not read the payout.".into())
    })?;
    payload.map(Json).ok_or(ApiError::NotFound)
}
