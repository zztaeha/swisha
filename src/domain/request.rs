//! What a caller sends: a payout, a person's action on one, and a Swish number to pay from.

use serde::Deserialize;

/// A payout request.
///
/// swisha's model is Swish's model: these fields are all the payout instruction takes from
/// outside. Anything else a consumer keeps alongside a payout, such as an invoice or an order,
/// lives in their own storage keyed by the same reference.
///
/// Unknown fields are rejected, which is what stops an outdated caller having fields silently
/// dropped and a payout going out built from whatever happened to match.
///
/// ```
/// use swisha::domain::request::PayoutRequest;
///
/// let request: PayoutRequest = serde_json::from_str(r#"{
///     "payer": "sthlm",
///     "reference": "INV-1042",
///     "payee_alias": "0701234567",
///     "payee_ssn": "196408233234",
///     "amount": 250.00
/// }"#)?;
/// assert_eq!(request.reference, "INV-1042");
///
/// let outdated = r#"{"reference":"INV-1","payee_alias":"0701234567","amount":1,"tip":5}"#;
/// assert!(serde_json::from_str::<PayoutRequest>(outdated).is_err(), "unknown fields are refused");
/// # Ok::<(), serde_json::Error>(())
/// ```
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PayoutRequest {
    /// Which number pays, by its name. Optional, since the request's key already names it; when
    /// given, it must be the key's number.
    pub payer: Option<String>,
    /// The caller's own identifier, and with the payer the idempotency key. Echoed to Swish as
    /// `payerPaymentReference` and never interpreted. 1 to 35 of the characters Swish accepts.
    pub reference: String,
    /// The recipient's Swish number, in any format. Normalized to `46XXXXXXXXX`.
    pub payee_alias: String,
    /// The recipient's personnummer, 12 digits. Required: Swish verifies every payout against
    /// it. Optional in the type only so that leaving it out gets a 400 that says so.
    pub payee_ssn: Option<String>,
    /// Amount in SEK, at most two decimals. At least 1, at most `SWISH_MAX_PAYOUT`.
    pub amount: f64,
    /// What the recipient reads in their Swish app. Falls back to the `SWISH_PAYOUT_MESSAGE`
    /// template. Cut to Swish's 50 characters before it is stored, and refused when it holds a
    /// character Swish does not take: see
    /// [`is_valid_message`](crate::domain::validate::is_valid_message).
    pub message: Option<String>,
}

// Debug output leaves out the recipient: a log line or a panic message must never carry a
// personnummer or a phone number.
impl std::fmt::Debug for PayoutRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PayoutRequest")
            .field("payer", &self.payer)
            .field("reference", &self.reference)
            .field("amount", &self.amount)
            .field("message", &self.message)
            .finish_non_exhaustive()
    }
}

/// A person's word that a payout was paid, for `POST /swish/payout/{payer}/{reference}/confirm`.
///
/// Both fields are required, so a confirmation always says who made it and on what grounds.
///
/// ```
/// use swisha::domain::request::ConfirmRequest;
///
/// let request: ConfirmRequest = serde_json::from_str(
///     r#"{"by": "Anna", "note": "Bank statement 2026-10-09 shows it arrived"}"#,
/// )?;
/// assert_eq!(request.by, "Anna");
/// assert!(serde_json::from_str::<ConfirmRequest>(r#"{"by": "Anna"}"#).is_err(), "no note");
/// # Ok::<(), serde_json::Error>(())
/// ```
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfirmRequest {
    /// Who confirms it, as your own system names its staff. 1 to 100 characters.
    pub by: String,
    /// What they checked, such as the bank statement or a call with the recipient. 1 to 500
    /// characters.
    pub note: String,
}

// Debug output leaves out a person's note, here, in RedoRequest and in PersonRequest: it is free
// text, and may hold the recipient's number or personnummer.
impl std::fmt::Debug for ConfirmRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConfirmRequest").field("by", &self.by).finish_non_exhaustive()
    }
}

/// A person's request to pay a payout again under a new reference, for
/// `POST /swish/payout/{payer}/{reference}/redo`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RedoRequest {
    /// The new payout's reference, with the same rules as any reference. The one it replaces stays
    /// spent.
    pub reference: String,
    /// Who asks for it. 1 to 100 characters.
    pub by: String,
    /// Why, when they say. At most 500 characters.
    pub note: Option<String>,
}

impl std::fmt::Debug for RedoRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RedoRequest")
            .field("reference", &self.reference)
            .field("by", &self.by)
            .finish_non_exhaustive()
    }
}

/// A Swish number to pay from, for `POST /swish/payers`.
///
/// `tls` and `signing` are the two certificate files as text, each the certificate followed by its
/// own private key, exactly as swisha keeps them. Unknown fields are refused.
///
/// ```
/// use swisha::domain::request::NewPayerRequest;
///
/// let request: NewPayerRequest = serde_json::from_str(r#"{
///     "name": "sthlm",
///     "nickname": "Stockholm refunds",
///     "number": "1231234567",
///     "tls": "-----BEGIN CERTIFICATE-----...",
///     "signing": "-----BEGIN CERTIFICATE-----...",
///     "by": "Anna"
/// }"#)?;
/// assert_eq!(request.name, "sthlm");
/// # Ok::<(), serde_json::Error>(())
/// ```
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewPayerRequest {
    /// What callers will send as `payer`: 1 to 32 of `a-z`, `0-9`, `-` and `_`. Permanent.
    pub name: String,
    /// What people call it, such as the city or the customer. 1 to 100 characters.
    pub nickname: String,
    /// The Swish number, 10 or 11 digits.
    pub number: String,
    /// The TLS certificate and its key, as PEM text.
    pub tls: String,
    /// The signing certificate and its key, as PEM text. May be the same as `tls` for an older
    /// certificate that does both.
    pub signing: String,
    /// Who adds it. 1 to 100 characters.
    pub by: String,
}

// Debug output leaves out both files, each of which holds a private key.
impl std::fmt::Debug for NewPayerRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NewPayerRequest")
            .field("name", &self.name)
            .field("nickname", &self.nickname)
            .field("number", &self.number)
            .field("by", &self.by)
            .finish_non_exhaustive()
    }
}

/// New certificates for a stored number, for `PUT /swish/payers/{name}/certificates`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CertificatesRequest {
    /// The TLS certificate and its key, as PEM text.
    pub tls: String,
    /// The signing certificate and its key, as PEM text.
    pub signing: String,
    /// Who replaces them. 1 to 100 characters.
    pub by: String,
}

impl std::fmt::Debug for CertificatesRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CertificatesRequest").field("by", &self.by).finish_non_exhaustive()
    }
}

/// A new nickname for a stored number, for `PATCH /swish/payers/{name}`. The name and the number
/// never change, so neither is accepted here.
///
/// ```
/// use swisha::domain::request::NicknameRequest;
///
/// let renamed = r#"{"nickname": "Stockholm", "by": "Anna", "name": "stockholm"}"#;
/// assert!(serde_json::from_str::<NicknameRequest>(renamed).is_err(), "the name is permanent");
/// ```
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NicknameRequest {
    /// What people call it from now on. 1 to 100 characters.
    pub nickname: String,
    /// Who changes it. 1 to 100 characters.
    pub by: String,
}

/// Who disables or enables a number, and why, for `POST /swish/payers/{name}/disable` and
/// `/enable`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PersonRequest {
    /// Who does it. 1 to 100 characters.
    pub by: String,
    /// Why, when they say. At most 500 characters.
    pub note: Option<String>,
}

impl std::fmt::Debug for PersonRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PersonRequest").field("by", &self.by).finish_non_exhaustive()
    }
}
