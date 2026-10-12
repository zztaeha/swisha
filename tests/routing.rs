#![cfg(feature = "http")]

// Serves each router on a listener of its own and asks real questions over real HTTP: which route
// lives on which listener, and what every error looks like.

mod common;

use common::{serve, state, SECRET};

fn payout_body() -> serde_json::Value {
    serde_json::json!({
        "reference":   "ROUTING-TEST",
        "payee_alias": common::PHONE,
        "amount":      100.0,
    })
}

// Every route of the internal listener, by method. Each moves money or changes what moves it, or
// shows payouts, so none may ever answer on the callback listener.
fn internal_routes() -> Vec<(reqwest::Method, &'static str)> {
    use reqwest::Method;
    vec![
        (Method::POST, "/swish/payout"),
        (Method::POST, "/swish/payout/test/INV-1/confirm"),
        (Method::POST, "/swish/payout/test/INV-1/redo"),
        (Method::GET, "/system/health"),
        (Method::GET, "/events"),
        (Method::GET, "/swish/status/ROUTING-TEST"),
        (Method::POST, "/swish/payers"),
        (Method::GET, "/swish/payers/test"),
        (Method::PATCH, "/swish/payers/test"),
        (Method::PUT, "/swish/payers/test/certificates"),
        (Method::POST, "/swish/payers/test/disable"),
        (Method::POST, "/swish/payers/test/enable"),
        (Method::POST, "/swish/payers/test/key"),
    ]
}

// No secret header is ever sent, so a route that exists answers 401 and stops before it
// reaches storage or Swish. That makes 401 mean "present" and 404 mean "absent".
async fn status_of(base: &str, method: reqwest::Method, path: &str) -> u16 {
    let http = reqwest::Client::new();
    let url = format!("{base}{path}");
    let request = match method {
        reqwest::Method::GET => http.get(url),
        method => http.request(method, url).json(&payout_body()),
    };
    request.send().await.expect("request").status().as_u16()
}

#[tokio::test]
async fn the_callback_listener_exposes_nothing_but_the_callback() {
    let base = serve(swisha::http::callback_router().with_state(state().await)).await;

    for (method, path) in internal_routes() {
        assert_eq!(
            status_of(&base, method, path).await,
            404,
            "{path} must never be reachable on the internet-facing listener"
        );
    }
}

#[tokio::test]
async fn the_callback_listener_still_serves_the_callback() {
    let base = serve(swisha::http::callback_router().with_state(state().await)).await;

    // A body the callback cannot parse is rejected by the extractor, which only runs once the
    // path has matched. That proves the route is there without reaching the handler.
    let code = reqwest::Client::new()
        .post(format!("{base}/swish/callback"))
        .header("content-type", "application/json")
        .body("{ not json")
        .send()
        .await
        .expect("request")
        .status()
        .as_u16();
    assert_ne!(code, 404, "the callback route is missing from its own listener");
}

#[tokio::test]
async fn the_internal_listener_serves_every_internal_route() {
    let base = serve(swisha::http::internal_router().with_state(state().await)).await;

    for (method, path) in internal_routes() {
        assert_eq!(status_of(&base, method, path).await, 401, "{path} should exist and demand the secret");
    }
}

// axum captures `{reference}`, so a literal `:reference` in the path is a value like any other.
// That proves the placeholder is still a placeholder.
#[tokio::test]
async fn the_status_route_captures_its_path_parameter() {
    let base = serve(swisha::http::internal_router().with_state(state().await)).await;

    for reference in ["INV-1001", "abc", "a-b_c.1"] {
        assert_eq!(
            status_of(&base, reqwest::Method::GET, &format!("/swish/status/{reference}")).await,
            401,
            "/swish/status/{reference} did not match the route"
        );
    }
    assert_eq!(
        status_of(&base, reqwest::Method::GET, "/swish/status/:reference").await,
        401,
        "a literal colon should be captured as a value, not treated as syntax"
    );
}

#[tokio::test]
async fn the_internal_listener_does_not_accept_callbacks() {
    let base = serve(swisha::http::internal_router().with_state(state().await)).await;

    assert_eq!(
        status_of(&base, reqwest::Method::POST, "/swish/callback").await,
        404,
        "Swish callbacks belong on the callback listener only"
    );
}

#[tokio::test]
async fn unknown_paths_fall_back_to_swishas_own_not_found() {
    for base in [
        serve(swisha::http::internal_router().with_state(state().await)).await,
        serve(swisha::http::callback_router().with_state(state().await)).await,
    ] {
        let response = reqwest::Client::new()
            .get(format!("{base}/nope"))
            .send()
            .await
            .expect("request");
        assert_eq!(response.status().as_u16(), 404);
        assert!(
            response.text().await.expect("body").contains("Not found."),
            "the fallback should be swisha's error shape, not an empty body"
        );
    }
}

// Every error the internal API gives has the same JSON shape, the framework's own rejections
// included, so a caller never has to parse text/plain.
#[tokio::test]
async fn every_internal_error_is_json() {
    let base = serve(swisha::http::internal_router().with_state(state().await)).await;
    let http = reqwest::Client::new();
    let payout = format!("{base}/swish/payout");
    let json = |body: &'static str| http.post(&payout).header("content-type", "application/json").body(body);
    let cases = [
        ("unknown field", json(r#"{"reference":"X1","amount":1,"payee_alias":"0701234567","payee_ssn":"196408233234","tip":5}"#), 422),
        ("wrong type", json(r#"{"reference":"X1","amount":"one","payee_alias":"0701234567","payee_ssn":"196408233234"}"#), 422),
        ("broken JSON", json(r#"{"reference":"#), 400),
        ("no content type", http.post(&payout).body(r#"{"reference":"X1"}"#), 415),
        ("wrong method", http.get(&payout), 405),
        ("repeated query field", http.get(format!("{base}/events?payer=a&payer=b")), 400),
        ("path not UTF-8", http.get(format!("{base}/swish/status/test/%FF")), 400),
        ("unknown path", http.get(format!("{base}/nope")), 404),
    ];
    for (label, request, expected) in cases {
        let response = request
            .header("x-api-secret", SECRET)
            .header(common::KEY_HEADER, common::key(common::PAYER))
            .send()
            .await
            .expect("request");
        let code = response.status().as_u16();
        let kind = response.headers().get("content-type").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
        let body = response.text().await.expect("body");
        assert_eq!(code, expected, "{label}: {body}");
        assert!(kind.starts_with("application/json"), "{label}: {kind} {body}");
        let error: serde_json::Value = serde_json::from_str(&body).expect(label);
        assert!(error["error"].as_str().is_some_and(|e| !e.is_empty()), "{label}: {body}");
    }
}

// A caller without the secret learns nothing about the body's shape: the secret comes first.
#[tokio::test]
async fn the_secret_is_checked_before_the_body() {
    let base = serve(swisha::http::internal_router().with_state(state().await)).await;
    let code = reqwest::Client::new()
        .post(format!("{base}/swish/payout"))
        .header("content-type", "application/json")
        .body(r#"{"tip":5}"#)
        .send()
        .await
        .expect("request")
        .status()
        .as_u16();
    assert_eq!(code, 401);
}
