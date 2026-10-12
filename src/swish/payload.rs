//! The payout instruction, in wire order.
//!
//! Kept apart from the transport because a mistake in this file breaks every signature, and the
//! error Swish returns for a bad signature says nothing about signatures.

use serde::Serialize;

/// The caller-supplied half of a payout instruction.
pub struct ExecutePayoutArgs<'a> {
    /// The caller's identifier, sent as `payerPaymentReference`.
    pub reference:    &'a str,
    /// The `payoutInstructionUUID` for this instruction.
    pub payout_uuid:   &'a str,
    /// The recipient's Swish number, normalized.
    pub payee_alias: &'a str,
    /// The recipient's personnummer, 12 digits, when supplied.
    pub payee_ssn:  Option<&'a str>,
    /// Amount in SEK.
    pub amount:        f64,
    /// The recipient's message. Truncated to 50 characters by [`PayoutPayload::new`].
    pub message:       &'a str,
}

/// The Swish payout instruction, in wire order.
///
/// **Field order matters.** The signature covers exactly the bytes that get sent, so this
/// declaration is the wire format. Reordering a field changes what is signed.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PayoutPayload<'a> {
    /// Swish's identity for this instruction. A new value is a new instruction.
    #[serde(rename = "payoutInstructionUUID")]
    pub payout_instruction_uuid: &'a str,
    /// The caller's own reference, echoed back in the callback.
    pub payer_payment_reference: &'a str,
    /// The merchant's Swish number, paying out.
    pub payer_alias: &'a str,
    /// The recipient's Swish number.
    pub payee_alias: &'a str,
    /// The recipient's personnummer. Omitted from the payload entirely when absent, rather than
    /// sent as null.
    #[serde(rename = "payeeSSN", skip_serializing_if = "Option::is_none")]
    pub payee_ssn: Option<&'a str>,
    /// Amount, formatted to exactly two decimals.
    pub amount: String,
    /// Always `SEK`.
    pub currency: &'static str,
    /// Always `PAYOUT`.
    pub payout_type: &'static str,
    /// What the recipient reads, at most 50 characters.
    pub message: String,
    /// When the instruction was issued.
    pub instruction_date: String,
    /// The serial of the certificate that signed, so Swish can find the public key.
    pub signing_certificate_serial_number: &'a str,
}

impl<'a> PayoutPayload<'a> {
    /// Builds the instruction, formatting the amount to two decimals and truncating the message
    /// to Swish's 50 characters.
    ///
    /// Serialized, this is exactly what is signed and sent, field for field:
    ///
    /// ```
    /// use swisha::swish::payload::{ExecutePayoutArgs, PayoutPayload};
    ///
    /// let args = ExecutePayoutArgs {
    ///     reference: "INV-1042",
    ///     payout_uuid: "7CBA599425BC499C93D6F0E7310085C3",
    ///     payee_alias: "46701234567",
    ///     payee_ssn: Some("196408233234"),
    ///     amount: 250.5,
    ///     message: "Faktura 1042",
    /// };
    /// let payload = PayoutPayload::new(&args, "1231234567", "4512B3", "2026-10-07T12:00:00Z".into());
    /// let wire = serde_json::to_string(&payload)?;
    /// assert!(wire.starts_with(r#"{"payoutInstructionUUID":"7CBA599425BC499C93D6F0E7310085C3","#));
    /// assert!(wire.contains(r#""amount":"250.50","currency":"SEK","payoutType":"PAYOUT""#));
    /// # Ok::<(), serde_json::Error>(())
    /// ```
    pub fn new(
        args: &ExecutePayoutArgs<'a>,
        payer_alias: &'a str,
        signing_serial: &'a str,
        instruction_date: String,
    ) -> Self {
        Self {
            payout_instruction_uuid: args.payout_uuid,
            payer_payment_reference: args.reference,
            payer_alias,
            payee_alias: args.payee_alias,
            payee_ssn: args.payee_ssn,
            amount: format!("{:.2}", args.amount),
            currency: "SEK",
            payout_type: "PAYOUT",
            message: crate::domain::validate::swish_message(args.message),
            instruction_date,
            signing_certificate_serial_number: signing_serial,
        }
    }
}
