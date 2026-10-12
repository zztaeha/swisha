//! The crate's only submission to Swish, and polling for the outcome.

use serde::Serialize;
use serde_json::{Value, value::RawValue};
use tokio::time::{sleep, timeout, Duration};

use crate::domain::status::{ANSWERS, PayoutStatus};
use crate::error::ApiError;
use crate::{
    state::{PayerState, SharedState},
    swish::{
        client::{reaches, swish_get, swish_post},
        payload::{ExecutePayoutArgs, PayoutPayload},
        sign::sign_payload,
    },
};

/// A payer's payout instruction and its signature, made by [`sign_payout`] for
/// [`submit_payout`] alone.
pub struct Signed {
    payload:   String,
    signature: String,
}

/// Builds a payer's payout instruction and signs it with the payer's key, sending nothing. Waits
/// for one of [`signing`](crate::state::AppState::signing)'s slots first.
///
/// # Errors
///
/// [`ApiError::Internal`] when the instruction cannot be serialized or signed.
pub async fn sign_payout(
    payer: &PayerState,
    args: &ExecutePayoutArgs<'_>,
    state: &SharedState,
) -> Result<Signed, ApiError> {
    let payload = PayoutPayload::new(
        args,
        &payer.number,
        &payer.signing_serial,
        crate::util::time::now_utc().utc_z(),
    );

    let payload_str = serde_json::to_string(&payload)
        .map_err(|e| ApiError::Internal(e.into()))?;

    // RSA-4096 signing takes about 3.4 ms of CPU: on the blocking pool, so no other request on this
    // worker waits behind it, and in a slot, so a burst queues instead of growing the pool.
    let _slot = state.signing.acquire().await.map_err(|e| ApiError::Internal(e.into()))?;
    let key = std::sync::Arc::clone(&payer.signing_key);
    let (payload_str, signed) = tokio::task::spawn_blocking(move || {
        let signed = sign_payload(&payload_str, &key);
        (payload_str, signed)
    })
    .await
    .map_err(|e| ApiError::Internal(e.into()))?;
    let signature = signed.map_err(|_| ApiError::Internal(crate::error::err("Failed to sign payout.")))?;
    Ok(Signed { payload: payload_str, signature })
}

/// Submits a signed payout to Swish from the payer that signed it, sent with its certificate.
///
/// `Ok` means Swish answered `201` and holds the instruction; the outcome comes later, from
/// [`poll_payout_status`] or a callback.
///
/// **This is the crate's only `POST` to Swish, and it has exactly one call site.**
/// `tests/no_resubmission.rs` asserts both by scanning the source. Adding a second call site is
/// how a payout gets made twice.
///
/// # Errors
///
/// [`ApiError::SwishRejected`] with Swish's status and body when it answers anything but `201`;
/// [`read_rejection`] says what that proves. [`ApiError::ServiceUnavailable`] when Swish cannot
/// be reached, in which case it may still hold the instruction.
pub async fn submit_payout(
    payer: &PayerState,
    signed: Signed,
    callback_identifier: &str,
    state: &SharedState,
) -> Result<(), ApiError> {
    let Signed { payload, signature } = signed;
    let raw_payload = RawValue::from_string(payload)
        .map_err(|e| ApiError::Internal(e.into()))?;

    // callbackIdentifier sits outside the signed payload. Swish echoes it as a header on the
    // callback, proof the callback is theirs that an address allowlist alone cannot give.
    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct MssPayoutRequest<'a> {
        payload:             &'a RawValue,
        callback_url:        &'a str,
        callback_identifier: &'a str,
        signature:           &'a str,
    }

    let request_body = MssPayoutRequest {
        payload:             &raw_payload,
        callback_url:        &state.config.swish_callback_url,
        callback_identifier,
        signature:           &signature,
    };

    let mss_url = format!("{}/swish-cpcapi/api/v1/payouts", state.config.swish_base_url);

    // Not reaching Swish is Swish being unavailable, not swisha failing. Reported as such so a
    // caller can tell "try later" from "something here is broken", and so the reason survives
    // into the answer instead of being swallowed by Internal's deliberate silence.
    let resp = swish_post(&mss_url, &request_body, &payer.client)
        .await
        .map_err(|e| ApiError::ServiceUnavailable(format!("Could not reach Swish: {e}")))?;
    note(payer, resp.status());

    let status_code = resp.status().as_u16();
    if status_code != 201 {
        let body = resp.text().await.unwrap_or_else(|e| {
            tracing::warn!("[swish/submit] failed to read error response body: {e}");
            String::new()
        });
        return Err(ApiError::SwishRejected { code: status_code, body });
    }

    Ok(())
}

// What an answer from Swish says about the number's reach, for health.
fn note(payer: &PayerState, status: reqwest::StatusCode) {
    if reaches(status) { payer.answered() } else { payer.refused() }
}

/// What a refused submission tells swisha.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rejection {
    /// The code Swish named, or its HTTP status when the body named none. The error table
    /// explains both.
    pub code: String,
    /// `DECLINED` when the refusal proves Swish stored no instruction, otherwise `PENDING`.
    pub status: PayoutStatus,
    /// Whether Swish named `RP09`: an instruction already holds the UUID, so Swish holds this one.
    pub held: bool,
}

// The statuses Swish documents for refusing an instruction before storing it. Only ever narrowed:
// DECLINED tells a person no money moved.
const REFUSED_OUTRIGHT: [u16; 6] = [400, 401, 403, 415, 422, 429];

/// Reads a refused submission: the code Swish named, and whether an instruction can exist.
///
/// Swish answers with a JSON array of `{errorCode, errorMessage}`, and the first code is the one
/// recorded. A body without one, such as a proxy's HTML page, falls back to the HTTP status.
///
/// Only a status Swish documents as refusing outright proves nothing was stored, and even then
/// `RP09` says an instruction already holds the UUID. Anything else may have reached Swish, so the
/// payout is `PENDING`, with no answer yet, for the sweep to ask about.
///
/// # Examples
///
/// ```
/// use swisha::domain::status::PayoutStatus;
/// use swisha::swish::submit::read_rejection;
///
/// let refused = read_rejection(422, r#"[{"errorCode":"ACMT07","errorMessage":"Not enrolled"}]"#);
/// assert_eq!(refused.code, "ACMT07");
/// assert_eq!(refused.status, PayoutStatus::Declined, "nothing stored, no money moved");
///
/// let unknown = read_rejection(503, "<html>Service Unavailable</html>");
/// assert_eq!(unknown.code, "503");
/// assert_eq!(unknown.status, PayoutStatus::Pending, "Swish may hold it, so the sweep asks");
///
/// let taken = read_rejection(422, r#"[{"errorCode":"RP09","errorMessage":"Not available"}]"#);
/// assert!(taken.held && taken.status == PayoutStatus::Pending, "an instruction holds the UUID");
/// ```
pub fn read_rejection(http_status: u16, body: &str) -> Rejection {
    let parsed: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let errors = match &parsed {
        Value::Array(items) => items.as_slice(),
        single => std::slice::from_ref(single),
    };
    let codes: Vec<&str> = errors
        .iter()
        .filter_map(|error| error.get("errorCode")?.as_str())
        .map(str::trim)
        .filter(|code| !code.is_empty())
        .collect();

    let held = codes.iter().any(|code| code.eq_ignore_ascii_case("RP09"));
    let nothing_stored = REFUSED_OUTRIGHT.contains(&http_status) && !held;

    Rejection {
        code: codes.first().map_or_else(|| http_status.to_string(), |code| code.to_string()),
        status: if nothing_stored { PayoutStatus::Declined } else { PayoutStatus::Pending },
        held,
    }
}

/// What Swish said about a payout when asked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PollAnswer {
    /// `PAID`, `DEBITED`, `DECLINED` or `ERROR`, or `PENDING` when Swish had not resolved it.
    pub status: String,
    /// Swish's error code, when it reports one.
    pub error_code: Option<String>,
    /// Swish's error text, when it reports one.
    pub error_message: Option<String>,
}

/// Why a question to Swish about a payout got no answer. Never an outcome: the payout is left as
/// it is, and asked about again later.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoAnswer {
    /// Swish could not be reached, or did not answer within 4 seconds.
    Unreachable,
    /// Swish has no record of the payout: it answered `400` naming `PA01`, as Swish documents and
    /// its simulator does for an instruction it never received. Swish documents no `404` here, so
    /// one is [`Unusable`](NoAnswer::Unusable), not this.
    NotFound,
    /// Swish answered with something other than a payout, such as `429` or a body that is not
    /// JSON.
    Unusable,
}

/// Asks Swish once what became of a payout, with the certificate of the payer that sent it.
///
/// Answers `PAID`, `DEBITED`, `DECLINED` or `ERROR` as Swish wrote it, with its error code and
/// message, and `PENDING` for a status that is not an outcome yet, such as `CREATED` or
/// `INITIATED`. Reads only: this never resubmits. A question that gets no answer is logged at
/// debug: whoever asked warns once, rather than once a question.
///
/// # Errors
///
/// [`NoAnswer`], saying why Swish gave none.
pub async fn lookup_payout_status(
    payer: &PayerState,
    payout_uuid: &str,
    state: &SharedState,
) -> Result<PollAnswer, NoAnswer> {
    let url = format!("{}/swish-cpcapi/api/v1/payouts/{payout_uuid}", state.config.swish_base_url);

    let Ok(Ok(response)) = timeout(Duration::from_secs(4), swish_get(&url, &payer.client)).await else {
        tracing::debug!("[swish/lookup] no answer from Swish about {payout_uuid}");
        return Err(NoAnswer::Unreachable);
    };
    note(payer, response.status());
    if !response.status().is_success() {
        let status = response.status();
        tracing::debug!("[swish/lookup] Swish answered {status} about {payout_uuid}");
        let unknown = status.as_u16() == 400
            && response.text().await.is_ok_and(|body| read_rejection(400, &body).code.eq_ignore_ascii_case("PA01"));
        return Err(if unknown { NoAnswer::NotFound } else { NoAnswer::Unusable });
    }
    let data: Value = response.json().await.map_err(|e| {
        tracing::debug!("[swish/lookup] unreadable answer about {payout_uuid}: {e}");
        NoAnswer::Unusable
    })?;

    let text = |key: &str| {
        data[key].as_str().map(str::trim).filter(|t| !t.is_empty()).map(str::to_string)
    };
    let status = data["status"].as_str().and_then(PayoutStatus::parse);
    Ok(match status.filter(|status| ANSWERS.contains(status)) {
        Some(status) => PollAnswer {
            status: status.as_str().to_string(),
            error_code: text("errorCode"),
            error_message: text("errorMessage"),
        },
        None => PollAnswer { status: "PENDING".to_string(), error_code: None, error_message: None },
    })
}

/// What a poll found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Polled {
    /// What Swish answered.
    Answered(PollAnswer),
    /// Swish holds the instruction, and gave no outcome yet.
    Held,
    /// The payout already holds this status, one of those asked for, as a callback brought it, so
    /// Swish was not asked for it again.
    Settled(PayoutStatus),
}

/// Asks Swish what became of the payout `payer` sent as `reference` up to 8 times, half a second
/// after the call and then every 2 seconds, stopping at the first answer in `until`.
///
/// Before each question it looks at what the payout took on this instance since it was sent, as
/// [`Followed`](crate::state::Followed) holds it: a status in `until`, which a callback brought,
/// ends the poll as [`Polled::Settled`] without asking Swish for what swisha already knows.
/// Otherwise it answers with the last answer Swish gave when none was in `until`, such as `DEBITED`
/// while waiting for `PAID`, and [`Polled::Held`] when Swish only said it holds the instruction;
/// the checks ask again later. Reads only: this never resubmits.
///
/// # Errors
///
/// [`ApiError::ServiceUnavailable`] when Swish gave no answer about the payout on any attempt: it
/// could not be reached, said it has no record of the payout, or answered with nothing usable. The
/// outcome is unknown, which is neither pending nor failed.
pub async fn poll_payout_status(
    payer: &PayerState,
    reference: &str,
    payout_uuid: &str,
    state: &SharedState,
    until: &[PayoutStatus],
) -> Result<Polled, ApiError> {
    let (mut contact, mut not_found, mut answered) = (false, false, false);
    let mut last = None;

    for attempt in 0u8..8 {
        // Short first delay: Swish usually resolves within a second of the POST.
        let delay = if attempt == 0 { Duration::from_millis(500) } else { Duration::from_secs(2) };
        sleep(delay).await;

        if let Some(status) = state.followed.status(&payer.name, reference)
            && until.contains(&status)
        {
            return Ok(Polled::Settled(status));
        }
        match lookup_payout_status(payer, payout_uuid, state).await {
            Ok(answer) => {
                (contact, answered) = (true, true);
                if PayoutStatus::parse(&answer.status).is_some_and(|status| until.contains(&status)) {
                    return Ok(Polled::Answered(answer));
                }
                if answer.status != "PENDING" {
                    last = Some(answer);
                }
            }
            Err(NoAnswer::NotFound) => (contact, not_found) = (true, true),
            Err(NoAnswer::Unusable) => contact = true,
            Err(NoAnswer::Unreachable) => {}
        }
    }

    if let Some(answer) = last {
        return Ok(Polled::Answered(answer));
    }
    // Swish answered about the instruction, so it holds it, whatever else it said in between.
    if answered {
        return Ok(Polled::Held);
    }
    Err(ApiError::ServiceUnavailable(if !contact {
        "Swish API unreachable during status polling.".into()
    } else if not_found {
        format!("Swish has no record of payout {payout_uuid}.")
    } else {
        format!("Swish gave no usable answer about payout {payout_uuid} in 8 questions.")
    }))
}
