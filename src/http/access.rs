//! Who may act for which number: the API secret first, then the number's own key.
//!
//! The secret only reaches swisha. Every request about a stored number also carries that
//! number's key in [`KEY_HEADER`], read fresh from the store each time, so a key replaced on one
//! instance stops working on every instance at once. Both are read from the headers alone, by
//! [`Number`] and [`Operator`], before a handler reads the path or the body.

use axum::{
    extract::FromRequestParts,
    http::{HeaderMap, request::Parts},
};

use crate::domain::payer::KeyHolder;
use crate::error::ApiError;
use crate::http::secret;
use crate::state::SharedState;
use crate::store::PayoutStore;

/// The header a caller sends a number's key in.
pub const KEY_HEADER: &str = "x-payer-key";

/// The number a request may act for, as [`number`] finds it, extracted from the headers alone.
///
/// A handler takes it before its path and its body, so a caller without the secret and a key is
/// answered from its headers: a body it announces is never read, and holds nothing open.
pub struct Number(pub KeyHolder);

impl FromRequestParts<SharedState> for Number {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &SharedState) -> Result<Self, ApiError> {
        number(state, &parts.headers).await.map(Number)
    }
}

/// The API secret alone, extracted from the headers before the body is read: what adding a number
/// and reading health need.
pub struct Operator;

impl FromRequestParts<SharedState> for Operator {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &SharedState) -> Result<Self, ApiError> {
        if secret::authorized(&parts.headers, state.config.api_secret.as_ref()) {
            Ok(Operator)
        } else {
            Err(ApiError::Unauthorized)
        }
    }
}

/// The number a request may act for, after the secret and the key in [`KEY_HEADER`].
///
/// Read from the headers alone, before a request's path or body, so a caller without both learns
/// nothing else. A request that names a number then checks it with [`covers`].
///
/// # Errors
///
/// - [`ApiError::Unauthorized`] without the secret.
/// - [`ApiError::UnknownKey`] when the key is missing, malformed or no number's current key.
/// - [`ApiError::ServiceUnavailable`] when the store cannot be read.
pub async fn number(state: &SharedState, headers: &HeaderMap) -> Result<KeyHolder, ApiError> {
    if !secret::authorized(headers, state.config.api_secret.as_ref()) {
        return Err(ApiError::Unauthorized);
    }
    let hash = presented(headers).ok_or(ApiError::UnknownKey)?;
    state
        .store
        .payer_by_key(&hash)
        .await
        .map_err(|e| {
            tracing::warn!("[access] store error checking a key: {e}");
            ApiError::ServiceUnavailable("Could not check the key.".into())
        })?
        .ok_or(ApiError::UnknownKey)
}

/// Whether the key's number is the one a request names.
///
/// # Errors
///
/// [`ApiError::Forbidden`] when it is another number. The answer never says whether `payer`
/// exists.
pub fn covers(holder: &KeyHolder, payer: &str) -> Result<(), ApiError> {
    if holder.name == payer {
        Ok(())
    } else {
        Err(ApiError::Forbidden(
            "This key belongs to another number. Send the key of the number the request is about."
                .into(),
        ))
    }
}

/// The hash of the key a request carries, or `None` when it carries none shaped as a key.
pub fn presented(headers: &HeaderMap) -> Option<String> {
    let key = headers.get(KEY_HEADER).and_then(|value| value.to_str().ok())?;
    crate::payers::key_hash(key)
}
