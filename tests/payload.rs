use swisha::swish::payload::{ExecutePayoutArgs, PayoutPayload};

fn args<'a>(payee_ssn: Option<&'a str>, amount: f64, message: &'a str) -> ExecutePayoutArgs<'a> {
    ExecutePayoutArgs {
        reference: "K-1",
        payout_uuid: "ABC123",
        payee_alias: "46701234567",
        payee_ssn,
        amount,
        message,
    }
}

fn json(a: &ExecutePayoutArgs<'_>) -> String {
    let payload = PayoutPayload::new(a, "1234679304", "00A5FF0F2C7B01", "2026-08-25T19:00:00Z".into());
    serde_json::to_string(&payload).unwrap()
}

// The signature covers exactly these bytes, so key order and spelling are load-bearing.
#[test]
fn wire_format_with_ssn() {
    assert_eq!(
        json(&args(Some("196408233234"), 100.0, "Acme AB: K-1")),
        r#"{"payoutInstructionUUID":"ABC123","payerPaymentReference":"K-1","payerAlias":"1234679304","payeeAlias":"46701234567","payeeSSN":"196408233234","amount":"100.00","currency":"SEK","payoutType":"PAYOUT","message":"Acme AB: K-1","instructionDate":"2026-08-25T19:00:00Z","signingCertificateSerialNumber":"00A5FF0F2C7B01"}"#
    );
}

#[test]
fn payee_ssn_is_omitted_entirely_when_absent() {
    let out = json(&args(None, 100.0, "Acme AB: K-1"));
    assert!(!out.contains("payeeSSN"));
    assert_eq!(
        out,
        r#"{"payoutInstructionUUID":"ABC123","payerPaymentReference":"K-1","payerAlias":"1234679304","payeeAlias":"46701234567","amount":"100.00","currency":"SEK","payoutType":"PAYOUT","message":"Acme AB: K-1","instructionDate":"2026-08-25T19:00:00Z","signingCertificateSerialNumber":"00A5FF0F2C7B01"}"#
    );
}

#[test]
fn amount_is_always_two_decimals_as_a_string() {
    assert!(json(&args(None, 1.0, "m")).contains(r#""amount":"1.00""#));
    assert!(json(&args(None, 49999.995, "m")).contains(r#""amount":"50000.00""#));
    assert!(json(&args(None, 0.005, "m")).contains(r#""amount":"0.01""#));
}

#[test]
fn message_is_capped_at_fifty_characters() {
    let long = "x".repeat(80);
    let out = json(&args(None, 1.0, &long));
    assert!(out.contains(&format!(r#""message":"{}""#, "x".repeat(50))));
}

#[test]
fn message_cap_counts_characters_not_bytes() {
    let long = "å".repeat(80);
    let out = json(&args(None, 1.0, &long));
    assert!(out.contains(&format!(r#""message":"{}""#, "å".repeat(50))));
}
