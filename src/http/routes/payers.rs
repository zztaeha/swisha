//! `/swish/payers`: the Swish numbers swisha pays from, added and changed.
//!
//! Adding a number needs the API secret alone, and answers with the number's key, once. Every
//! other route here is about one stored number and needs its key too. Every change is a person's,
//! so each one names `by`, and its log line, with who, what and from where, is written with it or
//! the change is not made. Certificates arrive as PEM text in the body; swisha decides where they
//! are kept.

use axum::{
    Json,
    extract::{Path, State, rejection::{JsonRejection, PathRejection}},
    http::{HeaderMap, StatusCode},
};
use serde_json::{Value, json};

use crate::domain::payer::KeyHolder;
use crate::domain::request::{CertificatesRequest, NewPayerRequest, NicknameRequest, PersonRequest};
use crate::error::ApiError;
use crate::http::{access::{self, Number, Operator}, client_ip::PeerAddr};
use crate::payers::{self, PayerError};
use crate::state::{Entry, SharedState};
use crate::util::time::{to_stockholm, unix_now};

/// `GET /swish/payers/{name}`. One stored number: its nickname and number, whether it is
/// disabled, whether its certificates loaded and why not, when each certificate expires, and
/// whether Swish answered its last probe.
///
/// # Errors
///
/// `401` without the secret or the number's key, `403` with another number's key.
pub async fn read(
    State(state): State<SharedState>,
    Number(holder): Number,
    path: Result<Path<String>, PathRejection>,
) -> Result<Json<Value>, ApiError> {
    let name = named(&holder, path)?;
    current(&state, &name).await
}

/// `POST /swish/payers`. Adds a number from its name, nickname, number and two certificates, and
/// answers `201` with it as [`read`] shows it, plus its `key`, after one probe of Swish with the
/// new certificate.
///
/// The key is in this answer only: swisha keeps its hash, so it can never be shown again, and
/// every later request about the number needs it. Both certificates are checked as swisha will use
/// them before anything is kept: a certificate and its own key, not expired, and not issued to
/// another Swish number. The probe informs and never refuses, so a moment's network trouble cannot
/// block an upload.
///
/// Once the number is stored the answer is `201` with its key, even when its files could not be
/// put in place or it could not be read back: it is then `usable: false`, and `problem` says why.
///
/// # Errors
///
/// - `401` without the secret.
/// - `400` for a field or a certificate swisha would not pay with, saying which and why.
/// - `409` when the name or the number is taken, or the number paid out under another name. The
///   answer never names another number.
/// - `503` when the store or the certificate folder fails before the number is stored.
pub async fn add(
    State(state): State<SharedState>,
    _: Operator,
    peer: PeerAddr,
    body: Result<Json<NewPayerRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let Json(body) = body?;
    let added = payers::add(&state, &body, &peer.ip().to_string()).await.map_err(refused)?;
    let name = body.name.trim();
    probe_now(&state, name).await;
    // The number is stored, so its key is answered whatever follows: nothing else can show it.
    let mut answer = match current(&state, name).await {
        Ok(Json(answer)) => answer,
        Err(_) => json!({
            "name": name,
            "nickname": body.nickname.trim(),
            "number": body.number.trim(),
            "usable": false,
            "problem": format!("Stored, but it could not be read back. Read it with GET /swish/payers/{name}."),
        }),
    };
    if let Some(unplaced) = added.unplaced {
        answer["usable"] = json!(false);
        answer["problem"] = json!(unplaced);
    }
    answer["key"] = json!(added.key);
    Ok((StatusCode::CREATED, Json(answer)))
}

/// `PUT /swish/payers/{name}/certificates`. Replaces a number's two certificates once Swish has
/// issued new ones, with the checks [`add`] makes, and answers with the number as it now stands.
///
/// # Errors
///
/// `401` without the secret or the number's key, `403` with another number's key, and as [`add`]
/// for the certificates.
pub async fn certificates(
    State(state): State<SharedState>,
    Number(holder): Number,
    peer: PeerAddr,
    path: Result<Path<String>, PathRejection>,
    body: Result<Json<CertificatesRequest>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    let name = named(&holder, path)?;
    let Json(body) = body?;
    payers::replace_certificates(&state, &name, &body, &peer.ip().to_string())
        .await
        .map_err(refused)?;
    probe_now(&state, &name).await;
    current(&state, &name).await
}

/// `PATCH /swish/payers/{name}`. Gives a number a new nickname. Its name and number never change,
/// so a body carrying either is refused.
///
/// # Errors
///
/// `401` without the secret or the number's key, `403` with another number's key, `400` for the
/// nickname or `by`, `422` for a field other than `nickname` and `by`, `503` when the store fails.
pub async fn nickname(
    State(state): State<SharedState>,
    Number(holder): Number,
    peer: PeerAddr,
    path: Result<Path<String>, PathRejection>,
    body: Result<Json<NicknameRequest>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    let name = named(&holder, path)?;
    let Json(body) = body?;
    payers::set_nickname(&state, &name, &body, &peer.ip().to_string()).await.map_err(refused)?;
    current(&state, &name).await
}

/// `POST /swish/payers/{name}/disable`. The number takes no new payouts from now on, on every
/// instance at once. Its earlier payouts still settle: their callbacks, follow-ups and checks
/// carry on.
///
/// # Errors
///
/// `401` without the secret or the number's key, `403` with another number's key, `400` for `by`
/// or `note`, `503` when the store fails.
pub async fn disable(
    State(state): State<SharedState>,
    Number(holder): Number,
    peer: PeerAddr,
    path: Result<Path<String>, PathRejection>,
    body: Result<Json<PersonRequest>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    switch(state, holder, peer, path, body, true).await
}

/// `POST /swish/payers/{name}/enable`. The number takes payouts again.
///
/// # Errors
///
/// As [`disable`].
pub async fn enable(
    State(state): State<SharedState>,
    Number(holder): Number,
    peer: PeerAddr,
    path: Result<Path<String>, PathRejection>,
    body: Result<Json<PersonRequest>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    switch(state, holder, peer, path, body, false).await
}

/// `POST /swish/payers/{name}/key`. Gives the number a new key, answered once as
/// `{"payer", "key"}`. The key the request came with stops working at once, on every instance.
///
/// # Errors
///
/// `401` without the secret or the number's key, `403` with another number's key, `400` for `by`
/// or `note`, `409` when the key was replaced first by another request, `503` when the store fails.
pub async fn key(
    State(state): State<SharedState>,
    Number(holder): Number,
    peer: PeerAddr,
    headers: HeaderMap,
    path: Result<Path<String>, PathRejection>,
    body: Result<Json<PersonRequest>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    let name = named(&holder, path)?;
    let Json(body) = body?;
    let current = access::presented(&headers);
    let ip = peer.ip().to_string();
    let key = payers::replace_key(&state, &name, current.as_deref(), &body, &ip).await.map_err(refused)?;
    Ok(Json(json!({ "payer": name, "key": key })))
}

async fn switch(
    state: SharedState,
    holder: KeyHolder,
    peer: PeerAddr,
    path: Result<Path<String>, PathRejection>,
    body: Result<Json<PersonRequest>, JsonRejection>,
    disabled: bool,
) -> Result<Json<Value>, ApiError> {
    let name = named(&holder, path)?;
    let Json(body) = body?;
    payers::set_disabled(&state, &name, disabled, &body, &peer.ip().to_string())
        .await
        .map_err(refused)?;
    current(&state, &name).await
}

// The number the path names, which must be the key's own.
fn named(holder: &KeyHolder, path: Result<Path<String>, PathRejection>) -> Result<String, ApiError> {
    let Path(name) = path?;
    access::covers(holder, &name)?;
    Ok(name)
}

fn refused(error: PayerError) -> ApiError {
    match error {
        PayerError::Invalid(message) => ApiError::BadRequest(message),
        PayerError::NotFound => ApiError::NotFound,
        PayerError::Conflict(message) => ApiError::Conflict(message),
        PayerError::Failed(message) => ApiError::ServiceUnavailable(message),
    }
}

// The answer, and health's count of numbers offline, show whether Swish answers the new
// certificate without waiting for the number's next scheduled probe.
async fn probe_now(state: &SharedState, name: &str) {
    if let Some(payer) = state.payer(name) {
        let probe = crate::swish::client::probe(&payer.client, &state.config.swish_base_url).await;
        *payer.probe.write().await = Some(probe);
    }
}

async fn current(state: &SharedState, name: &str) -> Result<Json<Value>, ApiError> {
    let entry = payers::find(state, name).await.ok_or(ApiError::NotFound)?;
    Ok(Json(view(&entry).await))
}

async fn view(entry: &Entry) -> Value {
    let now = unix_now();
    let (probe, tls, signing) = match &entry.loaded {
        Ok(payer) => (*payer.probe.read().await, Some(payer.tls_expires), Some(payer.signing_expires)),
        Err(_) => (None, None, None),
    };
    let date = |at: i64| to_stockholm(at).date();
    let days = |at: i64| (at - now).div_euclid(86_400);
    json!({
        "name":                    entry.record.name,
        "nickname":                entry.record.nickname,
        "number":                  entry.record.number,
        "disabled":                entry.record.disabled,
        "usable":                  entry.loaded.is_ok(),
        "problem":                 entry.loaded.as_ref().err(),
        "tls_expires":             tls.map(date),
        "tls_expires_in_days":     tls.map(days),
        "signing_expires":         signing.map(date),
        "signing_expires_in_days": signing.map(days),
        "swish_online":            probe.map(|p| p.reachable),
        "added_by":                entry.record.added_by,
        "created_at":              to_stockholm(entry.record.created_at).rfc3339(),
    })
}
