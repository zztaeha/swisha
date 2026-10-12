//! Where payouts and the numbers that pay them are stored, and what the store must guarantee.
//!
//! PostgreSQL 18 is the only database swisha supports, through [`postgres::PostgresStore`].
//! [`PayoutStore`] is defined by what each operation must guarantee rather than by SQL, and
//! [`conformance`] is that contract as executable checks.

pub mod conformance;

pub mod postgres;

use crate::domain::payer::{AddPayerOutcome, KeyHolder, LogLine, NewPayer, PayerRecord};
use crate::domain::payout::{
    CheckSchedule, ClaimOutcome, ConfirmOutcome, DueCheck, EventRecord, NewPayout, NewRedo,
    PayoutSnapshot, RedoOutcome, StatusView,
};
use crate::error::Result;

/// The storage contract, specified by what each operation must guarantee rather than how.
///
/// [`postgres::PostgresStore`] implements it, and PostgreSQL is the only database swisha
/// supports. [`conformance::run`] checks every guarantee against a store.
///
/// The methods marked **MUST** carry the payment guarantees. Getting one of them wrong is how a
/// payout gets made twice, so read those before changing the store.
pub trait PayoutStore: Send + Sync {
    /// Claims a payer's reference for payout, returning the row's state after the attempt.
    ///
    /// A payout is identified by its payer and its reference together: the same reference under
    /// two payers is two payouts.
    ///
    /// **MUST be atomic**: two concurrent calls for one payer and reference yield exactly one
    /// claim. This is the double-payout guard, and everything else depends on it.
    ///
    /// **MUST NOT rewrite a payout that already exists**, whatever status it holds, including
    /// one this version does not know. One reference is one instruction, forever: a repeat
    /// request cannot change an amount, a recipient or the instruction UUID, and a failed payout
    /// is paid again under a new reference.
    fn claim(&self, new: &NewPayout<'_>) -> impl Future<Output = Result<ClaimOutcome>> + Send;

    /// Claims exactly as [`claim`](Self::claim) does and, beside a claim this call won, writes
    /// `opened`, the event the payout's trail starts with.
    ///
    /// **MUST write `opened` only beside a claim this call won**, never beside a repeat or the
    /// loser of a race. An engine that can **SHOULD** write the two as one, both or neither: then a
    /// payout whose first event cannot be stored is not claimed either. This default writes them
    /// one after the other.
    fn claim_opening(
        &self,
        new: &NewPayout<'_>,
        opened: &EventRecord,
    ) -> impl Future<Output = Result<ClaimOutcome>> + Send {
        async move {
            let claim = self.claim(new).await?;
            if claim.claimed_by(new.swish_ref) {
                self.record_event(opened).await?;
            }
            Ok(claim)
        }
    }

    /// The stored payout, or `None` when the payer has no payout under that reference.
    fn snapshot(
        &self,
        payer: &str,
        reference: &str,
    ) -> impl Future<Output = Result<Option<PayoutSnapshot>>> + Send;

    /// The payout and what its trail says beside the status, for the status endpoint and a redo:
    /// its amount and most recent error code, whether its checks have ended, what it redoes or was
    /// redone by, whether a payout it redoes was paid after all, whether it was ever `DEBITED`,
    /// whether Swish ever showed it holds the instruction, whether its send ended without Swish
    /// taking it, whether Swish reported it failed after a person confirmed it, and the latest
    /// manual confirmation. [`StatusView`] says what each means.
    ///
    /// Separate from [`snapshot`](Self::snapshot), which every callback and every reconcile reads
    /// and which skips the event-trail lookup the code costs.
    fn status_view(
        &self,
        payer: &str,
        reference: &str,
    ) -> impl Future<Output = Result<Option<StatusView>>> + Send;

    /// The payers with at least one payout due a check on `schedule`, each once, as
    /// [`claim_due_checks`](Self::claim_due_checks) defines due. Reads only.
    fn payers_with_due_checks(
        &self,
        schedule: &CheckSchedule,
    ) -> impl Future<Output = Result<Vec<String>>> + Send;

    /// Claims up to `limit` of one payer's payouts due a check on `schedule`, the longest due
    /// first.
    ///
    /// A payout is due when its status is in
    /// [`UNCONFIRMED`](crate::domain::status::UNCONFIRMED), its checks have not ended, and its last
    /// check, or its last change when it has never been checked, is `first_after` old for the first
    /// check and `every` old for each after that. Every check once `window` has passed since the
    /// payout was made is claimed with `last` set. A payout stays due until its status moves on or
    /// [`end_checks`](Self::end_checks) ends its checks, so a last check cut off by a stop or a
    /// failure is run again rather than lost.
    ///
    /// **MUST claim only `payer`'s payouts**, so one number's backlog never takes another's turn.
    /// **MUST be atomic**: two concurrent calls never claim the same payout, so two instances
    /// never ask Swish twice. **MUST NOT write a status.** The caller is about to ask Swish what
    /// happened, and a status swisha has not learned yet would be a guess.
    fn claim_due_checks(
        &self,
        payer: &str,
        schedule: &CheckSchedule,
        limit: usize,
    ) -> impl Future<Output = Result<Vec<DueCheck>>> + Send;

    /// Marks the confirmation's payout `PAID` on a person's word, and appends the confirmation,
    /// with who and why, to its trail.
    ///
    /// **MUST write only from [`CONFIRMABLE`](crate::domain::status::CONFIRMABLE), or from
    /// [`CONFIRMABLE_AFTER_CHECKS`](crate::domain::status::CONFIRMABLE_AFTER_CHECKS) once
    /// [`end_checks`](Self::end_checks) has ended its checks, and never on a payout that was
    /// redone, deciding atomically**: a confirmation must not land on a status that changed while
    /// it was being checked, nor on a payout a person already paid again. **MUST write both or
    /// neither.** Reports what it found otherwise.
    fn confirm_paid(&self, confirmation: &EventRecord) -> impl Future<Output = Result<ConfirmOutcome>> + Send;

    /// Claims a redo: a new payout under a new reference that replaces an earlier one of the
    /// same payer, copying its recipient, amount and message. Appends `redone`, who redid the
    /// payout replaced and why, to that payout's trail.
    ///
    /// **MUST decide and claim atomically**: the payout replaced exists, holds a status in
    /// [`REDOABLE`](crate::domain::status::REDOABLE), and has not been redone; every payout it
    /// replaces, however far back, still holds a status in `REDOABLE`; and the new reference is
    /// claimed as [`claim`](Self::claim) claims one. Two concurrent redos of one
    /// payout yield exactly one new payout. This is the double-payout guard for a person's
    /// redo, as `claim` is for a caller's payout. **MUST write the claim and `redone` both or
    /// neither**, and **MUST NOT** rewrite any existing payout.
    ///
    /// **MUST find its own claim:** asked again with the reference and instruction UUID it claimed,
    /// it is checked by the rules above as a new claim is, its own claim aside, and answers
    /// `Claimed` without writing. A claim whose answer was lost is then sent once, rather than taken
    /// for another person's redo.
    fn claim_redo(
        &self,
        redo: &NewRedo<'_>,
        redone: &EventRecord,
    ) -> impl Future<Output = Result<RedoOutcome>> + Send;

    /// Appends one payout event to the audit trail.
    fn record_event(&self, event: &EventRecord) -> impl Future<Output = Result<()>> + Send;

    /// Moves the event's payout to `status` where the status rules allow it, appends the event,
    /// and reports whether the status applied. With `always` the event is appended even when the
    /// status rules refuse the status, as every answer from Swish is; without it, only beside it.
    ///
    /// **MUST NOT** overwrite a [`TERMINAL`](crate::domain::status::TERMINAL) status, except by
    /// [`TERMINAL_ADVANCE`](crate::domain::status::TERMINAL_ADVANCE): `DEBITED` to `PAID` or
    /// `ERROR`, and never the reverse. A late, duplicated or out-of-order callback would otherwise
    /// move a settled payout back into flight. **MUST NOT** write a
    /// [`NO_ANSWER`](crate::domain::status::NO_ANSWER) status over anything but
    /// [`UNANSWERED`](crate::domain::status::UNANSWERED), so swisha's own "no answer" never hides
    /// one of Swish's. [`writable_condition`](crate::domain::status::writable_condition) renders
    /// all three rules as SQL, so no statement restates them by hand.
    ///
    /// **MUST write both or neither.** A status stored without the event that explains it loses
    /// its error code, and the checks then act on what is left of it.
    fn record_status(
        &self,
        status: &str,
        event: &EventRecord,
        always: bool,
    ) -> impl Future<Output = Result<bool>> + Send;

    /// Ends the checks on the event's payout while it still holds `status`, appends the event, and
    /// reports whether it did.
    ///
    /// **MUST write both or neither**, and nothing once the payout has moved on from `status` or
    /// its checks have already ended: the checks end only with their last answer recorded.
    fn end_checks(&self, status: &str, event: &EventRecord) -> impl Future<Output = Result<bool>> + Send;

    /// Appends one service log line.
    fn record_log(
        &self,
        level: &str,
        message: &str,
        context: Option<&str>,
        ip: Option<&str>,
    ) -> impl Future<Output = Result<()>> + Send;

    /// How many payouts Swish reported `PAID` or `DEBITED` after a person redid them: the
    /// recipient of each may be paid twice. Read on every health request, so it reads the redos
    /// alone, a few among millions of payouts.
    fn paid_twice(&self) -> impl Future<Output = Result<i64>> + Send;

    /// Every stored number, disabled ones included, by name.
    fn payers(&self) -> impl Future<Output = Result<Vec<PayerRecord>>> + Send;

    /// Stores a new number, or reports why not.
    ///
    /// **MUST be atomic**: a name and a number are each stored once, so two concurrent adds of one
    /// name or one number store one. **MUST refuse a number with payouts on record under another
    /// name**: the name is half of the double-payout key, so the same number added under a new
    /// name would pay every reference it already paid again. A number paid under this same name
    /// keeps its guard and is added.
    ///
    /// **MUST write the number and `logged` both or neither**, as every change to a number below:
    /// a number never stands without the line that says who added it, and from where.
    fn add_payer(
        &self,
        new: &NewPayer<'_>,
        logged: &LogLine<'_>,
    ) -> impl Future<Output = Result<AddPayerOutcome>> + Send;

    /// Gives a number a new nickname, reporting whether it exists. Moves `updated_at`. **MUST write
    /// the change and `logged` both or neither.**
    fn set_payer_nickname(
        &self,
        name: &str,
        nickname: &str,
        logged: &LogLine<'_>,
    ) -> impl Future<Output = Result<bool>> + Send;

    /// Disables or enables a number, reporting whether it exists. Moves `updated_at`. **MUST write
    /// the change and `logged` both or neither.**
    fn set_payer_disabled(
        &self,
        name: &str,
        disabled: bool,
        logged: &LogLine<'_>,
    ) -> impl Future<Output = Result<bool>> + Send;

    /// Marks a number changed, as after its certificates were replaced, reporting whether it
    /// exists. Moves `updated_at`, which every running swisha compares to know it must reload.
    /// **MUST write the change and `logged` both or neither.**
    fn touch_payer(&self, name: &str, logged: &LogLine<'_>) -> impl Future<Output = Result<bool>> + Send;

    /// The number whose key hashes to `key_hash`, or `None` when no number's current key does.
    ///
    /// **MUST read storage, never a cache**: a replaced key finds nothing from the moment
    /// [`replace_payer_key`](PayoutStore::replace_payer_key) returns, on every instance.
    fn payer_by_key(&self, key_hash: &str) -> impl Future<Output = Result<Option<KeyHolder>>> + Send;

    /// Of these key hashes, the ones a number's current key still has, each with that number's
    /// name. One question for every event stream open on an instance, rather than one each.
    ///
    /// **MUST read storage, never a cache**, as [`payer_by_key`](PayoutStore::payer_by_key).
    fn key_holders(&self, key_hashes: &[String]) -> impl Future<Output = Result<Vec<(String, String)>>> + Send;

    /// Gives a number a new key, reporting whether it did. With `current`, only while that is still
    /// the number's key.
    ///
    /// **MUST be atomic**: of two replacements made with one current key, exactly one succeeds, so
    /// a key can never be replaced by someone who no longer holds it. **MUST write the change and
    /// `logged` both or neither.**
    fn replace_payer_key(
        &self,
        name: &str,
        current: Option<&str>,
        new: &str,
        logged: &LogLine<'_>,
    ) -> impl Future<Output = Result<bool>> + Send;
}
