use swisha::domain::request::PayoutRequest;

fn parse(json: &str) -> Result<PayoutRequest, serde_json::Error> {
    serde_json::from_str(json)
}

#[test]
fn accepts_the_five_field_model() {
    let req = parse(
        r#"{"reference":"INV-1","payee_alias":"0701234567","payee_ssn":"196408233234",
            "amount":100.0,"message":"Acme AB: INV-1"}"#,
    )
    .unwrap();
    assert_eq!(req.reference, "INV-1");
    assert_eq!(req.payee_alias, "0701234567");
    assert_eq!(req.payee_ssn.as_deref(), Some("196408233234"));
    assert_eq!(req.amount, 100.0);
    assert_eq!(req.message.as_deref(), Some("Acme AB: INV-1"));
}

#[test]
fn only_reference_alias_and_amount_are_required() {
    let req = parse(r#"{"reference":"INV-2","payee_alias":"0701234567","amount":50.0}"#).unwrap();
    assert!(req.payee_ssn.is_none());
    assert!(req.message.is_none());
}

// Nothing prints these today. A log line or a panic message that ever does must not carry a
// personnummer, a phone number or a private key with it, nor a person's note, which may hold both.
#[test]
fn debug_output_leaves_out_the_personnummer_and_the_keys() {
    use swisha::domain::payout::RedoDetails;
    use swisha::domain::request::{CertificatesRequest, ConfirmRequest, NewPayerRequest, PersonRequest, RedoRequest};

    let payout = parse(r#"{"reference":"INV-3","payee_alias":"0701234567","payee_ssn":"196408233234","amount":1.0}"#)
        .expect("parse");
    let pem = "-----BEGIN PRIVATE KEY-----\nMIIsecret\n-----END PRIVATE KEY-----";
    let payer: NewPayerRequest = serde_json::from_value(serde_json::json!({
        "name": "sthlm", "nickname": "Stockholm", "number": "1231234567", "tls": pem, "signing": pem, "by": "Anna",
    }))
    .expect("parse");
    let certificates: CertificatesRequest =
        serde_json::from_value(serde_json::json!({ "tls": pem, "signing": pem, "by": "Anna" })).expect("parse");
    let redo = RedoDetails {
        payee_alias: "46701234567".into(),
        payee_ssn: Some("196408233234".into()),
        amount: 1.0,
        message: "INV-3".into(),
    };

    let note = "Called 0701234567, personnummer 196408233234";
    let confirm: ConfirmRequest = serde_json::from_value(serde_json::json!({ "by": "Anna", "note": note })).expect("parse");
    let redone: RedoRequest =
        serde_json::from_value(serde_json::json!({ "reference": "INV-4", "by": "Anna", "note": note })).expect("parse");
    let person: PersonRequest = serde_json::from_value(serde_json::json!({ "by": "Anna", "note": note })).expect("parse");

    for printed in [
        format!("{payout:?}"),
        format!("{payer:?}"),
        format!("{certificates:?}"),
        format!("{redo:?}"),
        format!("{confirm:?}"),
        format!("{redone:?}"),
        format!("{person:?}"),
    ] {
        assert!(!printed.contains("196408233234"), "{printed}");
        assert!(!printed.contains("0701234567") && !printed.contains("46701234567"), "{printed}");
        assert!(!printed.contains("MIIsecret") && !printed.contains("PRIVATE KEY"), "{printed}");
    }
    assert!(format!("{payout:?}").contains("INV-3"), "the reference still identifies it");
    assert!(format!("{payer:?}").contains("sthlm"), "the name still identifies it");
}
