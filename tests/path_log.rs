#![cfg(feature = "http")]

// What a request's path leaves in the server's log: a reference holding a line break must not forge
// a line. A binary of its own, since a subscriber set for one test misses lines other tests hit first.

mod common;

use common::{PAYER, SECRET};
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

// Every read of a payout fails, so a reference that reached the store would reach its error line.
// One no payout can have is answered 404 first, and logged nowhere.
#[tokio::test(flavor = "current_thread")]
async fn a_path_reference_cannot_forge_a_log_line() {
    let captured = Captured::default();
    let _logging = tracing::subscriber::set_default(
        tracing_subscriber::fmt().with_writer(captured.clone()).with_ansi(false).finish(),
    );
    let state = common::state().await;
    let table = state.config.table_payouts.clone();
    sqlx::query(sqlx::AssertSqlSafe(format!("ALTER TABLE {table} RENAME COLUMN status TO unreadable")))
        .execute(state.store.pool())
        .await
        .expect("make the payouts unreadable");
    let base = common::serve(swisha::http::internal_router().with_state(state)).await;

    let reference = format!("R%0A{}", FORGED.replace(' ', "%20"));
    let client = reqwest::Client::new();
    let requests = [
        client.get(format!("{base}/swish/status/{PAYER}/{reference}")),
        client.get(format!("{base}/swish/status/{reference}")),
        client.post(format!("{base}/swish/payout/{PAYER}/{reference}/confirm")).json(&json!({ "by": "Anna", "note": "bank" })),
        client.post(format!("{base}/swish/payout/{PAYER}/{reference}/redo")).json(&json!({ "reference": "R-2", "by": "Anna" })),
    ];
    let mut answers = Vec::new();
    for request in requests {
        let response = request
            .header("x-api-secret", SECRET)
            .header(common::KEY_HEADER, common::key(PAYER))
            .send()
            .await
            .expect("swisha answered");
        answers.push((response.url().path().to_string(), response.status().as_u16()));
    }

    let lines = String::from_utf8(captured.0.lock().expect("capture").clone()).expect("utf-8");
    for line in lines.lines() {
        assert!(!line.starts_with(FORGED), "a forged line: {line}");
    }
    for (path, code) in answers {
        assert_eq!(code, 404, "{path}");
    }
}
