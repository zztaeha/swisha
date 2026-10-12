//! The payout lifecycle, and the sets that guard it.
//!
//! The sets below are the only place status membership is decided. Every SQL guard is built from
//! them through [`sql_list`], so a guard cannot drift from the status it is meant to protect.
//!
//! ```
//! use swisha::domain::status::{PayoutStatus, UNCONFIRMED, writable_condition};
//!
//! let status = PayoutStatus::parse("DEBITED").expect("a status Swish sends");
//! assert!(status.is_terminal(), "the money has left: nothing moves it back");
//! assert!(UNCONFIRMED.contains(&status), "the sweep asks Swish until it says PAID or ERROR");
//! assert_eq!(PayoutStatus::parse("debited"), None, "case sensitive, as Swish is");
//!
//! // The guard every status write runs behind, rendered for a WHERE clause.
//! let guard = writable_condition("$1");
//! assert!(guard.contains("status NOT IN ('PAID', 'DEBITED')"));
//! ```

use PayoutStatus::{Created, Debited, Declined, Error, NeedsReview, Paid, Pending};

/// Where a payout is in its lifecycle.
///
/// Swish sends these uppercase, and [`parse`](PayoutStatus::parse) is case sensitive to match.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PayoutStatus {
    /// Claimed in the database, and sent or about to be. Swish may have taken it without an outcome
    /// yet: the sweep asks.
    Created,
    /// No answer yet: Swish holds the instruction and has not resolved it, or swisha could not
    /// tell whether the instruction reached Swish. The sweep asks.
    Pending,
    /// The money has left the merchant account. Only Swish's `PAID` or `ERROR` follows it.
    Debited,
    /// Swish confirmed the recipient was paid. Final.
    Paid,
    /// Swish refused the payout. No money moved.
    Declined,
    /// Swish reported the payout failed. After `DEBITED`, the money is on its way back.
    Error,
    /// swisha could not get an answer from Swish and has stopped asking. Not terminal: a genuine
    /// late answer from Swish still settles it. A person decides whether to pay again.
    NeedsReview,
}

impl PayoutStatus {
    /// The wire form, as Swish writes it and as the database stores it.
    pub const fn as_str(self) -> &'static str {
        match self {
            Created => "CREATED",
            Pending => "PENDING",
            Debited => "DEBITED",
            Paid => "PAID",
            Declined => "DECLINED",
            Error => "ERROR",
            NeedsReview => "NEEDS_REVIEW",
        }
    }

    /// Parses the wire form. Case sensitive, because Swish always sends uppercase and a
    /// lowercase value means something upstream is not what it claims to be.
    pub fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "CREATED" => Created,
            "PENDING" => Pending,
            "DEBITED" => Debited,
            "PAID" => Paid,
            "DECLINED" => Declined,
            "ERROR" => Error,
            "NEEDS_REVIEW" => NeedsReview,
            _ => return None,
        })
    }

    /// Whether this status is in [`TERMINAL`], and so only [`TERMINAL_ADVANCE`] moves it.
    pub fn is_terminal(self) -> bool {
        TERMINAL.contains(&self)
    }
}

impl std::fmt::Display for PayoutStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The money has left: never overwritten, except by the moves in [`TERMINAL_ADVANCE`].
///
/// A late, duplicated or out-of-order callback must not move a payout back into flight.
/// [`PayoutStatus::NeedsReview`] is deliberately absent: swisha has stopped chasing the payout,
/// but a genuine late answer from Swish should still be allowed to settle it.
pub const TERMINAL: &[PayoutStatus] = &[Paid, Debited];

/// Statuses Swish has not confirmed as `PAID`. The sweep keeps asking Swish about these, reading
/// only, until it does or the checks end.
///
/// `ERROR` is among them, so a failure that later goes through is still recorded. `DECLINED` is
/// not: Swish refused it, no money moved, and there is nothing more to learn.
pub const UNCONFIRMED: &[PayoutStatus] = &[Created, Pending, Error, Debited];

/// Every status Swish answers with about a payout. Anything else it reports, such as `CREATED` or
/// `INITIATED`, is no answer yet.
pub const ANSWERS: &[PayoutStatus] = &[Paid, Debited, Declined, Error];

/// What swisha records when Swish has given no answer: `PENDING` while it keeps asking, and
/// `NEEDS_REVIEW` once it has stopped. Neither is Swish's word, so neither replaces an answer.
pub const NO_ANSWER: &[PayoutStatus] = &[Pending, NeedsReview];

/// The only statuses [`NO_ANSWER`] may replace. Over an answer (`DEBITED`, `ERROR`, `DECLINED`)
/// swisha's own "no answer" would hide what Swish said.
pub const UNANSWERED: &[PayoutStatus] = &[Created, Pending];

/// What Swish reports only about an instruction it holds: it accepted it, whatever came next.
pub const HELD: &[PayoutStatus] = &[Debited, Paid, Error];

/// What a person may confirm as paid at any time: swisha has stopped asking, with no answer.
pub const CONFIRMABLE: &[PayoutStatus] = &[NeedsReview];

/// What a person may confirm as paid once the checks have ended, and not before: until then Swish
/// may still give its last word.
pub const CONFIRMABLE_AFTER_CHECKS: &[PayoutStatus] = &[Debited, Error];

/// What a person may redo under a new reference: Swish refused it or reported it failed, or never
/// answered. Never `DEBITED` or `PAID`, whose money has left, nor `CREATED` or `PENDING`, which
/// Swish may still pay.
pub const REDOABLE: &[PayoutStatus] = &[Error, Declined, NeedsReview];

/// The only moves out of a terminal status: `DEBITED` to `PAID` or to `ERROR`.
///
/// Swish reports a successful payout twice, as two callbacks seconds apart: `DEBITED` when the
/// money leaves the merchant account, then `PAID` when the recipient has it. A late failure is
/// `DEBITED` then `ERROR`: the recipient's bank refused the money and Swish returns it. Through
/// this guard only Swish's answers make either move: when swisha cannot tell what happened it
/// records `PENDING`, which never replaces `DEBITED`. A person's confirmation has its own guard,
/// [`CONFIRMABLE_AFTER_CHECKS`].
///
/// Nothing moves out of `PAID`. The reverse of either move is a regression rather than a later
/// answer, such as a delayed `DEBITED` callback arriving after the `ERROR`, so it stays refused.
pub const TERMINAL_ADVANCE: (PayoutStatus, &[PayoutStatus]) = (Debited, &[Paid, Error]);

/// The SQL condition for "this row's status may be overwritten", for a `WHERE` clause: never a
/// [`TERMINAL`] status except by [`TERMINAL_ADVANCE`], never the reverse of one, and
/// [`NO_ANSWER`] only over [`UNANSWERED`].
///
/// `new_status` names the bind parameter carrying the incoming status, `$1` on PostgreSQL. Every
/// guard is built from this and [`sql_list`], so the sets above stay the single source of truth.
pub fn writable_condition(new_status: &str) -> String {
    let (from, to) = TERMINAL_ADVANCE;
    let (from, to) = (from.as_str(), sql_list(to));
    format!(
        "(status NOT IN ({terminal}) OR (status = '{from}' AND {new_status} IN ({to}))) \
         AND ({new_status} <> '{from}' OR status NOT IN ({to})) \
         AND ({new_status} NOT IN ({no_answer}) OR status IN ({unanswered}))",
        terminal = sql_list(TERMINAL),
        no_answer = sql_list(NO_ANSWER),
        unanswered = sql_list(UNANSWERED),
    )
}

/// Renders a set for a SQL `IN` clause, as `'PAID', 'DEBITED'`.
///
/// Statement text is built from this and nothing else, so the sets above are the single source
/// of truth for every guard in the codebase.
pub fn sql_list(statuses: &[PayoutStatus]) -> String {
    statuses
        .iter()
        .map(|status| format!("'{}'", status.as_str()))
        .collect::<Vec<_>>()
        .join(", ")
}
