#![cfg(feature = "http")]

use axum::{http::StatusCode, response::IntoResponse};
use swisha::error::{ApiError, err};

// These strings go to API callers, so they are part of the contract.
#[test]
fn display_strings() {
    assert_eq!(ApiError::NotFound.to_string(), "Not found.");
    assert_eq!(ApiError::Unauthorized.to_string(), "Unauthorized.");
    assert_eq!(
        ApiError::UnknownKey.to_string(),
        "Unauthorized: x-payer-key must hold the current key of the number this request is about."
    );
    assert_eq!(ApiError::Forbidden("Not this number.".into()).to_string(), "Not this number.");
    assert_eq!(ApiError::TooManyRequests.to_string(), "Too many requests.");
    assert_eq!(ApiError::BadRequest("Ogiltigt belopp.".into()).to_string(), "Ogiltigt belopp.");
    assert_eq!(ApiError::Conflict("Redan genomförd.".into()).to_string(), "Redan genomförd.");
    assert_eq!(
        ApiError::ServiceUnavailable("Swish API unreachable.".into()).to_string(),
        "Swish API unreachable."
    );
    assert_eq!(
        ApiError::SwishRejected { code: 422, body: r#"{"errorCode":"RF07"}"#.into() }.to_string(),
        r#"Swish rejected the request (422): {"errorCode":"RF07"}"#
    );
    // The cause is never shown to a caller.
    assert_eq!(
        ApiError::Internal(err("database exploded")).to_string(),
        "Internal server error."
    );
}

#[test]
fn empty_payload_strings_stay_empty() {
    assert_eq!(ApiError::BadRequest(String::new()).to_string(), "");
    assert_eq!(
        ApiError::SwishRejected { code: 500, body: String::new() }.to_string(),
        "Swish rejected the request (500): "
    );
}

#[test]
fn http_status_mapping() {
    let cases = [
        (ApiError::NotFound, StatusCode::NOT_FOUND),
        (ApiError::Unauthorized, StatusCode::UNAUTHORIZED),
        (ApiError::UnknownKey, StatusCode::UNAUTHORIZED),
        (ApiError::Forbidden("x".into()), StatusCode::FORBIDDEN),
        (ApiError::TooManyRequests, StatusCode::TOO_MANY_REQUESTS),
        (ApiError::BadRequest("x".into()), StatusCode::BAD_REQUEST),
        (ApiError::Conflict("x".into()), StatusCode::CONFLICT),
        (ApiError::ServiceUnavailable("x".into()), StatusCode::SERVICE_UNAVAILABLE),
        (ApiError::SwishRejected { code: 422, body: "x".into() }, StatusCode::UNPROCESSABLE_ENTITY),
        (ApiError::SwishRejected { code: 401, body: "x".into() }, StatusCode::BAD_GATEWAY),
        (ApiError::SwishRejected { code: 500, body: "x".into() }, StatusCode::SERVICE_UNAVAILABLE),
        (ApiError::Internal(err("x")), StatusCode::INTERNAL_SERVER_ERROR),
    ];
    for (error, expected) in cases {
        assert_eq!(error.into_response().status(), expected);
    }
}

// Whose problem a refusal is decides the answer: the caller's data (422), swisha's own request
// (502), or Swish failing (503). Pinned per status, since a caller branches on it.
#[test]
fn a_swish_refusal_maps_to_whose_problem_it_is() {
    use swisha::http::response::refusal_status;
    for (swish, expected) in [
        (422, 422),
        (400, 502), (401, 502), (403, 502), (404, 502), (415, 502), (429, 502),
        (500, 503), (502, 503), (503, 503), (504, 503),
    ] {
        assert_eq!(refusal_status(swish).as_u16(), expected, "Swish answered {swish}");
    }
}

// Internal carries its cause as a source, so the chain reaches the real error in the logs.
#[test]
fn only_internal_carries_a_source() {
    use std::error::Error;

    let internal = ApiError::Internal(err("database exploded"));
    let source = internal.source().expect("Internal should expose its cause");
    assert!(
        source.to_string().contains("database exploded"),
        "the chain should reach the real error: {source}"
    );

    // The rest describe themselves fully, so a chain would add nothing.
    for e in [
        ApiError::NotFound,
        ApiError::Unauthorized,
        ApiError::TooManyRequests,
        ApiError::BadRequest("x".into()),
        ApiError::Conflict("x".into()),
        ApiError::ServiceUnavailable("x".into()),
        ApiError::SwishRejected { code: 422, body: "x".into() },
    ] {
        assert!(e.source().is_none(), "{e} should not carry a source");
    }
}
