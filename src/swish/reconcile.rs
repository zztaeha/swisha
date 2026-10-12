//! Following up on payouts Swish has not given its last word on, by reading only.
//!
//! Every number is its own account at Swish and is checked on its own: each has a worker that
//! claims only that number's due checks and asks about them one a second. A backlog, a slow
//! answer or an unusable certificate on one number never delays another's checks.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Notify;

use crate::domain::payout::{ACCEPTED, CheckSchedule};
use crate::domain::status::{PayoutStatus, UNANSWERED};
use crate::error::Result;
use crate::events::{Change, SwishLogFields, log_swish_event, record};
use crate::store::PayoutStore;
use crate::{
    state::{PayerState, SharedState},
    swish::submit::{NoAnswer, PollAnswer, lookup_payout_status},
};

/// The checks the service runs: 5 minutes after a payout last changed, then hourly, until 48 hours
/// after it was made. Then a person decides.
pub const CHECKS: CheckSchedule = CheckSchedule {
    first_after: Duration::from_secs(5 * 60),
    every: Duration::from_secs(60 * 60),
    window: Duration::from_secs(48 * 60 * 60),
};

/// How often the checks look for numbers with a payout due a check.
pub const CHECK_TICK: Duration = Duration::from_secs(5);

/// How many of its own due checks a number's worker claims at a time.
pub const PER_CLAIM: usize = 5;

/// How long a number's worker waits after each question, so one number asks Swish at most once a
/// second, 3,600 times an hour, however many of its payouts are due.
pub const BETWEEN_CHECKS: Duration = Duration::from_secs(1);

/// Runs the checks on `schedule` until the process stops.
///
/// Every [`CHECK_TICK`] it wakes the worker of each number with a payout due a check, starting one
/// the first time a number has one. Each worker asks only about its own number's payouts, so no
/// number ever waits for another's. The service runs this with [`CHECKS`].
pub async fn run(state: SharedState, schedule: CheckSchedule) {
    let mut workers: HashMap<String, Arc<Notify>> = HashMap::new();
    let mut tick = tokio::time::interval(CHECK_TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        let due = match state.store.payers_with_due_checks(&schedule).await {
            Ok(due) => due,
            Err(e) => {
                tracing::warn!("[checks] store error finding due checks: {e}");
                continue;
            }
        };
        note_waiting(&state, due.iter().filter(|payer| state.payer(payer).is_none()).cloned().collect());
        for payer in due {
            workers
                .entry(payer)
                .or_insert_with_key(|payer| worker(state.clone(), payer.clone(), schedule))
                .notify_one();
        }
    }
}

// Numbers whose payouts wait on certificates this instance has not loaded: said as each starts and
// stops waiting, and counted for health, so no payout waits in silence.
fn note_waiting(state: &SharedState, waiting: HashSet<String>) {
    let mut noted = state.checks_waiting.lock().unwrap_or_else(|e| e.into_inner());
    for payer in waiting.difference(&noted) {
        tracing::warn!("[checks] {payer} has payouts due, but no certificate loaded here can ask Swish");
    }
    for payer in noted.difference(&waiting) {
        tracing::info!("[checks] {payer}'s payouts can be asked about again");
    }
    *noted = waiting;
}

// One number's worker: woken when the number has due checks, it works through them until none
// are left. Keyed by the name on the payouts, so a number that is not loaded still gets one.
fn worker(state: SharedState, payer: String, schedule: CheckSchedule) -> Arc<Notify> {
    let wake = Arc::new(Notify::new());
    let woken = Arc::clone(&wake);
    tokio::spawn(async move {
        loop {
            woken.notified().await;
            loop {
                // Boxed, so a worker waiting to be woken holds its loop and not a whole sweep: 496
                // bytes against 2,800, measured with 10,000 numbers' workers idle.
                match Box::pin(sweep(&state, &payer, &schedule)).await {
                    Ok(0) => break,
                    Ok(_) => {}
                    Err(e) => {
                        tracing::warn!("[checks] store error claiming checks for {payer}: {e}");
                        break;
                    }
                }
            }
        }
    });
    wake
}

/// Claims up to [`PER_CLAIM`] of one number's payouts due a check on `schedule` and asks Swish
/// about each in turn, [`BETWEEN_CHECKS`] apart, returning how many it asked about.
///
/// Claims nothing for a number this instance has not loaded: only that number's certificate can
/// ask Swish, so its checks wait for an instance that has it.
///
/// # Errors
///
/// When the store cannot claim the due payouts. Swish giving no answer about one is not an error:
/// that payout is asked about again at its next check.
pub async fn sweep(state: &SharedState, payer: &str, schedule: &CheckSchedule) -> Result<usize> {
    if state.payer(payer).is_none() {
        return Ok(0);
    }
    let due = state.store.claim_due_checks(payer, schedule, PER_CLAIM).await?;
    let count = due.len();
    for check in due {
        reconcile(check.payer, check.reference, check.last, state.clone()).await;
        tokio::time::sleep(BETWEEN_CHECKS).await;
    }
    Ok(count)
}

/// Asks Swish once what became of a payout, and records the answer.
///
/// **Reads only.** swisha does not resubmit under any circumstance, because a resubmission needs
/// a fresh `payoutInstructionUUID` that Swish cannot tie to the original, so it could debit
/// twice. Swish is asked with the certificate of the payer that sent the payout.
///
/// On the `last` check swisha stops asking and a person decides: a payout Swish never answered
/// becomes `NEEDS_REVIEW`, and one Swish left at `DEBITED` or `ERROR` keeps Swish's word, with a
/// `CHECKS_ENDED` event. A last check that could not ask Swish, or record what it said, has not
/// happened: the payout keeps its checks.
pub async fn reconcile(payer: String, reference: String, last: bool, state: SharedState) {
    if let Asked::Recorded(swish_ref) = ask(&payer, &reference, &state).await
        && last
    {
        end_checks(&payer, &reference, swish_ref.as_deref(), &state).await;
    }
}

// Whether a check did its part: Swish asked about the payout and what it said, if anything,
// recorded. One that did not must not end the checks.
enum Asked {
    Recorded(Option<String>),
    Unrecorded,
}

// One question to Swish, and its answer recorded when it is new.
async fn ask(payer: &str, reference: &str, state: &SharedState) -> Asked {
    let snapshot = match state.store.snapshot(payer, reference).await {
        Ok(Some(snapshot)) => snapshot,
        Ok(None) => {
            tracing::warn!("[reconcile] payout not found: {payer}/{reference}");
            return Asked::Unrecorded;
        }
        Err(e) => {
            tracing::warn!("[reconcile] store error fetching {payer}/{reference}: {e}");
            return Asked::Unrecorded;
        }
    };

    // Blank counts as absent, as it does everywhere else here. Asking about an empty UUID would ask
    // Swish about the collection rather than an instruction, and read the answer as a status.
    let recorded = snapshot.swish_ref.filter(|r| !r.trim().is_empty());
    let Some(swish_ref) = recorded else {
        tracing::warn!("[reconcile] {payer}/{reference} has no Swish reference to ask about");
        needs_review(payer, reference, None, "No Swish reference was ever recorded.", state).await;
        return Asked::Recorded(None);
    };

    // Only the payer that sent it can ask Swish about it. One that is not loaded gives no answer,
    // which is not an outcome: Swish may still pay it, so the payout waits until it can be asked.
    let Some(sender) = state.payer(payer) else {
        tracing::warn!("[reconcile] {payer}/{reference}: {payer} is not loaded, so Swish cannot be asked");
        return Asked::Unrecorded;
    };

    let stored = snapshot.status.unwrap_or_default();
    match lookup_payout_status(&sender, &swish_ref, state).await {
        Ok(answer) if answer.status == "PENDING" => {
            tracing::info!("[reconcile] {payer}/{reference}: Swish has no outcome yet");
            held(payer, reference, &swish_ref, state).await;
        }
        // Swish repeating what is stored adds nothing to the trail.
        Ok(answer) if answer.status == stored => {
            tracing::info!("[reconcile] {payer}/{reference}: Swish still says {stored}");
        }
        Ok(answer) => {
            if !settle(payer, reference, &swish_ref, &answer, state).await {
                return Asked::Unrecorded;
            }
        }
        // Swish's word that it holds no such instruction is an answer: one it never received.
        Err(NoAnswer::NotFound) => {
            tracing::warn!("[reconcile] {payer}/{reference}: Swish has no record of it");
        }
        Err(why) => {
            tracing::warn!("[reconcile] {payer}/{reference}: no answer from Swish ({why:?})");
            return Asked::Unrecorded;
        }
    }
    Asked::Recorded(Some(swish_ref))
}

/// What Swish says before a person pays a payout again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BeforeRedo {
    /// Swish says the payout and every one it redoes failed, or never received them.
    Unpaid,
    /// Swish reports the money for this payout gone, with this status: paying again pays twice.
    Paid {
        /// The payout's reference.
        reference: String,
        /// `PAID` or `DEBITED`.
        status: String,
    },
    /// Swish holds this payout without an outcome yet, and may still pay it.
    Unresolved(String),
    /// Swish has no record of this payout, though it showed it held it before.
    Unrecorded(String),
    /// Swish has no record of this payout, and swisha none of how its send ended: Swish may have
    /// taken it with the record of its `201` lost, so its having none proves nothing.
    SendUnknown(String),
    /// swisha holds no instruction UUID for this payout, so Swish cannot be asked about it.
    NoInstruction(String),
    /// Swish could not be asked about this payout, or gave no usable answer, so nothing is known.
    Unasked(String),
}

/// Asks Swish about `reference`, and every payout it redoes, one redo back at a time, before a
/// person pays it again. Reads only, and records what Swish says as the checks would.
///
/// Stops at the first payout Swish has not said failed: a redo goes out only on Swish's word that
/// nothing before it was paid. Swish having no record of a payout counts as failed only where it
/// never showed it held it, and swisha recorded the send ending without Swish taking it: refused,
/// or never answered. Swish cannot pay what it never received.
///
/// # Errors
///
/// When the store cannot be read.
pub async fn before_redo(state: &SharedState, sender: &PayerState, reference: &str) -> Result<BeforeRedo> {
    let payer = sender.name.as_str();
    let mut asked = HashSet::new();
    let mut next = Some(reference.to_string());
    while let Some(reference) = next.take() {
        if !asked.insert(reference.clone()) {
            break;
        }
        let Some(view) = state.store.status_view(payer, &reference).await? else { break };
        next = view.replaces;
        let Some(swish_ref) = view.swish_ref.filter(|r| !r.trim().is_empty()) else {
            return Ok(BeforeRedo::NoInstruction(reference));
        };
        // A payout whose send ended without Swish taking it never reached Swish. One still in flight
        // goes on to the claim, which refuses it as one Swish may still pay.
        let stored = view.status.as_deref().and_then(PayoutStatus::parse);
        let in_flight = stored.is_some_and(|status| UNANSWERED.contains(&status));
        let ended = view.send_unaccepted
            && stored.is_some_and(|status| matches!(status, PayoutStatus::Declined | PayoutStatus::NeedsReview));
        let answer = match lookup_payout_status(sender, &swish_ref, state).await {
            Ok(answer) => answer,
            Err(NoAnswer::NotFound) if view.accepted => return Ok(BeforeRedo::Unrecorded(reference)),
            Err(NoAnswer::NotFound) if in_flight || ended => continue,
            Err(NoAnswer::NotFound) => return Ok(BeforeRedo::SendUnknown(reference)),
            Err(why) => {
                tracing::warn!("[swish/redo] {payer}/{reference}: no usable answer from Swish ({why:?})");
                return Ok(BeforeRedo::Unasked(reference));
            }
        };
        if answer.status != "PENDING" && view.status.as_deref() != Some(answer.status.as_str()) {
            settle(payer, &reference, &swish_ref, &answer, state).await;
        }
        match PayoutStatus::parse(&answer.status) {
            Some(PayoutStatus::Error | PayoutStatus::Declined) => {}
            Some(status) if status.is_terminal() => {
                return Ok(BeforeRedo::Paid { reference, status: answer.status });
            }
            _ => {
                held(payer, &reference, &swish_ref, state).await;
                return Ok(BeforeRedo::Unresolved(reference));
            }
        }
    }
    Ok(BeforeRedo::Unpaid)
}

// Swish answered about the instruction, so it holds it: recorded once, so that Swish having no
// record of it later reads as the contradiction it is, never as a payout that was never sent.
pub(crate) async fn held(payer: &str, reference: &str, swish_ref: &str, state: &SharedState) {
    match state.store.status_view(payer, reference).await {
        Ok(Some(view)) if !view.accepted => {
            log_swish_event(ACCEPTED, fields(payer, reference, Some(swish_ref), None, None), state).await;
        }
        Ok(_) => {}
        Err(e) => tracing::warn!("[reconcile] {payer}/{reference}: could not read whether Swish holds it: {e}"),
    }
}

// Swish's answer is always recorded, and announced only if the store took it: a refused write means
// the payout already holds an answer, announced when it was written. False when nothing was stored.
async fn settle(payer: &str, reference: &str, swish_ref: &str, answer: &PollAnswer, state: &SharedState) -> bool {
    let status = answer.status.as_str();
    tracing::info!("[reconcile] {payer}/{reference} resolved to {status}");
    let recorded = SwishLogFields {
        error_code: answer.error_code.clone(),
        ..fields(payer, reference, Some(swish_ref), Some(status), answer.error_message.as_deref())
    };
    match record(status, recorded, Change::Status(status), state).await {
        Ok(recorded) => {
            recorded.announce_if_applied(payer, reference, state).await;
            true
        }
        Err(e) => {
            tracing::warn!("[reconcile] {payer}/{reference}: could not record Swish's {status}: {e}");
            false
        }
    }
}

// The last check: a person takes over. A payout Swish never answered goes to review; one Swish
// left at DEBITED or ERROR keeps Swish's word, and the trail says the checks have ended.
async fn end_checks(payer: &str, reference: &str, swish_ref: Option<&str>, state: &SharedState) {
    let status = match state.store.snapshot(payer, reference).await {
        Ok(Some(snapshot)) => snapshot.status.unwrap_or_default(),
        Ok(None) => return,
        Err(e) => {
            tracing::warn!("[reconcile] store error ending the checks on {payer}/{reference}: {e}");
            return;
        }
    };
    match PayoutStatus::parse(&status) {
        Some(PayoutStatus::Created | PayoutStatus::Pending) => {
            let why = "Swish gave no outcome before the checks ended.";
            needs_review(payer, reference, swish_ref, why, state).await;
        }
        Some(PayoutStatus::Debited | PayoutStatus::Error) => {
            let ended = fields(payer, reference, swish_ref, Some(&status), None);
            match record("CHECKS_ENDED", ended, Change::EndChecks(&status), state).await {
                Ok(recorded) => {
                    if recorded.applied {
                        tracing::warn!("[reconcile] {payer}/{reference}: checks ended at {status}");
                    }
                    recorded.announce_if_applied(payer, reference, state).await;
                }
                Err(e) => tracing::warn!("[reconcile] {payer}/{reference}: could not end the checks: {e}"),
            }
        }
        _ => {}
    }
}

// swisha stops chasing and says so. The store refuses this over an answer Swish already gave, and
// NEEDS_REVIEW is not terminal, so an answer arriving later still settles the payout.
async fn needs_review(
    payer: &str,
    reference: &str,
    swish_ref: Option<&str>,
    why: &str,
    state: &SharedState,
) {
    let recorded = fields(payer, reference, swish_ref, Some("NEEDS_REVIEW"), Some(why));
    match record("NEEDS_REVIEW", recorded, Change::NeedsReview, state).await {
        Ok(recorded) => {
            if recorded.applied {
                tracing::warn!("[reconcile] {payer}/{reference} needs review: {why}");
            }
            recorded.announce_if_applied(payer, reference, state).await;
        }
        Err(e) => tracing::warn!("[reconcile] {payer}/{reference}: could not record the review: {e}"),
    }
}

fn fields(
    payer: &str,
    reference: &str,
    swish_ref: Option<&str>,
    status: Option<&str>,
    error_message: Option<&str>,
) -> SwishLogFields {
    SwishLogFields {
        payer:         payer.to_string(),
        reference:     reference.to_string(),
        swish_ref:     swish_ref.map(str::to_string),
        status:        status.map(str::to_string),
        amount:        None,
        payee_alias:   None,
        error_code:    None,
        error_message: error_message.map(str::to_string),
        ip:            None,
        actor:         None,
        note:          None,
    }
}
