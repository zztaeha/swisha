//! A Swish number swisha pays from, as it is stored.
//!
//! Each number is a row: the name callers use, a nickname people use, the number itself, who
//! added it, and the hash of its key. Its two certificates are files in the certificate folder,
//! never in the database.

/// One stored number.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PayerRecord {
    /// What callers send as `payer`. Permanent: with the reference, the double-payout key.
    pub name: String,
    /// What people call it. May change at any time.
    pub nickname: String,
    /// The Swish number, sent to Swish as `payerAlias`.
    pub number: String,
    /// Who added it.
    pub added_by: String,
    /// When it was added, as seconds since the Unix epoch.
    pub created_at: i64,
    /// When it last changed, its certificates included, as microseconds since the Unix epoch.
    /// What a running swisha compares to see that a number needs reloading.
    pub updated_at: i64,
    /// Whether it is disabled: it takes no new payouts, while its earlier ones still settle.
    pub disabled: bool,
}

/// A number on its way into storage.
pub struct NewPayer<'a> {
    /// What callers will send as `payer`.
    pub name: &'a str,
    /// What people call it.
    pub nickname: &'a str,
    /// The Swish number.
    pub number: &'a str,
    /// Who adds it.
    pub added_by: &'a str,
    /// The SHA-256 of its key, in hex. The key itself is never stored.
    pub key_hash: &'a str,
}

/// The service log line that records a change to a number: who made it, what it was, and from
/// where. Written in the same write as the change, so a change never stands without it.
pub struct LogLine<'a> {
    /// `info`, `warn` or `error`.
    pub level: &'a str,
    /// What happened, naming who did it.
    pub message: &'a str,
    /// The details, as JSON text.
    pub context: Option<&'a str>,
    /// Who asked, when a request did.
    pub ip: Option<&'a str>,
}

/// The number a key belongs to, as the store holds it at the moment of asking.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyHolder {
    /// The number's name.
    pub name: String,
    /// Whether it is disabled, read with the key so a change is seen at once.
    pub disabled: bool,
}

/// What adding a number found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AddPayerOutcome {
    /// The number is stored.
    Added,
    /// Another number already has this name.
    NameTaken,
    /// The number is already stored, under this name. For the server's log: a caller is never
    /// told another number's name.
    NumberTaken(String),
    /// The number has paid out under this other name. Added under a new one, every reference it
    /// already paid would pay again. The name is for the server's log, as with `NumberTaken`.
    NumberPaidAs(String),
}
