// swisha must never send a second payout instruction for one reference. A resubmission needs a
// fresh payoutInstructionUUID, which Swish cannot tie back to the original, so it can debit
// twice. The guarantee is that only one call site exists, and no type can express that.

use std::fs;
use std::path::{Path, PathBuf};

fn sources() -> Vec<PathBuf> {
    fn walk(dir: &Path, found: &mut Vec<PathBuf>) {
        for entry in fs::read_dir(dir).expect("read the source directory") {
            let path = entry.expect("read a directory entry").path();
            if path.is_dir() {
                walk(&path, found);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                found.push(path);
            }
        }
    }
    let mut found = Vec::new();
    walk(&Path::new(env!("CARGO_MANIFEST_DIR")).join("src"), &mut found);
    assert!(!found.is_empty(), "found no sources to scan, the test would pass vacuously");
    found
}

// Every line holding `needle`, as file:line, apart from the line that defines it.
fn hits(needle: &str, definition: Option<&str>) -> Vec<String> {
    let mut hits = Vec::new();
    for path in sources() {
        let text = fs::read_to_string(&path).expect("read a source file");
        for (index, line) in text.lines().enumerate() {
            if !line.contains(needle) {
                continue;
            }
            if definition.is_some_and(|definition| line.contains(definition)) {
                continue;
            }
            let name = path.file_name().unwrap_or_default().to_string_lossy().to_string();
            hits.push(format!("{name}:{}", index + 1));
        }
    }
    hits
}

#[test]
fn only_the_payout_route_can_submit_to_swish() {
    let callers = hits("submit_payout(", Some("pub async fn submit_payout"));
    assert_eq!(
        callers.len(),
        1,
        "exactly one call site may submit a payout to Swish, found: {callers:?}"
    );
    assert!(
        callers[0].starts_with("payout.rs:"),
        "the only submit call site must be the payout route, found: {callers:?}"
    );
}

// Proves the sanity of the check above: if the scan were broken it would report zero for
// everything, and the assertion on one call site would be the only thing that noticed.
#[test]
fn the_source_scan_actually_finds_things() {
    assert!(!hits("poll_payout_status(", None).is_empty());
    assert!(!hits("PayoutStore", None).is_empty());
}

#[test]
fn no_resubmission_machinery_survives() {
    for banned in [
        "retry_payout",
        "sweep_retry",
        "claim_retry",
        "retry_context",
        "RetryContext",
        "execute_payout",
        "AUTO_RETRY_CODES",
        "is_auto_retryable",
        "may_sweep_retry",
        "sweep_stalled",
        "retry_count",
        "FAILED_RETRY",
        "FailedRetry",
    ] {
        let found = hits(banned, None);
        assert!(found.is_empty(), "`{banned}` should be gone, found at {found:?}");
    }
}

// The submission is also the crate's only POST to Swish: `swish_post` has one call, in submit.rs,
// and nothing else in the crate posts. A second POST is how a payout gets sent twice.
#[test]
fn the_submission_is_the_only_post_to_swish() {
    let calls = hits("swish_post(", Some("pub async fn swish_post"));
    assert!(calls.len() == 1 && calls[0].starts_with("submit.rs:"), "one call, in submit.rs: {calls:?}");
    let posts = hits(".post(", None);
    assert!(posts.len() == 1 && posts[0].starts_with("client.rs:"), "one POST, in swish_post: {posts:?}");
    for other_way in ["Method::POST", ".request(", "Request::new("] {
        let found = hits(other_way, None);
        assert!(found.is_empty(), "a request built with `{other_way}` could POST unseen: {found:?}");
    }
}
