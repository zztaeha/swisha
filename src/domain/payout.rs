//! Values that cross the storage boundary.
//!
//! Deliberately plain: no driver types, no row handles, nothing that assumes a particular engine
//! returned them. A [`PayoutStore`](crate::store::PayoutStore) implementation converts to and
//! from these and nothing else.

/// A payout as the caller submitted it, on its way into storage.
pub struct NewPayout<'a> {
    /// Which payer pays, by name. With the reference, the idempotency key.
    pub payer: &'a str,
    /// The caller's own identifier. Sent to Swish as `payerPaymentReference`.
    pub reference: &'a str,
    /// The payer's Swish number at the time, kept for the audit trail.
    pub payer_alias: &'a str,
    /// The recipient's Swish number, already normalized to `46XXXXXXXXX`.
    pub payee_alias: &'a str,
    /// The recipient's personnummer as 12 digits. Swish checks it against the phone number, and
    /// the payout endpoint always supplies it; `None` is left to other callers of the store.
    pub payee_ssn: Option<&'a str>,
    /// Amount in SEK.
    pub amount: f64,
    /// What the recipient reads in their Swish app, already truncated to Swish's 50 characters.
    pub message: &'a str,
    /// The `payoutInstructionUUID` swisha generated for this attempt.
    pub swish_ref: &'a str,
    /// The `callbackIdentifier` sent with the instruction, which Swish echoes on its callback.
    pub callback_identifier: Option<&'a str>,
}

/// The state of the row after a claim attempt.
pub struct ClaimOutcome {
    /// The status the row now holds.
    pub status: String,
    /// Whichever instruction reference now owns the payout: the caller's own when the claim
    /// succeeded, an earlier one when it did not.
    pub swish_ref: Option<String>,
}

impl ClaimOutcome {
    /// Whether this caller won the claim, and may therefore submit to Swish.
    ///
    /// True only when the row now carries the reference this caller supplied. Anything else
    /// means another attempt owns the payout and this one must not send.
    ///
    /// ```
    /// use swisha::domain::payout::ClaimOutcome;
    ///
    /// let won = ClaimOutcome { status: "CREATED".into(), swish_ref: Some("AAAA".into()) };
    /// assert!(won.claimed_by("AAAA"));
    ///
    /// // An earlier request owns the reference, whatever state it reached.
    /// let spent = ClaimOutcome { status: "PAID".into(), swish_ref: Some("BBBB".into()) };
    /// assert!(!spent.claimed_by("AAAA"));
    /// ```
    pub fn claimed_by(&self, swish_ref: &str) -> bool {
        self.status == "CREATED" && self.swish_ref.as_deref() == Some(swish_ref)
    }
}

/// When swisha asks Swish about a payout it has no last word on, and for how long.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CheckSchedule {
    /// The first check, this long after the payout last changed.
    pub first_after: std::time::Duration,
    /// Each later check, this long after the one before.
    pub every: std::time::Duration,
    /// Checks end this long after the payout was made, with one last check once it has passed.
    pub window: std::time::Duration,
}

/// A payout due a check, claimed by the sweep.
pub struct DueCheck {
    /// The payer that sent it, whose certificate has to ask Swish about it.
    pub payer: String,
    /// The payout's reference.
    pub reference: String,
    /// Whether this is the last check: the window has passed, and after this one a person decides.
    pub last: bool,
}

/// What a status lookup returns.
pub struct PayoutSnapshot {
    /// The stored status, or `None` if the row has never held one.
    pub status: Option<String>,
    /// The `payoutInstructionUUID` Swish knows the payout by.
    pub swish_ref: Option<String>,
    /// The `callbackIdentifier` sent with the instruction, when one was.
    pub callback_identifier: Option<String>,
}

/// A payout and what its trail says beside the status, gathered in one round trip: everything the
/// status endpoint answers with, and what a redo checks before it asks Swish.
pub struct StatusView {
    /// The stored status.
    pub status: Option<String>,
    /// Amount in SEK.
    pub amount: f64,
    /// The `payoutInstructionUUID` Swish knows the payout by.
    pub swish_ref: Option<String>,
    /// The most recent Swish error code recorded against the payout.
    pub error_code: Option<String>,
    /// Whether the checks have ended: swisha asked Swish one last time and recorded the answer.
    pub checks_ended: bool,
    /// The reference of the payout this one redoes, when it is a redo.
    pub replaces: Option<String>,
    /// The reference of the redo that replaced this payout, when there is one.
    pub replaced_by: Option<String>,
    /// Whether Swish ever reported the payout `DEBITED`: the money left the account at some point.
    pub was_debited: bool,
    /// Whether a payout this one redoes, or one before that, is `PAID` or `DEBITED`: Swish paid it
    /// after a person had it paid again.
    pub earlier_paid: bool,
    /// Whether Swish ever showed it holds the instruction: its `201`, a callback, or an answer about
    /// it, with or without an outcome.
    pub accepted: bool,
    /// Whether the trail holds how the send ended without Swish taking it: `PENDING` for a send that
    /// got no answer, or `DECLINED`. Without one, Swish having no record of it proves nothing.
    pub send_unaccepted: bool,
    /// Whether Swish reported the payout `ERROR` or `DECLINED` after a person last confirmed it paid,
    /// so the recipient may not have the money.
    pub contradicted: bool,
    /// The latest confirmation by a person that the payout was paid, when there is one.
    pub manual_confirmation: Option<ManualConfirmation>,
}

/// A person's word that a payout was paid, as recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManualConfirmation {
    /// Who confirmed it.
    pub by: String,
    /// Why: what they checked.
    pub note: String,
    /// When, as seconds since the Unix epoch.
    pub at: i64,
}

/// What asking to confirm a payout as paid found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfirmOutcome {
    /// The payout is now `PAID`.
    Confirmed,
    /// The payout holds this status, which may not be confirmed now.
    Refused(String),
    /// The payout was redone, under this reference, so the redo carries it on.
    Redone(String),
    /// The payer has no payout under that reference.
    NotFound,
}

/// A redo on its way into storage: a new payout under a new reference, replacing an earlier one
/// of the same payer and copying its recipient, amount and message.
pub struct NewRedo<'a> {
    /// Which payer pays, by name: the payer of the payout it replaces.
    pub payer: &'a str,
    /// The reference of the payout it replaces.
    pub replaces: &'a str,
    /// Its own reference, which must be unspent.
    pub reference: &'a str,
    /// The payer's Swish number now, kept for the audit trail.
    pub payer_alias: &'a str,
    /// The `payoutInstructionUUID` swisha generated for it.
    pub swish_ref: &'a str,
    /// The `callbackIdentifier` sent with it.
    pub callback_identifier: Option<&'a str>,
}

/// What a redo sends, copied from the payout it replaces. Debug output leaves out the recipient.
#[derive(Clone, PartialEq)]
pub struct RedoDetails {
    /// The recipient's Swish number.
    pub payee_alias: String,
    /// The recipient's personnummer.
    pub payee_ssn: Option<String>,
    /// Amount in SEK.
    pub amount: f64,
    /// What the recipient reads.
    pub message: String,
}

impl std::fmt::Debug for RedoDetails {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RedoDetails")
            .field("amount", &self.amount)
            .field("message", &self.message)
            .finish_non_exhaustive()
    }
}

/// What asking to redo a payout found.
pub enum RedoOutcome {
    /// The redo is claimed and may be sent, with these details.
    Claimed(RedoDetails),
    /// The payer has no payout under the reference to redo.
    NotFound,
    /// The payout to redo holds this status, which may not be redone.
    NotRedoable(String),
    /// The payout was already redone, under this reference.
    AlreadyRedone(String),
    /// A payout the one to redo replaces, however far back, holds this status: `PAID` or `DEBITED`,
    /// whose money left, or one this version does not know. Paying again could pay twice.
    EarlierPaid {
        /// That payout's reference.
        reference: String,
        /// Its status.
        status: String,
    },
    /// The new reference is already spent, by the payout described.
    ReferenceTaken(ClaimOutcome),
}

/// The event recorded when Swish shows it holds a payout's instruction: its `201` to the send, `RP09`
/// to it, or a question about it answered without an outcome yet. After it, Swish having no record
/// is no reason to redo.
pub const ACCEPTED: &str = "ACCEPTED";

/// One row of the Swish event log, which is the audit trail for a payout.
pub struct EventRecord {
    /// The payer the payout belongs to.
    pub payer: String,
    /// The payout's reference.
    pub reference: String,
    /// The `payoutInstructionUUID`, when one exists yet.
    pub swish_ref: Option<String>,
    /// What happened, such as `INITIATED`, `DEBITED` or `ERROR`.
    pub event: String,
    /// The status at the time, when the event carries one.
    pub status: Option<String>,
    /// Amount in SEK. Present on events where it is meaningful.
    pub amount: Option<f64>,
    /// The recipient's Swish number.
    pub payee_alias: Option<String>,
    /// The Swish error code, on failures.
    pub error_code: Option<String>,
    /// The error text, on failures.
    pub error_message: Option<String>,
    /// The address the request came from.
    pub ip: Option<String>,
    /// Who did it, when a person did.
    pub actor: Option<String>,
    /// Why, in their words.
    pub note: Option<String>,
}
