//! `POST /swish/payout/{payer}/{reference}/confirm`: a person's word that a payout was paid.

use axum::{
    Json,
    extract::{Path, State, rejection::{JsonRejection, PathRejection}},
};
use serde_json::{Value, json};

use crate::domain::{
    payout::ConfirmOutcome,
    request::ConfirmRequest,
    review::{CONFIRMED_PAID, declined_over_details},
    validate::{MAX_BY, MAX_NOTE},
};
use crate::error::ApiError;
use crate::events::{SwishLogFields, announce, log_admin_event, prepare, published};
use crate::http::{access::{self, Number}, client_ip::PeerAddr};
use crate::state::SharedState;
use crate::store::PayoutStore;
use crate::swish::reconcile::CHECKS;

use super::person;

/// `POST /swish/payout/{payer}/{reference}/confirm`. Marks a payout `PAID` because a person
/// checked with the bank or the recipient that the money arrived, and records who and why.
///
/// Accepted on `NEEDS_REVIEW` at any time, and on `DEBITED` or `ERROR` once swisha has asked Swish
/// about it one last time, about 48 hours after it was made: until then Swish may still give its
/// last word. Never on a payout that was redone, since the redo carries it on. The status and who
/// and why are one write. Answers `200` with the payout as it now stands.
///
/// # Errors
///
/// - `401` without the secret or the payer's key, `403` with another number's key.
/// - `400` when `by` or `note` is blank, too long or holds a control or invisible character.
/// - `404` when the payer has no payout under that reference.
/// - `409` when the payout may not be confirmed now, saying why and when it can be, or was redone.
/// - `503` when the database fails before the confirmation is stored. Once it is, the answer is
///   `200`, even when the payout cannot be read back.
pub async fn confirm(
    State(state): State<SharedState>,
    Number(holder): Number,
    peer: PeerAddr,
    path: Result<Path<(String, String)>, PathRejection>,
    body: Result<Json<ConfirmRequest>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    let Path((payer, reference)) = path?;
    access::covers(&holder, &payer)?;
    super::payout_reference(&reference)?;
    let Json(body) = body?;
    let by = person::text(&body.by, "by", MAX_BY)?;
    let note = person::text(&body.note, "note", MAX_NOTE)?;
    let ip = peer.ip().to_string();

    let fields = SwishLogFields {
        payer:         payer.clone(),
        reference:     reference.clone(),
        swish_ref:     None,
        status:        Some("PAID".into()),
        amount:        None,
        payee_alias:   None,
        error_code:    None,
        error_message: None,
        ip:            Some(ip.clone()),
        actor:         Some(by.clone()),
        note:          Some(note),
    };
    let (confirmation, payload) = prepare(CONFIRMED_PAID, fields);
    let confirmed = state.store.confirm_paid(&confirmation).await.map_err(|e| {
        tracing::warn!("[swish/confirm] store error for {payer}/{reference}: {e}");
        ApiError::ServiceUnavailable(
            "The database failed while confirming the payout. Read it to see whether it was confirmed.".into(),
        )
    })?;
    match confirmed {
        ConfirmOutcome::Confirmed => {}
        ConfirmOutcome::Refused(status) => {
            let over_details = status == "DECLINED"
                && matches!(
                    state.store.status_view(&payer, &reference).await,
                    Ok(Some(view)) if declined_over_details(&view)
                );
            let why = if over_details {
                "Swish declined this payout over the recipient's details and no money moved, so it cannot \
                 have been paid. Correct the details and send a new payout under a new reference."
                    .into()
            } else {
                not_confirmable(&status, &payer, state.payer(&payer).is_some())
            };
            return Err(ApiError::Conflict(why));
        }
        ConfirmOutcome::Redone(redo) => {
            return Err(ApiError::Conflict(format!(
                "This payout was redone as {redo}, so it is closed: the redo carries it on."
            )));
        }
        ConfirmOutcome::NotFound => return Err(ApiError::NotFound),
    }

    published(&state, &confirmation, &payload).await;
    let line = format!("Payout {payer}/{reference} confirmed paid by {by}");
    let context = json!({ "payer": &payer, "reference": &reference, "by": &by });
    log_admin_event("info", &line, context, &ip, &state).await;

    // Stored: a 503 now would say "try again", and the retry would hear it is already paid.
    let answer = announce(&payer, &reference, &state)
        .await
        .unwrap_or_else(|| json!({ "payer": &payer, "reference": &reference, "status": "PAID" }));
    Ok(Json(answer))
}

// Why a person may not confirm a payout in `status` now, and when they can. Its checks wait while
// no certificate of `payer` is loaded here, so no hour can be promised then.
fn not_confirmable(status: &str, payer: &str, loaded: bool) -> String {
    let hours = CHECKS.window.as_secs() / 3600;
    match status {
        "DEBITED" | "ERROR" | "CREATED" | "PENDING" if !loaded => format!(
            "swisha cannot ask Swish about this payout yet: the certificates of payer '{payer}' are not \
             loaded here. Its checks wait for them, and it can be confirmed once its last check has run."
        ),
        "PAID" => "This payout is already paid.".into(),
        "DEBITED" | "ERROR" => format!(
            "swisha is still asking Swish about this payout, and Swish may yet give its last word. \
             It can be confirmed once swisha has asked one last time, about {hours} hours after it \
             was made."
        ),
        "CREATED" | "PENDING" => format!(
            "Swish has not answered about this payout yet. If it still has not when the checks end, \
             about {hours} hours after it was made, it can be confirmed then."
        ),
        "DECLINED" => "Swish refused this payout and no money moved, so it cannot have been paid. \
                       Redo it under a new reference instead."
            .into(),
        other => format!("A payout with status {other} cannot be confirmed as paid."),
    }
}
