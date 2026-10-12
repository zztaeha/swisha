//! What a person may do about a payout, and what they should check before doing it.
//!
//! swisha never pays again on its own. Once its checks end without Swish's last word, or Swish
//! reports a failure, a person decides: confirm the payout as paid, or redo it under a new
//! reference. [`actions`] says which of the two swisha would accept now, and [`review`] what to
//! check first.
//!
//! ```
//! use swisha::domain::payout::StatusView;
//! use swisha::domain::review::{Action, PAID_AFTER_REDO, actions, review};
//!
//! let view = |status: &str, checks_ended: bool, redone: bool| StatusView {
//!     status: Some(status.into()),
//!     amount: 250.5,
//!     swish_ref: None,
//!     error_code: None,
//!     checks_ended,
//!     replaces: None,
//!     replaced_by: redone.then(|| "INV-1042-2".into()),
//!     was_debited: false,
//!     earlier_paid: false,
//!     accepted: true,
//!     send_unaccepted: false,
//!     contradicted: false,
//!     manual_confirmation: None,
//! };
//!
//! // Swish reported it failed: it can be paid again under a new reference, once only.
//! assert_eq!(actions(&view("ERROR", false, false)), [Action::Redo]);
//! assert_eq!(actions(&view("ERROR", false, true)), [], "redone: the redo carries it on");
//!
//! // The money left and Swish never said PAID: once the checks end, a person confirms it.
//! assert_eq!(actions(&view("DEBITED", false, false)), []);
//! assert_eq!(actions(&view("DEBITED", true, false)), [Action::ConfirmPaid]);
//! assert!(review(&view("DEBITED", true, false)).is_some());
//!
//! // Swish paid it after it was redone: the recipient may have the money twice.
//! assert_eq!(review(&view("PAID", false, true)), Some(PAID_AFTER_REDO));
//! ```

use crate::domain::errors::{Language, is_recipient_details};
use crate::domain::payout::StatusView;
use crate::domain::status::{CONFIRMABLE, CONFIRMABLE_AFTER_CHECKS, PayoutStatus, REDOABLE};

/// The event recorded when a person confirms a payout as paid.
pub const CONFIRMED_PAID: &str = "CONFIRMED_PAID";

/// The event recorded on a payout when a person redoes it under a new reference.
pub const REDONE: &str = "REDONE";

/// Something a person may do about a payout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Mark it paid, because the bank or the recipient confirmed the money arrived.
    ConfirmPaid,
    /// Pay it again under a new reference, because the money never left or has come back.
    Redo,
}

impl Action {
    /// The wire form, as the status endpoint lists it: `confirm_paid` or `redo`.
    pub fn as_str(self) -> &'static str {
        match self {
            Action::ConfirmPaid => "confirm_paid",
            Action::Redo => "redo",
        }
    }
}

/// What swisha would accept for a payout now. A redone payout takes neither: the redo carries it
/// on. Nothing is redone once a payout before it was paid after all, nor what Swish declined over
/// the recipient's details: see [`declined_over_details`].
pub fn actions(view: &StatusView) -> Vec<Action> {
    let redone = view.replaced_by.is_some();
    let Some(status) = view.status.as_deref().and_then(PayoutStatus::parse).filter(|_| !redone) else {
        return Vec::new();
    };
    let mut actions = Vec::new();
    if CONFIRMABLE.contains(&status) || (view.checks_ended && CONFIRMABLE_AFTER_CHECKS.contains(&status)) {
        actions.push(Action::ConfirmPaid);
    }
    if REDOABLE.contains(&status) && !view.earlier_paid && !declined_over_details(view) {
        actions.push(Action::Redo);
    }
    actions
}

/// Whether Swish declined the payout over the recipient's own details, one of
/// [`RECIPIENT_DETAILS`](crate::domain::errors::RECIPIENT_DETAILS). A redo would send the same
/// details and meet the same decline, so a person corrects them and sends a new payout instead.
pub fn declined_over_details(view: &StatusView) -> bool {
    let declined = view.status.as_deref() == Some(PayoutStatus::Declined.as_str());
    declined && is_recipient_details(view.error_code.as_deref())
}

/// What a person should check about a payout, in English and Swedish.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Review {
    /// The English text.
    pub english: &'static str,
    /// The Swedish text.
    pub swedish: &'static str,
}

impl Review {
    /// The text in the requested language.
    pub fn message(&self, language: Language) -> &'static str {
        match language {
            Language::English => self.english,
            Language::Swedish => self.swedish,
        }
    }
}

/// The money left and Swish then reported a failure, so it is on its way back.
pub const RETURNED: Review = Review {
    english: "The money left your account, but Swish then reported the payout failed, so it should come back. Check your bank statement before paying it again.",
    swedish: "Pengarna lämnade ditt konto, men Swish rapporterade sedan att utbetalningen misslyckades, så de bör komma tillbaka. Kontrollera kontoutdraget innan du betalar ut den igen.",
};

/// The money left, and Swish never confirmed that the recipient has it.
pub const UNCONFIRMED: Review = Review {
    english: "The money left your account, but Swish never confirmed that the recipient received it. Check with your bank or the recipient, then confirm it as paid.",
    swedish: "Pengarna har lämnat ditt konto, men Swish har aldrig bekräftat att mottagaren fick dem. Kontrollera med banken eller mottagaren och bekräfta sedan utbetalningen som betald.",
};

/// Swish still reports a failure when the checks end.
pub const STILL_FAILED: Review = Review {
    english: "Swish still reports this payout as failed. If the recipient received the money anyway, confirm it as paid; otherwise pay it again.",
    swedish: "Swish rapporterar fortfarande utbetalningen som misslyckad. Bekräfta den som betald om mottagaren ändå fick pengarna, annars betala ut den igen.",
};

/// Swish never answered, and swisha has stopped asking.
pub const NO_OUTCOME: Review = Review {
    english: "Swish never gave an outcome for this payout, and swisha has stopped asking. Check your bank statement or ask the recipient: confirm it as paid if the money arrived, and pay it again only if it never left your account.",
    swedish: "Swish gav aldrig något utfall för utbetalningen, och swisha har slutat fråga. Kontrollera kontoutdraget eller fråga mottagaren: bekräfta utbetalningen som betald om pengarna kom fram, och betala bara ut den igen om de aldrig lämnade kontot.",
};

/// Swish reported the money for a payout gone after it was redone.
pub const PAID_AFTER_REDO: Review = Review {
    english: "Swish reported that this payout's money left your account after it was redone, so the recipient may be paid twice. Check your bank statement, and if both payouts went through, ask the recipient to pay one back.",
    swedish: "Swish rapporterade att pengarna för utbetalningen lämnade ditt konto efter att den gjorts om, så mottagaren kan få betalt två gånger. Kontrollera kontoutdraget, och om båda utbetalningarna gick igenom, be mottagaren betala tillbaka den ena.",
};

/// Swish reported the money for a payout that came before this one gone after all.
pub const EARLIER_PAID: Review = Review {
    english: "Swish reported that the money for a payout that came before this one left your account after all, so the recipient may be paid twice. Check your bank statement, and if both payouts went through, ask the recipient to pay one back.",
    swedish: "Swish rapporterade att pengarna för en utbetalning som kom före den här ändå lämnade ditt konto, så mottagaren kan få betalt två gånger. Kontrollera kontoutdraget, och om båda utbetalningarna gick igenom, be mottagaren betala tillbaka den ena.",
};

/// Swish declined a payout over the recipient's details, and would decline it again as it stands.
pub const DETAILS: Review = Review {
    english: "Swish declined this payout over the recipient's details, and would decline it again as it stands. Correct the details, then send a new payout under a new reference.",
    swedish: "Swish nekade utbetalningen på grund av mottagarens uppgifter och skulle neka den igen som den är. Rätta uppgifterna och skicka sedan en ny utbetalning med en ny referens.",
};

/// Swish reported a payout failed after a person confirmed it paid.
pub const CONTRADICTED: Review = Review {
    english: "Swish reported this payout failed after it was confirmed as paid. Check with your bank or the recipient whether the money arrived, and if it did not, send it again as a new payout under a new reference.",
    swedish: "Swish rapporterade att utbetalningen misslyckades efter att den bekräftats som betald. Kontrollera med banken eller mottagaren om pengarna kom fram, och om de inte gjorde det, skicka den igen som en ny utbetalning med en ny referens.",
};

/// What a person should check about a payout, or `None` when nothing needs them.
///
/// A payout Swish paid after it was redone always does, and so does every redo after it: the
/// recipient may be paid twice. So does one Swish reported failed after a person confirmed it paid,
/// a decline over the recipient's details, which needs a new payout, and a failure after `DEBITED`,
/// since money that left is on its way back. Otherwise a person is needed once swisha has stopped
/// asking: at `NEEDS_REVIEW`, or at `DEBITED` or `ERROR` once the checks have ended. Any other
/// redone payout needs nobody: the redo carries it on.
pub fn review(view: &StatusView) -> Option<Review> {
    let status = view.status.as_deref().unwrap_or_default();
    if view.earlier_paid {
        return Some(EARLIER_PAID);
    }
    if view.contradicted {
        return Some(CONTRADICTED);
    }
    if view.replaced_by.is_some() {
        let paid = PayoutStatus::parse(status).is_some_and(PayoutStatus::is_terminal);
        return paid.then_some(PAID_AFTER_REDO);
    }
    match status {
        "DECLINED" if declined_over_details(view) => Some(DETAILS),
        "ERROR" if view.was_debited => Some(RETURNED),
        "ERROR" if view.checks_ended => Some(STILL_FAILED),
        "DEBITED" if view.checks_ended => Some(UNCONFIRMED),
        "NEEDS_REVIEW" => Some(NO_OUTCOME),
        _ => None,
    }
}
