//! `POST /swish/payout`: check the request as Swish will, claim the reference, submit once. And
//! `POST /swish/payout/{payer}/{reference}/redo`: a person's new payout in place of one that
//! failed, claimed under a new reference and sent the same way.

use axum::{
    extract::{Path, State, rejection::{JsonRejection, PathRejection}},
    http::StatusCode,
    Json,
};
use serde_json::{json, Value};

use crate::domain::{
    payout::{ACCEPTED, ClaimOutcome, EventRecord, NewPayout, NewRedo, RedoOutcome},
    request::{PayoutRequest, RedoRequest},
    review::{REDONE, declined_over_details},
    status::{ANSWERS, HELD, PayoutStatus, TERMINAL_ADVANCE},
    validate::{
        MAX_BY, MAX_NOTE, MESSAGE_CHARACTERS, is_valid_message, is_valid_personnummer,
        is_valid_reference, is_whole_ore, normalize_phone, normalize_ssn, swish_message,
        validate_phone,
    },
};
use crate::domain::payer::KeyHolder;
use crate::error::ApiError;
use crate::events::{
    Change, Recorded, SwishLogFields, announce, current_payload, log_admin_event, log_swish_event, prepare,
    published,
};
use crate::http::{
    access::{self, Number},
    client_ip::PeerAddr,
    rate_limit, response::refusal_status,
};
use crate::store::PayoutStore;
use crate::{
    state::{Following, PayerState, SharedState},
    swish::{
        payload::ExecutePayoutArgs,
        random_payout_uuid,
        reconcile::{BeforeRedo, before_redo, held},
        submit::{PollAnswer, Polled, poll_payout_status, read_rejection, sign_payout, submit_payout},
    },
};

use super::person;

/// `POST /swish/payout`. Validates, claims the payer's reference, submits once from that payer,
/// and returns `202`. The payer is the number whose key the request carries; a `payer` in the body
/// must name the same one.
///
/// When Swish refuses the payout, the answer is shaped like the status endpoint's, with the
/// status from [`refusal_status`]. A reference that is already spent returns `409` whatever state
/// the payout reached. That is never a reason to retry: see [`PayoutStore::claim`].
///
/// # Errors
///
/// - `401` without the secret or a number's key, `403` with another number's key than `payer`,
///   and `429` past the rate limit.
/// - `400` for a disabled payer, and anything Swish would refuse, before anything is stored.
/// - `409` for a spent reference.
/// - `503` when a certificate has expired or could not be loaded, the database fails, or Swish
///   cannot be reached. A claim the database refused is read back first: one an earlier request
///   holds is `409`, and the `503` says whether anything was stored.
///
/// A body that is not the expected JSON answers `400`, `415` or `422`. Swish's own refusal
/// arrives as `Ok`, with the status [`refusal_status`] chose and the reason in the body.
pub async fn payout(
    State(state): State<SharedState>,
    Number(holder): Number,
    peer: PeerAddr,
    body: Result<Json<PayoutRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let Json(body) = body?;
    if let Some(name) = body.payer.as_deref() {
        access::covers(&holder, name)?;
    }
    let payer = ready_to_pay(&state, &holder).await?;
    let payer_name = payer.name.as_str();
    let ip = limited(&state, peer, payer_name)?;

    // Swish refuses any other character with FF08, which would spend the reference on a payout
    // that never had a chance. Refused here, nothing is stored and the reference stays usable.
    if body.reference.trim().is_empty() {
        return Err(ApiError::BadRequest("reference is required.".into()));
    }
    if !is_valid_reference(&body.reference) {
        return Err(ApiError::BadRequest(
            "reference must be 1 to 35 characters of A-Z, a-z, 0-9 and -.".into(),
        ));
    }

    if body.amount < 1.0 || body.amount > state.config.swish_max_payout {
        return Err(ApiError::BadRequest(format!(
            "amount must be between 1 and {} SEK.",
            state.config.swish_max_payout
        )));
    }
    if !is_whole_ore(body.amount) {
        return Err(ApiError::BadRequest("amount must have at most two decimals.".into()));
    }

    let payee_alias = normalize_phone(&body.payee_alias);
    if !validate_phone(&payee_alias) {
        return Err(ApiError::BadRequest(format!(
            "invalid Swish number: {}.",
            body.payee_alias
        )));
    }

    // Swish verifies every payout against the recipient's personnummer and refuses one without
    // it, so a missing or malformed one is refused here instead. Blank counts as absent.
    let supplied = body.payee_ssn.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let Some(raw_ssn) = supplied else {
        return Err(ApiError::BadRequest(
            "payee_ssn is required: Swish verifies every payout against the recipient's \
             personnummer.".into(),
        ));
    };
    let payee_ssn = normalize_ssn(raw_ssn);
    if !is_valid_personnummer(&payee_ssn) {
        return Err(ApiError::BadRequest(
            "payee_ssn must be a 12-digit personnummer, YYYYMMDDNNNN, with a date that exists and a \
             valid check digit."
                .into(),
        ));
    }

    // Cut to the 50 characters Swish shows here rather than in the payload, so the message
    // stored is exactly the one sent.
    let message = match body.message.as_deref() {
        Some(text) if !text.trim().is_empty() => swish_message(text),
        _ => swish_message(&state.config.payout_message.replace("{reference}", &body.reference)),
    };
    if !is_valid_message(&message) {
        return Err(ApiError::BadRequest(format!(
            "message may only hold {MESSAGE_CHARACTERS}. Swish refuses anything else, and the \
             refusal would spend the reference."
        )));
    }

    certificates_valid(&payer)?;

    let payout_uuid = random_payout_uuid();
    let callback_identifier = random_payout_uuid();

    // The trail's first event is stored with the claim, so a payout is never claimed without it.
    let opened = prepare(
        "INITIATED",
        initiated(payer_name, &body.reference, &payout_uuid, body.amount, &payee_alias, &ip),
    );
    let claimed = state
        .store
        .claim_opening(
            &NewPayout {
                payer:               payer_name,
                reference:           &body.reference,
                payer_alias:         &payer.number,
                payee_alias:         &payee_alias,
                payee_ssn:           Some(&payee_ssn),
                amount:              body.amount,
                message:             &message,
                swish_ref:           &payout_uuid,
                callback_identifier: Some(&callback_identifier),
            },
            &opened.0,
        )
        .await;
    // A claim whose answer was lost may still have been stored, so the store decides: this
    // request's own row is sent, another's is spent, and none at all leaves the reference free.
    let claim = match claimed {
        Ok(claim) => claim,
        Err(e) => {
            tracing::warn!("Failed to persist payout record: {e}");
            match state.store.snapshot(payer_name, &body.reference).await {
                Ok(Some(stored)) => {
                    ClaimOutcome { status: stored.status.unwrap_or_default(), swish_ref: stored.swish_ref }
                }
                Ok(None) => {
                    return Err(ApiError::ServiceUnavailable(
                        "Failed to persist payout record. Nothing was stored, so this reference can be used again."
                            .into(),
                    ));
                }
                Err(_) => {
                    return Err(ApiError::ServiceUnavailable(
                        "Failed to persist payout record, and could not read back whether it was stored. \
                         Sending the same reference again is safe: if it was stored, the answer is 409."
                            .into(),
                    ));
                }
            }
        }
    };

    // Only a claim this request won may submit. Anything else, a status this version does not
    // know included, is a reference already spent: the store never rewrote it, so neither may this.
    if !claim.claimed_by(&payout_uuid) {
        if claim.status == PayoutStatus::Declined.as_str()
            && let Ok(Some(view)) = state.store.status_view(payer_name, &body.reference).await
            && declined_over_details(&view)
        {
            return Err(ApiError::Conflict(details_declined(view.error_code.as_deref().unwrap_or_default())));
        }
        return Err(ApiError::Conflict(already_spent(&claim, payer_name, &body.reference)));
    }

    send(&state, &payer, Outgoing {
        reference: body.reference,
        payout_uuid,
        callback_identifier,
        payee_alias,
        payee_ssn: Some(payee_ssn),
        amount: body.amount,
        message,
        replaces: None,
        opened: Some(opened),
    }, ip).await
}

/// `POST /swish/payout/{payer}/{reference}/redo`. A person's request to pay a payout again under
/// a new reference, because Swish refused it, reported it failed, or never answered.
///
/// The new payout copies the recipient, amount and message of the one it replaces, and is sent
/// exactly as `POST /swish/payout` sends one, answering in the same shape with `replaces` added.
/// swisha never does this on its own: a person asks, after checking with the bank that the money
/// did not leave or has come back.
///
/// Swish is asked about the payout, and every payout it redoes, before anything is claimed: see
/// [`before_redo`]. Only its word that none was paid lets the redo go out.
///
/// # Errors
///
/// - `401` without the secret or the payer's key, `403` with another number's key, and `429`
///   past the rate limit.
/// - `400` for a disabled payer, a reference Swish would refuse or the same reference, a blank
///   `by`, or a payout for more than `SWISH_MAX_PAYOUT` allows now.
/// - `422` for a body without `by`, or with a field of the wrong type.
/// - `404` when the payer has no payout under the reference to redo.
/// - `409` when the payout may not be redone: its money left, or that of a payout it redoes; Swish
///   may still pay it, or cannot vouch that it did not; Swish declined it over the recipient's
///   details, which a redo would send again; it was already redone; or the new reference is spent.
/// - `503` when Swish gives no usable answer, the payer's certificates could not be loaded or have
///   expired, or the database fails.
pub async fn redo(
    State(state): State<SharedState>,
    Number(holder): Number,
    peer: PeerAddr,
    path: Result<Path<(String, String)>, PathRejection>,
    body: Result<Json<RedoRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let Path((payer_name, original)) = path?;
    access::covers(&holder, &payer_name)?;
    super::payout_reference(&original)?;
    let Json(body) = body?;

    let payer = ready_to_pay(&state, &holder).await?;
    let ip = limited(&state, peer, &payer_name)?;

    if !is_valid_reference(&body.reference) {
        return Err(ApiError::BadRequest(
            "reference must be 1 to 35 characters of A-Z, a-z, 0-9 and -.".into(),
        ));
    }
    if body.reference == original {
        return Err(ApiError::BadRequest(
            "a redo needs a new reference: the one it replaces stays spent.".into(),
        ));
    }
    let by = person::text(&body.by, "by", MAX_BY)?;
    let note = match body.note.as_deref().filter(|note| !note.trim().is_empty()) {
        Some(note) => Some(person::text(note, "note", MAX_NOTE)?),
        None => None,
    };
    certificates_valid(&payer)?;

    // What the payout holds now decides before Swish is asked: a redo is held to today's ceiling,
    // and a decline over the recipient's details would only be declined again.
    let view = state.store.status_view(&payer_name, &original).await.map_err(|e| {
        tracing::warn!("[swish/redo] store error reading {payer_name}/{original}: {e}");
        ApiError::ServiceUnavailable("Could not read the payout, so nothing was redone.".into())
    })?;
    let view = view.ok_or(ApiError::NotFound)?;
    if view.amount > state.config.swish_max_payout {
        return Err(ApiError::BadRequest(format!(
            "This payout is for {:.2} SEK, above the most a payout may be now, {} SEK, so nothing was \
             redone.",
            view.amount, state.config.swish_max_payout
        )));
    }
    if declined_over_details(&view) {
        return Err(ApiError::Conflict(details_declined(view.error_code.as_deref().unwrap_or_default())));
    }

    let asked = before_redo(&state, &payer, &original).await.map_err(|e| {
        tracing::warn!("[swish/redo] store error reading {payer_name}/{original}: {e}");
        ApiError::ServiceUnavailable("Could not read the payout, so nothing was redone.".into())
    })?;
    match asked {
        BeforeRedo::Unpaid => {}
        BeforeRedo::Paid { reference, status } => {
            return Err(ApiError::Conflict(paid_before(&original, &reference, &status)));
        }
        BeforeRedo::Unresolved(reference) => {
            return Err(ApiError::Conflict(format!(
                "Swish holds {} without an outcome yet, and may still pay it, so nothing was redone.",
                which(&original, &reference)
            )));
        }
        BeforeRedo::Unrecorded(reference) => {
            return Err(ApiError::Conflict(format!(
                "Swish has no record of {}, though it showed it held it before, so nothing was \
                 redone. Check the payout with Swish.",
                which(&original, &reference)
            )));
        }
        BeforeRedo::SendUnknown(reference) => {
            return Err(ApiError::Conflict(format!(
                "Swish has no record of {} and swisha none of how its send ended, so Swish may have \
                 taken it, and nothing was redone. Check your bank statement: if it was never paid, \
                 send it as a new payout under a new reference.",
                which(&original, &reference)
            )));
        }
        BeforeRedo::NoInstruction(reference) => {
            return Err(ApiError::Conflict(format!(
                "swisha holds no instruction UUID for {}, so Swish cannot be asked about it and \
                 nothing was redone. Check the payout with Swish.",
                which(&original, &reference)
            )));
        }
        BeforeRedo::Unasked(reference) => {
            return Err(ApiError::ServiceUnavailable(format!(
                "Swish gave no usable answer about {}, so nothing was redone. Try again shortly.",
                which(&original, &reference)
            )));
        }
    }

    // The payout replaced records who redid it and why, in the same write as the claim.
    let fields = SwishLogFields {
        payer:         payer_name.clone(),
        reference:     original.clone(),
        swish_ref:     None,
        status:        None,
        amount:        None,
        payee_alias:   None,
        error_code:    None,
        error_message: None,
        ip:            Some(ip.clone()),
        actor:         Some(by.clone()),
        note,
    };
    let (redone, payload) = prepare(REDONE, fields);
    let payout_uuid = random_payout_uuid();
    let callback_identifier = random_payout_uuid();
    let new = NewRedo {
        payer:               &payer_name,
        replaces:            &original,
        reference:           &body.reference,
        payer_alias:         &payer.number,
        swish_ref:           &payout_uuid,
        callback_identifier: Some(&callback_identifier),
    };
    // A claim whose answer was lost may still have been stored. Asked again with the same
    // instruction, the store finds this request's own claim, or makes it now.
    let outcome = match state.store.claim_redo(&new, &redone).await {
        Ok(outcome) => outcome,
        Err(e) => {
            tracing::warn!("Failed to persist redo record, asking again: {e}");
            state.store.claim_redo(&new, &redone).await.map_err(|e| {
                tracing::warn!("Failed to persist redo record: {e}");
                ApiError::ServiceUnavailable(
                    "Failed to persist payout record, and could not read back whether it was stored. \
                     Sending the same redo again is safe: if it was stored, the answer is 409 and \
                     names it."
                        .into(),
                )
            })?
        }
    };
    let details = match outcome {
        RedoOutcome::Claimed(details) => details,
        RedoOutcome::NotFound => return Err(ApiError::NotFound),
        RedoOutcome::NotRedoable(status) => return Err(ApiError::Conflict(not_redoable(&status))),
        RedoOutcome::AlreadyRedone(reference) => {
            return Err(ApiError::Conflict(format!(
                "This payout was already redone as {reference}. If that one failed too, redo \
                 {reference} instead."
            )));
        }
        RedoOutcome::EarlierPaid { reference, status } => {
            return Err(ApiError::Conflict(paid_before(&original, &reference, &status)));
        }
        RedoOutcome::ReferenceTaken(_) => {
            return Err(ApiError::Conflict(format!(
                "The reference {} is already taken by another payout, so nothing was redone. Redo \
                 this payout under a reference no payout has used.",
                body.reference
            )));
        }
    };

    // The new payout goes to Swish first. The payout replaced is announced, and the service log
    // told, after it: neither is the new payout's, and a slow write must not hold its POST.
    let line = format!("Payout {payer_name}/{original} redone as {} by {by}", body.reference);
    let context = json!({
        "payer": &payer_name, "reference": &original, "redo": &body.reference, "by": &by,
    });
    let sent = send(&state, &payer, Outgoing {
        reference: body.reference,
        payout_uuid,
        callback_identifier,
        payee_alias: details.payee_alias,
        payee_ssn: details.payee_ssn,
        amount: details.amount,
        message: details.message,
        replaces: Some(original.clone()),
        opened: None,
    }, ip.clone()).await;
    published(&state, &redone, &payload).await;
    log_admin_event("info", &line, context, &ip, &state).await;
    announce(&payer_name, &original, &state).await;
    sent
}

// Who is asking, after the rate limit. Every caller over the socket shares one bucket per payer.
fn limited(state: &SharedState, peer: PeerAddr, payer: &str) -> Result<String, ApiError> {
    let caller = peer.ip();
    if !rate_limit::allow(caller, payer, state.config.payouts_per_minute) {
        tracing::warn!("[swish/payout] rate limit exceeded for {caller} paying from {payer}");
        return Err(ApiError::TooManyRequests);
    }
    Ok(caller.to_string())
}

// A number that may take a new payout: not disabled, as its key read it just now, and with
// certificates that loaded. Refused before the claim, so the reference stays free.
async fn ready_to_pay(
    state: &SharedState,
    holder: &KeyHolder,
) -> Result<std::sync::Arc<PayerState>, ApiError> {
    let name = holder.name.as_str();
    if holder.disabled {
        return Err(ApiError::BadRequest(format!(
            "payer {name} is disabled and takes no new payouts. Nothing was stored, so this \
             reference can be used again."
        )));
    }
    let entry = crate::payers::find(state, name).await.ok_or_else(|| {
        ApiError::ServiceUnavailable(format!(
            "payer {name} could not be loaded yet. Nothing was stored, so this reference can be \
             used again."
        ))
    })?;
    entry.loaded.map_err(|problem| {
        ApiError::ServiceUnavailable(format!(
            "The certificates of payer '{name}' could not be loaded: {problem}. Nothing was \
             stored, so this reference can be used again."
        ))
    })
}

// An expired certificate can neither authenticate nor sign, so Swish could only refuse the payout.
// Refused here instead, before the claim, so the reference survives the renewal.
fn certificates_valid(payer: &PayerState) -> Result<(), ApiError> {
    let now = crate::util::time::unix_now();
    for (which, expires) in [("TLS", payer.tls_expires), ("signing", payer.signing_expires)] {
        if expires <= now {
            let date = crate::util::time::to_stockholm(expires).date();
            return Err(ApiError::ServiceUnavailable(format!(
                "The {which} certificate of payer '{name}' expired on {date}. Renew it in the Swish \
                 Certificate System and upload it with PUT /swish/payers/{name}/certificates. \
                 Nothing was stored, so this reference can be used again.",
                name = payer.name
            )));
        }
    }
    Ok(())
}

// A payout this request claimed, on its way to Swish.
struct Outgoing {
    reference:           String,
    payout_uuid:         String,
    callback_identifier: String,
    payee_alias:         String,
    payee_ssn:           Option<String>,
    amount:              f64,
    message:             String,
    replaces:            Option<String>,
    // INITIATED as the claim stored it, to announce; a redo, claimed without it, writes its own.
    opened:              Option<(EventRecord, String)>,
}

// The event a payout's trail opens with: its submission.
fn initiated(
    payer: &str,
    reference: &str,
    uuid: &str,
    amount: f64,
    payee_alias: &str,
    ip: &str,
) -> SwishLogFields {
    SwishLogFields {
        payer:         payer.to_string(),
        reference:     reference.to_string(),
        swish_ref:     Some(uuid.to_string()),
        status:        None,
        amount:        Some(amount),
        payee_alias:   Some(payee_alias.to_string()),
        error_code:    None,
        error_message: None,
        ip:            Some(ip.to_string()),
        actor:         None,
        note:          None,
    }
}

// The one way a claimed payout reaches Swish: logged, submitted once, and answered. Both the payout
// route and a person's redo come through here, so neither can submit any other way.
async fn send(
    state: &SharedState,
    payer: &PayerState,
    out: Outgoing,
    ip: String,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let payer_name = payer.name.as_str();

    let args = ExecutePayoutArgs {
        reference:     &out.reference,
        payout_uuid:   &out.payout_uuid,
        payee_alias:   &out.payee_alias,
        payee_ssn:     out.payee_ssn.as_deref(),
        amount:        out.amount,
        message:       &out.message,
    };

    // The trail opens before the instruction leaves, so it always starts with the submission and a
    // stop right after the answer cannot lose it. Announced or written while the payout is signed.
    let opening = async {
        match &out.opened {
            Some((record, payload)) => published(state, record, payload).await,
            None => {
                let (uuid, alias) = (&out.payout_uuid, &out.payee_alias);
                let fields = initiated(payer_name, &out.reference, uuid, out.amount, alias, &ip);
                log_swish_event("INITIATED", fields, state).await;
            }
        }
    };
    let (_, signed) = tokio::join!(opening, sign_payout(payer, &args, state));

    // Submit to Swish synchronously, only the POST. On acceptance answer 202 at once; the follow-up
    // runs in the background and delivers the outcome on the event stream. Followed from before the
    // POST, so a callback that lands first is seen.
    let following = state.followed.start(payer_name, &out.reference);
    let submitted = match signed {
        Ok(signed) => submit_payout(payer, signed, &out.callback_identifier, state).await,
        Err(e) => Err(e),
    };
    match submitted {
        // What the refusal proves decides the status, and the caller gets the status endpoint's
        // explanation. Swish's raw body goes to the audit trail only.
        Err(ApiError::SwishRejected { code, body: swish_body }) => {
            let rejection = read_rejection(code, &swish_body);
            if rejection.held {
                held(payer_name, &out.reference, &out.payout_uuid, state).await;
            }
            let (reference, named, left) = (&out.reference, &rejection.code, rejection.status.as_str());
            tracing::warn!("[swish/payout] Swish refused {payer_name}/{reference}: {code} with {named}, left {left}");
            log_outcome(state, payer_name, &out, &ip, &format!("refused by Swish ({})", rejection.code));
            let cause = if rejection.code == code.to_string() {
                format!("Swish answered {code}")
            } else {
                format!("Swish answered {code} with {}", rejection.code)
            };
            let unaccepted = Unaccepted {
                status: rejection.status.as_str(),
                error_code: Some(rejection.code),
                detail: swish_body,
                cause,
            };
            Ok(settle_send(state, payer_name, &out, &ip, unaccepted, refusal_status(code)).await)
        }
        Err(e) => {
            // Swish may hold the instruction, so the reference is spent. The answer says so in the
            // status endpoint's shape, as a refusal's does, rather than as a bare 503 to retry.
            let code = match e {
                ApiError::ServiceUnavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
                _ => StatusCode::INTERNAL_SERVER_ERROR,
            };
            tracing::warn!("[swish/payout] could not send {payer_name}/{}, left PENDING: {e}", out.reference);
            log_outcome(state, payer_name, &out, &ip, "could not be sent");
            let unaccepted = Unaccepted {
                status: "PENDING",
                error_code: None,
                detail: e.to_string(),
                cause: e.to_string().trim_end_matches('.').to_string(),
            };
            Ok(settle_send(state, payer_name, &out, &ip, unaccepted, code).await)
        }
        Ok(()) => {
            // Swish holds the instruction from here on, so its trail says so before the answer.
            let accepted = SwishLogFields {
                payer:         payer_name.to_string(),
                reference:     out.reference.clone(),
                swish_ref:     Some(out.payout_uuid.clone()),
                status:        None,
                amount:        None,
                payee_alias:   None,
                error_code:    None,
                error_message: None,
                ip:            Some(ip.clone()),
                actor:         None,
                note:          None,
            };
            log_swish_event(ACCEPTED, accepted, state).await;
            let sent = Sent {
                payer:       payer_name.to_string(),
                reference:   out.reference.clone(),
                swish_ref:   out.payout_uuid.clone(),
                amount:      out.amount,
                payee_alias: out.payee_alias.clone(),
                ip,
            };
            tokio::spawn(follow_up(sent, state.clone(), following));

            let mut answer = json!({
                "success": true,
                "payer": payer_name,
                "swish_ref": out.payout_uuid,
                "status": "CREATED",
            });
            if let Some(replaces) = out.replaces {
                answer["replaces"] = json!(replaces);
            }
            Ok((StatusCode::ACCEPTED, Json(answer)))
        }
    }
}

// The service log's line for a send Swish did not take. One it took is in the payout's own trail,
// as ACCEPTED. Not awaited: the trail is already written, and this is the service log.
fn log_outcome(state: &SharedState, payer: &str, out: &Outgoing, ip: &str, what: &str) {
    let (state, ip) = (state.clone(), ip.to_string());
    let message = format!("Payout {payer}/{} {what}", out.reference);
    let context = json!({ "payer": payer, "reference": &out.reference, "amount": out.amount });
    tokio::spawn(async move { log_admin_event("warn", &message, context, &ip, &state).await });
}

// A send Swish did not accept: the status it leaves the payout in, Swish's code, the raw answer
// for the trail, and the cause for the caller.
struct Unaccepted {
    status:     &'static str,
    error_code: Option<String>,
    detail:     String,
    cause:      String,
}

// Records what a send Swish did not accept left the payout as, and answers with the payout as it
// now stands, under `code`. A callback that answered first keeps its answer, and is announced alone.
async fn settle_send(
    state: &SharedState,
    payer: &str,
    out: &Outgoing,
    ip: &str,
    unaccepted: Unaccepted,
    code: StatusCode,
) -> (StatusCode, Json<Value>) {
    let Unaccepted { status, error_code, detail, cause } = unaccepted;
    let fields = SwishLogFields {
        payer:         payer.to_string(),
        reference:     out.reference.clone(),
        swish_ref:     Some(out.payout_uuid.clone()),
        status:        Some(status.into()),
        amount:        None,
        payee_alias:   None,
        error_code,
        error_message: Some(detail),
        ip:            Some(ip.to_string()),
        actor:         None,
        note:          None,
    };
    let announced = match recorded(status, fields, state).await {
        Some(recorded) if recorded.applied => recorded.announce(payer, &out.reference, state).await,
        recorded => {
            if let Some(recorded) = recorded {
                recorded.alone(state).await;
            }
            current_payload(payer, &out.reference, state).await.ok().flatten()
        }
    };
    let mut answer = announced.unwrap_or_else(|| {
        json!({ "payer": payer, "reference": &out.reference, "status": status, "swish_ref": &out.payout_uuid })
    });
    // A callback's answer that Swish gives only about an instruction it holds is answered as
    // accepted, and "may have received" belongs only beside a payout still without an answer.
    let answered = answer["status"].as_str().and_then(PayoutStatus::parse).is_some_and(|s| HELD.contains(&s));
    match answer["error_message"].as_str() {
        Some(message) => answer["error"] = json!(message),
        None if !answered => answer["error"] = json!(unknown_outcome(&cause)),
        None => {}
    }
    (if answered { StatusCode::ACCEPTED } else { code }, Json(answer))
}

// A payout Swish accepted, as the follow-up records it.
struct Sent {
    payer:       String,
    reference:   String,
    swish_ref:   String,
    amount:      f64,
    payee_alias: String,
    ip:          String,
}

// Asks Swish for the outcome once it accepted the payout, and follows DEBITED on to PAID or ERROR,
// asking only for what no callback has brought. No outcome yet, or no answer, is left to the checks.
async fn follow_up(sent: Sent, state: SharedState, _following: Following) {
    let Some(sender) = state.payer(&sent.payer) else { return };
    let (payer, reference) = (&sent.payer, &sent.reference);
    let status = match poll_payout_status(&sender, reference, &sent.swish_ref, &state, ANSWERS).await {
        Ok(Polled::Settled(status)) => status.as_str().to_string(),
        Ok(Polled::Answered(answer)) => {
            record(&sent, &answer, &state).await;
            answer.status
        }
        Ok(Polled::Held) => {
            held(payer, reference, &sent.swish_ref, &state).await;
            return;
        }
        Err(e) => {
            tracing::warn!("[swish/payout] could not confirm {payer}/{reference}, left to the sweep: {e}");
            return;
        }
    };
    if status != "DEBITED" {
        return;
    }
    if let Ok(Polled::Answered(later)) =
        poll_payout_status(&sender, reference, &sent.swish_ref, &state, TERMINAL_ADVANCE.1).await
        && later.status != "DEBITED"
    {
        record(&sent, &later, &state).await;
    }
}

// Swish's answer is always recorded, and announced only if the store took it: a refused write means
// a callback already settled the payout and announced it.
async fn record(sent: &Sent, answer: &PollAnswer, state: &SharedState) {
    let fields = SwishLogFields {
        payer:         sent.payer.clone(),
        reference:     sent.reference.clone(),
        swish_ref:     Some(sent.swish_ref.clone()),
        status:        Some(answer.status.clone()),
        amount:        Some(sent.amount),
        payee_alias:   Some(sent.payee_alias.clone()),
        error_code:    answer.error_code.clone(),
        error_message: answer.error_message.clone(),
        ip:            Some(sent.ip.clone()),
        actor:         None,
        note:          None,
    };
    if let Some(recorded) = recorded(&answer.status, fields, state).await {
        recorded.announce_if_applied(&sent.payer, &sent.reference, state).await;
    }
}

// Records a status and its event as one write, answering what was written, or `None` when nothing
// was. A failed write leaves the payout as it was, for the checks to ask about again.
async fn recorded(status: &str, fields: SwishLogFields, state: &SharedState) -> Option<Recorded> {
    let (payer, reference) = (fields.payer.clone(), fields.reference.clone());
    crate::events::record(status, fields, Change::Status(status), state)
        .await
        .map_err(|e| tracing::warn!("[swish/payout] could not record {status} for {payer}/{reference}: {e}"))
        .ok()
}

// What a caller is told when Swish may hold the payout: never "failed", which invites paying again,
// but that the reference is spent and swisha is finding out. The cause follows, for an operator.
fn unknown_outcome(cause: &str) -> String {
    format!(
        "Swish may have received this payout, so the reference is spent: swisha keeps asking Swish \
         what became of it. {cause}."
    )
}

// The payout a redo is about, as an answer names it: this one, or one before it.
fn which(original: &str, reference: &str) -> String {
    if reference == original {
        "this payout".into()
    } else {
        format!("{reference}, which came before this one,")
    }
}

// Why a payout is not paid again once its money, or an earlier payout's, may have left.
fn paid_before(original: &str, reference: &str, status: &str) -> String {
    let which = which(original, reference);
    if PayoutStatus::parse(status).is_some_and(PayoutStatus::is_terminal) {
        format!("Swish reports {which} as {status}: its money left your account, so nothing was redone.")
    } else {
        format!("Swish reports {which} as {status}, which this swisha does not know, so nothing was redone.")
    }
}

// Why a person may not redo a payout in `status`.
fn not_redoable(status: &str) -> String {
    match status {
        "PAID" => "This payout was paid, so it cannot be redone.".into(),
        "DEBITED" => "The money for this payout has left your account, so it cannot be redone. If Swish \
                      later reports it failed, it can be redone then."
            .into(),
        "CREATED" | "PENDING" => "Swish has not answered about this payout yet and may still pay it, so \
                                  it cannot be redone now."
            .into(),
        other => format!("A payout with status {other} cannot be redone."),
    }
}

// Why a payout Swish declined over the recipient's details is not sent again as it stands.
fn details_declined(code: &str) -> String {
    format!(
        "Swish declined this payout over the recipient's details ({code}), and would decline it again \
         as it stands, so it is not redone. Correct the details and send a new payout under a new \
         reference."
    )
}

// What a repeat request is told. A failed payout is not a free one: Swish may hold it, so paying it
// again is redo's, which asks Swish first and pays once.
fn already_spent(claim: &ClaimOutcome, payer: &str, reference: &str) -> String {
    let swish_ref = claim.swish_ref.as_deref().unwrap_or("");
    let redo = format!("POST /swish/payout/{payer}/{reference}/redo");
    match claim.status.as_str() {
        "PAID" => format!("Payout already completed for this reference. Swish ref: {swish_ref}"),
        "DEBITED" => format!(
            "Payout already debited for this reference, awaiting Swish's PAID. Swish ref: {swish_ref}"
        ),
        "CREATED" | "PENDING" => {
            format!("Payout already in progress for this reference. Swish ref: {swish_ref}")
        }
        "NEEDS_REVIEW" => format!(
            "Swish never gave an outcome for this payout. If it should still be paid, redo it with \
             {redo}: swisha asks Swish about it first, and pays it once."
        ),
        "ERROR" => format!(
            "Swish reported this payout failed. If it should still go out, redo it with {redo}: \
             swisha asks Swish about it first, and pays it once."
        ),
        "DECLINED" => format!(
            "Swish declined this payout. If it should still be paid, redo it with {redo}: swisha \
             asks Swish about it first, and pays it once."
        ),
        other => format!(
            "A payout already exists for this reference with status {other}, which this swisha \
             does not know, so it can neither redo nor confirm it. Check it with Swish."
        ),
    }
}
