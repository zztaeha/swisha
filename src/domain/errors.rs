//! Swish error codes, translated into something a person can act on.
//!
//! Swish returns a bare code and little else. [`TABLE`] maps all 27 of them to a plain message
//! in English or Swedish, plus a [`Category`] saying who can fix it, which is the part the
//! official documentation leaves out.
//!
//! ```
//! use swisha::domain::errors::{Category, Language, describe};
//!
//! let info = describe(Some("ACMT07"));
//! assert_eq!(info.category, Category::UserFixable);
//! assert_eq!(info.message(Language::English),
//!            "The recipient is not enrolled in Swish. Check the Swish number.");
//! ```

/// Who can do something about a failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    /// A transient failure that could succeed if tried again.
    ///
    /// swisha never acts on this by itself. It is a hint for whoever decides whether to redo the
    /// payout, which asks Swish about it first.
    Retryable,
    /// The recipient or the submitted details are wrong, and a person can correct them.
    UserFixable,
    /// The merchant's own setup or credentials are wrong. Retrying will never help.
    ContactSupport,
}

impl Category {
    /// The wire form, as it appears in API responses and events: `retryable`, `user_fixable`
    /// or `contact_support`.
    pub fn as_str(self) -> &'static str {
        match self {
            Category::Retryable => "retryable",
            Category::UserFixable => "user_fixable",
            Category::ContactSupport => "contact_support",
        }
    }
}

/// Which language error messages are rendered in, set by `SWISH_ERROR_LANG`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Language {
    /// The default.
    #[default]
    English,
    /// Swedish, for showing a message straight to a Swedish recipient.
    Swedish,
}

impl Language {
    /// Reads a language setting, accepting `sv`, `se`, `sv-se`, `swedish` or `svenska` for
    /// Swedish. Anything else, including a blank value, is English.
    pub fn parse(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "sv" | "se" | "sv-se" | "swedish" | "svenska" => Language::Swedish,
            _ => Language::English,
        }
    }
}

/// What one Swish error code means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ErrorInfo {
    /// Who can fix it.
    pub category: Category,
    /// The English message.
    pub english: &'static str,
    /// The Swedish message.
    pub swedish: &'static str,
}

impl ErrorInfo {
    /// The message in the requested language.
    pub fn message(&self, language: Language) -> &'static str {
        match language {
            Language::English => self.english,
            Language::Swedish => self.swedish,
        }
    }
}

/// The fallback for a code that is not in [`TABLE`], so a code Swish adds later still produces
/// a usable message rather than an empty one.
pub const UNKNOWN: ErrorInfo = ErrorInfo {
    category: Category::ContactSupport,
    english: "The Swish payout failed.",
    swedish: "Swish-utbetalningen misslyckades.",
};

/// The description for a payout swisha stopped asking about without an answer from Swish: every
/// `NEEDS_REVIEW`, whatever code its trail holds.
///
/// Never "failed" or "try again": Swish may hold the instruction, and a person who reads either and
/// pays again under a new reference can pay twice.
pub const UNKNOWN_OUTCOME: ErrorInfo = ErrorInfo {
    category: Category::ContactSupport,
    english: "The outcome is not known: Swish may have received this payout. Check with Swish before paying it again.",
    swedish: "Utfallet är okänt: Swish kan ha tagit emot utbetalningen. Kontrollera med Swish innan den betalas igen.",
};

use Category::{ContactSupport, Retryable, UserFixable};

/// Every Swish error response swisha recognizes, 27 of them.
///
/// The lettered codes are Swish's own. They arrive in the body of a refused submission, and in a
/// callback or a poll answer once an accepted payout fails. The numeric codes are HTTP statuses,
/// used when a refusal names no code. Looked up case insensitively by [`describe`].
pub const TABLE: &[(&str, ErrorInfo)] = &[
    ("400", ErrorInfo {
        category: ContactSupport,
        english: "The payout request was malformed. Contact support.",
        swedish: "Utbetalningsförfrågan är felaktigt utformad. Kontakta support.",
    }),
    ("401", ErrorInfo {
        category: ContactSupport,
        english: "Certificate authentication failed, or the Swish number in the certificate is not enrolled. Contact support.",
        swedish: "Autentiseringen med certifikatet misslyckades, eller så är Swish-numret i certifikatet inte anslutet. Kontakta support.",
    }),
    ("403", ErrorInfo {
        category: ContactSupport,
        english: "The sending Swish number does not match the merchant's Swish number. Contact support.",
        swedish: "Avsändande Swish-nummer matchar inte handlarens Swish-nummer. Kontakta support.",
    }),
    ("415", ErrorInfo {
        category: ContactSupport,
        english: "The Content-Type header must be application/json. Contact support.",
        swedish: "Content-Type måste vara application/json. Kontakta support.",
    }),
    ("422", ErrorInfo {
        category: UserFixable,
        english: "The payout data contains errors. Check the details and try again.",
        swedish: "Utbetalningsdata innehåller fel. Kontrollera uppgifterna och försök igen.",
    }),
    ("429", ErrorInfo {
        category: Retryable,
        english: "Too many attempts. Wait a moment and try again.",
        swedish: "För många försök. Vänta en stund och försök igen.",
    }),
    ("500", ErrorInfo {
        category: Retryable,
        english: "Unknown error at Swish. Try again shortly.",
        swedish: "Okänt fel hos Swish. Försök igen om en stund.",
    }),
    ("ACMT03", ErrorInfo {
        category: ContactSupport,
        english: "Your Swish number is not enrolled for payouts. Contact support.",
        swedish: "Ditt Swish-nummer är inte anslutet för utbetalningar. Kontakta support.",
    }),
    ("ACMT07", ErrorInfo {
        category: UserFixable,
        english: "The recipient is not enrolled in Swish. Check the Swish number.",
        swedish: "Mottagaren är inte ansluten till Swish. Kontrollera Swish-numret.",
    }),
    ("ACMT17", ErrorInfo {
        category: UserFixable,
        english: "The Swish number is not valid. Check that the number is correct.",
        swedish: "Swish-numret är ogiltigt. Kontrollera att numret är korrekt.",
    }),
    ("AM02", ErrorInfo {
        category: UserFixable,
        english: "The amount is above the highest allowed limit.",
        swedish: "Beloppet överstiger den högsta tillåtna gränsen.",
    }),
    ("AM06", ErrorInfo {
        category: UserFixable,
        english: "The amount is below the lowest allowed limit.",
        swedish: "Beloppet understiger den lägsta tillåtna gränsen.",
    }),
    ("CD01", ErrorInfo {
        category: UserFixable,
        english: "The amount exceeds a limit on the receiving account. The recipient should check with their bank.",
        swedish: "Beloppet överskrider en gräns på mottagarkontot. Mottagaren bör kontrollera med sin bank.",
    }),
    ("FF08", ErrorInfo {
        category: ContactSupport,
        english: "The payment reference is invalid. Contact support.",
        swedish: "Betalningsreferensen är ogiltig. Kontakta support.",
    }),
    ("FF10", ErrorInfo {
        category: Retryable,
        english: "The bank's system could not process the payout. Try again.",
        swedish: "Bankens system kunde inte behandla utbetalningen. Försök igen.",
    }),
    ("PA01", ErrorInfo {
        category: ContactSupport,
        english: "A parameter in the request is invalid. Contact support.",
        swedish: "En parameter i förfrågan är felaktig. Kontakta support.",
    }),
    ("PA02", ErrorInfo {
        category: ContactSupport,
        english: "The amount is missing or is not a valid number. Contact support.",
        swedish: "Beloppet saknas eller är inte ett giltigt tal. Kontakta support.",
    }),
    ("PA06", ErrorInfo {
        category: UserFixable,
        english: "The personal identity number has the wrong format. Check the details.",
        swedish: "Personnumret har fel format. Kontrollera uppgifterna.",
    }),
    ("RF07", ErrorInfo {
        category: UserFixable,
        english: "The payout was declined. Your Swish limit may be exceeded, or the account may have insufficient funds. Check with your bank.",
        swedish: "Utbetalningen nekades. Din Swish-gräns kan vara överskriden, eller så saknas täckning på kontot. Kontrollera med din bank.",
    }),
    ("RP01", ErrorInfo {
        category: ContactSupport,
        english: "The merchant's Swish number is missing. Contact support.",
        swedish: "Handlarens Swish-nummer saknas. Kontakta support.",
    }),
    ("RP02", ErrorInfo {
        category: ContactSupport,
        english: "The message is incorrectly formatted. Contact support.",
        swedish: "Meddelandet är felaktigt formaterat. Kontakta support.",
    }),
    ("RP03", ErrorInfo {
        category: ContactSupport,
        english: "The callback URL is missing or does not use HTTPS. Contact support.",
        swedish: "Callback-URL saknas eller använder inte HTTPS. Kontakta support.",
    }),
    ("RP09", ErrorInfo {
        category: Retryable,
        english: "The given instructionUUID is not available. Try again.",
        swedish: "Det angivna instructionUUID är inte tillgängligt. Försök igen.",
    }),
    ("RR04", ErrorInfo {
        category: ContactSupport,
        english: "The payout was declined for regulatory reasons. Contact the bank.",
        swedish: "Utbetalningen nekades av regulatoriska skäl. Kontakta banken.",
    }),
    ("TA01", ErrorInfo {
        category: Retryable,
        english: "Temporary technical error at Swish. Check whether the payout went through before sending it again.",
        swedish: "Tillfälligt tekniskt fel hos Swish. Kontrollera om utbetalningen gick igenom innan du skickar den igen.",
    }),
    ("TM01", ErrorInfo {
        category: Retryable,
        english: "Swish timed out before the payout was started. Confirm it did not go through before sending it again.",
        swedish: "Tidsgränsen överskreds innan utbetalningen startade. Bekräfta att den inte gick igenom innan du skickar den igen.",
    }),
    ("VR02", ErrorInfo {
        category: UserFixable,
        english: "The Swish number does not match the given personal identity number. Check the details.",
        swedish: "Swish-numret matchar inte det angivna personnumret. Kontrollera uppgifterna.",
    }),
];

/// The codes Swish declines a payout with over the recipient's own details: a number that is not
/// in Swish or not theirs, a malformed personnummer, or an amount outside their limits. Sent again
/// unchanged, as a redo sends it, the payout meets the same decline.
pub const RECIPIENT_DETAILS: &[&str] = &["ACMT07", "ACMT17", "PA06", "VR02", "AM02", "AM06"];

/// Whether `code` is one of [`RECIPIENT_DETAILS`], ignoring case and surrounding space.
///
/// ```
/// use swisha::domain::errors::is_recipient_details;
///
/// assert!(is_recipient_details(Some("vr02")));
/// assert!(!is_recipient_details(Some("RF07")), "the payer's own limit, which a resend can pass");
/// assert!(!is_recipient_details(None));
/// ```
pub fn is_recipient_details(code: Option<&str>) -> bool {
    code.is_some_and(|code| RECIPIENT_DETAILS.iter().any(|known| known.eq_ignore_ascii_case(code.trim())))
}

/// The description for a payout's current state, or `None` unless it has failed or swisha has
/// stopped asking about it.
///
/// A failed payout carries a description even when Swish supplied no code, so a caller never
/// has to explain a failure with a blank field. A settled one carries none whatever the trail
/// holds: a code from a refusal Swish later paid through is not advice to pay again. A
/// `NEEDS_REVIEW` is always [`UNKNOWN_OUTCOME`], since Swish may hold it: a code from its send,
/// such as `500` or `RP09`, would read as advice to try again.
///
/// ```
/// use swisha::domain::errors::{UNKNOWN, UNKNOWN_OUTCOME, describe, describe_failure};
///
/// assert_eq!(describe_failure("DECLINED", Some("RF07")), Some(describe(Some("RF07"))));
/// assert_eq!(describe_failure("ERROR", None), Some(UNKNOWN), "Swish said it failed");
/// assert_eq!(describe_failure("NEEDS_REVIEW", None), Some(UNKNOWN_OUTCOME), "Swish may hold it");
/// assert_eq!(describe_failure("PAID", Some("TA01")), None, "an old code is not advice to pay again");
/// ```
pub fn describe_failure(status: &str, code: Option<&str>) -> Option<ErrorInfo> {
    match status {
        "DECLINED" | "ERROR" => Some(describe(code)),
        "NEEDS_REVIEW" => Some(UNKNOWN_OUTCOME),
        _ => None,
    }
}

/// Looks up one error code, falling back to [`UNKNOWN`] rather than returning nothing.
pub fn describe(code: Option<&str>) -> ErrorInfo {
    let code = code.unwrap_or("").trim();
    if code.is_empty() {
        return UNKNOWN;
    }
    TABLE
        .iter()
        .find(|(known, _)| known.eq_ignore_ascii_case(code))
        .map(|(_, info)| *info)
        .unwrap_or(UNKNOWN)
}
