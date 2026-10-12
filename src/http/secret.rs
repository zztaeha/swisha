//! Constant-time checking of the shared secret header.

use axum::http::HeaderMap;

use crate::config::Secret;

/// The header a caller sends the shared secret in.
pub const HEADER: &str = "x-api-secret";

/// Whether a request may proceed.
///
/// No configured secret means no check, which [`Config`](crate::config::Config) only permits on a
/// Unix socket or a loopback port: there the listener is the boundary, enforced by the kernel.
pub fn authorized(headers: &HeaderMap, expected: Option<&Secret>) -> bool {
    match expected {
        Some(secret) => matches(headers, secret.expose()),
        None => true,
    }
}

/// Constant-time comparison of the header against the expected secret.
pub fn matches(headers: &HeaderMap, expected: &str) -> bool {
    if expected.is_empty() {
        return false;
    }
    let provided = headers.get(HEADER).and_then(|v| v.to_str().ok()).unwrap_or("");
    constant_time_eq(provided.as_bytes(), expected.as_bytes())
}

// XOR-fold so every byte is evaluated regardless of value.
/// Whether two byte strings are equal, taking the same time wherever they first differ.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}
