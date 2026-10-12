//! Normalizing and checking what a payout carries: its reference, its two identifiers and its
//! amount.
//!
//! Every function is pure and clock-independent: a personnummer is 12 digits or it is not,
//! with no century inference from today's date. The payout endpoint runs a request through these
//! before anything is stored, because anything Swish refuses would spend the reference.
//!
//! ```
//! use swisha::domain::validate::*;
//!
//! let payee = normalize_phone("070-123 45 67");
//! assert_eq!(payee, "46701234567");
//! assert!(validate_phone(&payee));
//!
//! let ssn = normalize_ssn("19640823-3234");
//! assert!(personnummer_luhn_valid(&ssn));
//!
//! assert!(is_valid_reference("INV-1042"));
//! assert!(is_whole_ore(250.50) && !is_whole_ore(1.005));
//! assert!(is_valid_message("Faktura 1042, tack!") && !is_valid_message("Faktura #1042"));
//! ```

/// Strips everything but digits and renders a Swedish number in E.164 form, `46XXXXXXXXX`: the
/// international prefix `00` before `46` is dropped, a leading `0` becomes `46`, and the trunk `0`
/// Swedes write after `46`, as in `+46 (0)70`, is dropped.
pub fn normalize_phone(raw: &str) -> String {
    let digits: String = raw.chars().filter(|c| c.is_ascii_digit()).collect();
    let digits = match digits.strip_prefix("00") {
        Some(rest) if rest.starts_with("46") => rest.to_string(),
        _ => digits,
    };
    if let Some(rest) = digits.strip_prefix('0') {
        format!("46{rest}")
    } else if let Some(rest) = digits.strip_prefix("460") {
        format!("46{rest}")
    } else {
        digits
    }
}

/// Whether a number that has been through [`normalize_phone`] is a plausible Swedish one: `46`
/// followed by a 9 or 10 digit subscriber number, which never starts with `0`.
pub fn validate_phone(normalized: &str) -> bool {
    matches!(normalized.len(), 11 | 12)
        && normalized.starts_with("46")
        && !normalized.starts_with("460")
        && normalized.bytes().all(|b| b.is_ascii_digit())
}

/// Whether an amount in SEK is a whole number of öre, the most precision Swish takes.
///
/// More would be rounded twice, once into the payload and once by the database, and the two
/// roundings differ: `1.005` is sent as `1.00` and stored as `1.01`. Binary floating point stores
/// `99.9` as `99.900000000000006`, so that sliver of drift is allowed, but nothing a person could
/// have typed: `100.0000001` is refused.
pub fn is_whole_ore(amount: f64) -> bool {
    let ore = amount * 100.0;
    ore.is_finite() && (ore - ore.round()).abs() < 1e-6
}

/// Whether a reference is one Swish accepts as `payerPaymentReference`: 1 to 35 of `A-Z`,
/// `a-z`, `0-9` and `-`. Swish refuses anything else with `FF08`, which would spend the
/// reference.
///
/// ```
/// use swisha::domain::validate::is_valid_reference;
///
/// assert!(is_valid_reference("INV-1042"));
/// assert!(!is_valid_reference("INV_1042"), "Swish refuses _ with FF08");
/// assert!(!is_valid_reference(&"A".repeat(36)));
/// ```
pub fn is_valid_reference(reference: &str) -> bool {
    (1..=35).contains(&reference.len())
        && reference.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// The message as Swish shows it to the recipient: its first 50 characters, Swish's limit.
///
/// Counted in characters, not bytes, so `å` costs one.
///
/// ```
/// let message = swisha::domain::validate::swish_message(&"å".repeat(60));
/// assert_eq!(message.chars().count(), 50);
/// ```
pub fn swish_message(text: &str) -> String {
    text.chars().take(50).collect()
}

/// The characters a payout message may hold, as error messages name them.
pub const MESSAGE_CHARACTERS: &str = "A-Z, a-z, Å, Ä, Ö, å, ä, ö, 0-9, space and !?(),.-:;";

/// Whether every character of a message is one Swish takes, as [`MESSAGE_CHARACTERS`] lists.
/// Swish refuses anything else with `RP02`, which would spend the reference. Empty is valid.
///
/// The set is Swish's documented one, `a-ö`, `A-Ö`, `0-9` and `!?(),.-:;`, plus the space its own
/// examples use. `a-ö` is the Swedish alphabet, so `é` and `ü` are out; the simulator refused
/// every character tested outside the set.
///
/// ```
/// use swisha::domain::validate::is_valid_message;
///
/// assert!(is_valid_message("Utbetalning: 1042 (klar), tack!"));
/// assert!(is_valid_message("Åsa Öberg"));
/// assert!(!is_valid_message("Café"), "é is outside the Swedish alphabet");
/// assert!(!is_valid_message("Faktura #1042"));
/// ```
pub fn is_valid_message(text: &str) -> bool {
    text.chars().all(|c| c.is_ascii_alphanumeric() || "ÅÄÖåäö !?(),.-:;".contains(c))
}

/// Strips separators from a personnummer, leaving digits only, and nothing else: the century is
/// part of the identity, so a caller supplies it. Guessing it from a 10-digit number would send
/// Swish a real personnummer belonging to someone else.
pub fn normalize_ssn(raw: &str) -> String {
    raw.chars().filter(|c| c.is_ascii_digit()).collect()
}

/// Whether a personnummer is one Swish takes: exactly 12 digits, `YYYYMMDDNNNN`, whose date
/// exists and whose Luhn digit checks out.
///
/// Swish refuses anything else with `PA06`, which would spend the reference: against the
/// simulator that included month 13, day 32 and coordination numbers, whose day is the birth day
/// plus 60. Clock-independent, so a date in the future is left to Swish.
///
/// ```
/// use swisha::domain::validate::is_valid_personnummer;
///
/// assert!(is_valid_personnummer("196408233234"));
/// assert!(!is_valid_personnummer("196413233237"), "month 13, though its Luhn digit is right");
/// assert!(!is_valid_personnummer("6408233234"), "the century is never guessed");
/// ```
pub fn is_valid_personnummer(digits: &str) -> bool {
    digits.len() == 12
        && digits.bytes().all(|b| b.is_ascii_digit())
        && crate::util::time::is_valid_date(&format!("{}-{}-{}", &digits[..4], &digits[4..6], &digits[6..8]))
        && personnummer_luhn_valid(digits)
}

/// Whether 12 digits pass the Luhn check a Swedish personnummer carries, over its 10 rightmost
/// digits, `YYMMDDNNNN`. The date is not checked here: see [`is_valid_personnummer`].
///
/// Expects exactly 12 digits, `YYYYMMDDNNNN`, as produced by [`normalize_ssn`]; anything else is
/// `false`, separators included, so a caller can never pass Swish a malformed one. Ten-digit
/// forms are not accepted anywhere: the century is ambiguous, and guessing it is how a payout
/// reaches the wrong person.
///
/// ```
/// use swisha::domain::validate::personnummer_luhn_valid;
///
/// assert!(personnummer_luhn_valid("196408233234"));
/// assert!(!personnummer_luhn_valid("196408233235"), "wrong check digit");
/// assert!(!personnummer_luhn_valid("6408233234"), "the century is not guessed");
/// ```
pub fn personnummer_luhn_valid(digits12: &str) -> bool {
    if digits12.len() != 12 || !digits12.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    let d: Vec<u32> = digits12.chars().skip(2).filter_map(|c| c.to_digit(10)).collect();
    let sum: u32 = d.iter().enumerate().map(|(i, &v)| {
        let m = if i % 2 == 0 { v * 2 } else { v };
        if m > 9 { m - 9 } else { m }
    }).sum();
    sum.is_multiple_of(10)
}

/// Whether text a person typed is fit to keep in the audit trail: not blank, at most `max`
/// characters once trimmed, and nothing that could forge a line in a log or on a screen or hide
/// what it says. That is no control character, no line or paragraph separator, and no format
/// character, such as a bidi override or a zero-width space.
///
/// ```
/// use swisha::domain::validate::is_plain_text;
///
/// assert!(is_plain_text("Anna Ö, kassa 2", 100));
/// assert!(!is_plain_text("   ", 100), "blank");
/// assert!(!is_plain_text("Anna\nCONFIRMED", 100), "a line break forges a line");
/// assert!(!is_plain_text("Anna\u{2028}CONFIRMED", 100), "so does a line separator");
/// assert!(!is_plain_text("Anna\u{202E}gnaB", 100), "a bidi override hides what it says");
/// assert!(!is_plain_text(&"a".repeat(101), 100));
/// ```
pub fn is_plain_text(text: &str, max: usize) -> bool {
    let text = text.trim();
    !text.is_empty() && text.chars().count() <= max && !text.chars().any(|c| c.is_control() || is_invisible(c))
}

// Unicode's format characters (Cf) and its line and paragraph separators (Zl, Zp), as of Unicode
// 16.0. Rust's std has no general category lookup, so the ranges are listed.
fn is_invisible(c: char) -> bool {
    matches!(
        c,
        '\u{AD}'
            | '\u{600}'..='\u{605}'
            | '\u{61C}'
            | '\u{6DD}'
            | '\u{70F}'
            | '\u{890}'..='\u{891}'
            | '\u{8E2}'
            | '\u{180E}'
            | '\u{200B}'..='\u{200F}'
            | '\u{2028}'..='\u{202E}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{206F}'
            | '\u{FEFF}'
            | '\u{FFF9}'..='\u{FFFB}'
            | '\u{110BD}'
            | '\u{110CD}'
            | '\u{13430}'..='\u{1343F}'
            | '\u{1BCA0}'..='\u{1BCA3}'
            | '\u{1D173}'..='\u{1D17A}'
            | '\u{E0001}'
            | '\u{E0020}'..='\u{E007F}'
    )
}

/// Whether a payer name is one swisha takes: 1 to 32 of `a-z`, `0-9`, `-` and `_`.
///
/// The name travels in request bodies, URL paths, channel names and the certificate folder, so it
/// is kept to characters that mean the same thing in all four. It can never name another path.
///
/// ```
/// use swisha::domain::validate::is_valid_payer_name;
///
/// assert!(is_valid_payer_name("sthlm") && is_valid_payer_name("city_9"));
/// assert!(!is_valid_payer_name("Sthlm") && !is_valid_payer_name("../sthlm"));
/// assert!(!is_valid_payer_name(&"a".repeat(33)));
/// ```
pub fn is_valid_payer_name(name: &str) -> bool {
    (1..=32).contains(&name.len())
        && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

/// Whether a Swish number is one Swish takes as `payerAlias`: 10 or 11 digits.
///
/// ```
/// use swisha::domain::validate::is_valid_swish_number;
///
/// assert!(is_valid_swish_number("1234679304"));
/// assert!(!is_valid_swish_number("123 467 93 04") && !is_valid_swish_number("123467930"));
/// ```
pub fn is_valid_swish_number(number: &str) -> bool {
    (10..=11).contains(&number.len()) && number.bytes().all(|b| b.is_ascii_digit())
}

/// The most characters a person's name, `by`, may hold.
pub const MAX_BY: usize = 100;

/// The most characters a person's `note` may hold.
pub const MAX_NOTE: usize = 500;

/// The most characters a number's nickname may hold.
pub const MAX_NICKNAME: usize = 100;

/// `value` trimmed, when it is fit to keep, as [`is_plain_text`] decides.
///
/// # Errors
///
/// The message a caller is shown, naming `field`, when the value is blank, longer than `max`
/// characters, or holds a control or invisible character.
///
/// ```
/// use swisha::domain::validate::{MAX_BY, plain_text};
///
/// assert_eq!(plain_text("  Anna ", "by", MAX_BY), Ok("Anna".to_string()));
/// assert_eq!(
///     plain_text("", "by", MAX_BY),
///     Err("by must be 1 to 100 characters, with no control or invisible characters.".to_string())
/// );
/// ```
pub fn plain_text(value: &str, field: &str, max: usize) -> Result<String, String> {
    if is_plain_text(value, max) {
        Ok(value.trim().to_string())
    } else {
        Err(format!("{field} must be 1 to {max} characters, with no control or invisible characters."))
    }
}
