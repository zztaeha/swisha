#![cfg(feature = "http")]

// What a callback leaves in the server's log. The callback is the one route the internet reaches,
// so nothing it sends may forge a log line or flood one. A binary of its own: a subscriber set
// for one test misses the lines other tests hit first.

mod common;

use serde_json::json;

#[derive(Clone, Default)]
struct Captured(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for Captured {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("capture").extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
    type Writer = Captured;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

const FORGED: &str = "2026-10-10T00:00:00.000000Z  INFO swisha: forged";

#[tokio::test(flavor = "current_thread")]
async fn a_callback_cannot_forge_or_flood_a_log_line() {
    let captured = Captured::default();
    let _logging = tracing::subscriber::set_default(
        tracing_subscriber::fmt().with_writer(captured.clone()).with_ansi(false).finish(),
    );
    let state = common::state().await;
    let callback = common::serve(swisha::http::callback_router().with_state(state)).await;
    let send = |body: serde_json::Value| {
        let request = reqwest::Client::new().post(format!("{callback}/swish/callback")).json(&body);
        async move { request.send().await.expect("swisha answered").status().as_u16() }
    };

    // Each value Swish's callback carries, with a line break and a long tail in it.
    let tail = "x".repeat(20_000);
    let forged = |field: &str| format!("{field}\n{FORGED} {tail}");
    let answered = send(json!({
        "payerAlias": forged("1231111111"),
        "payerPaymentReference": forged("R"),
        "payoutInstructionUUID": "U",
        "status": "PAID",
    }))
    .await;
    assert_eq!(answered, 200, "ignored, as every callback for a number swisha does not pay from");
    let answered = send(json!({
        "payerAlias": common::PAYER_NUMBER,
        "payerPaymentReference": forged("R"),
        "payoutInstructionUUID": forged("U"),
        "status": forged("PAID"),
    }))
    .await;
    assert_eq!(answered, 200, "ignored, as every callback for a payout swisha does not hold");

    let flood = send(json!({ "payerAlias": "1".repeat(100_000), "payerPaymentReference": "R" })).await;
    assert_eq!(flood, 413, "far larger than any callback Swish sends");

    let lines = String::from_utf8(captured.0.lock().expect("capture").clone()).expect("utf-8");
    assert!(lines.contains("not a configured payer"), "the callbacks were logged: {lines}");
    for line in lines.lines() {
        assert!(!line.starts_with(FORGED), "a forged line: {line}");
        assert!(line.len() < 1_000, "a line {} bytes long", line.len());
    }
}
