//! Turning a [`crate::error::ApiError`] into a status code and a JSON body.

use axum::{
    Json,
    extract::rejection::{JsonRejection, PathRejection, QueryRejection},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::json;

use crate::error::ApiError;

// The framework's own rejections answer in the same JSON shape as every other error, keeping
// their status: 400, 415 or 422.
impl From<JsonRejection> for ApiError {
    fn from(rejection: JsonRejection) -> Self {
        ApiError::Rejected { status: rejection.status().as_u16(), message: rejection.body_text() }
    }
}

impl From<QueryRejection> for ApiError {
    fn from(rejection: QueryRejection) -> Self {
        ApiError::Rejected { status: rejection.status().as_u16(), message: rejection.body_text() }
    }
}

impl From<PathRejection> for ApiError {
    fn from(rejection: PathRejection) -> Self {
        ApiError::Rejected { status: rejection.status().as_u16(), message: rejection.body_text() }
    }
}

// The only place an internal error becomes an HTTP status. Internal deliberately renders a
// fixed message so a cause never leaks to a caller.
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = match &self {
            ApiError::NotFound => StatusCode::NOT_FOUND,
            ApiError::Unauthorized | ApiError::UnknownKey => StatusCode::UNAUTHORIZED,
            ApiError::Forbidden(_) => StatusCode::FORBIDDEN,
            ApiError::TooManyRequests => StatusCode::TOO_MANY_REQUESTS,
            ApiError::BadRequest(_) => StatusCode::BAD_REQUEST,
            ApiError::Conflict(_) => StatusCode::CONFLICT,
            ApiError::ServiceUnavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
            ApiError::SwishRejected { code, .. } => refusal_status(*code),
            ApiError::Rejected { status, .. } => {
                StatusCode::from_u16(*status).unwrap_or(StatusCode::BAD_REQUEST)
            }
            ApiError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        let message = match &self {
            ApiError::Internal(_) => "Internal server error.".to_string(),
            other => other.to_string(),
        };
        (status, Json(json!({ "error": message }))).into_response()
    }
}

/// The status swisha answers with when Swish refused a payout, given the status Swish answered.
///
/// Swish refusing the payout's data is the caller's to fix, so it passes through as `422`. Swish
/// refusing swisha's own request is a failed gateway, `502`. Swish failing outright is `503`.
/// Separate from what the refusal proves about the payout, which
/// [`read_rejection`](crate::swish::submit::read_rejection) decides.
pub fn refusal_status(swish_status: u16) -> StatusCode {
    match swish_status {
        422 => StatusCode::UNPROCESSABLE_ENTITY,
        400..=499 => StatusCode::BAD_GATEWAY,
        _ => StatusCode::SERVICE_UNAVAILABLE,
    }
}
