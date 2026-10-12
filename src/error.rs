//! Two error types, for two audiences.
//!
//! [`BoxError`] is for startup and internals, where a failure is printed once and read by an
//! operator. [`ApiError`] is for anything that reaches an HTTP caller, where the message is part
//! of the contract and the status code carries the meaning.

/// A boxed error, used everywhere a failure is going to be printed rather than matched on.
pub type BoxError = Box<dyn std::error::Error + Send + Sync>;
/// The crate's result type for internal operations.
pub type Result<T> = std::result::Result<T, BoxError>;

/// Builds a [`BoxError`] from a message.
///
/// ```
/// let error = swisha::error::err("SWISH_ENV must be 'production' or 'test'");
/// assert_eq!(error.to_string(), "SWISH_ENV must be 'production' or 'test'");
/// ```
pub fn err(message: impl Into<String>) -> BoxError {
    message.into().into()
}

/// Adds a description to a failure.
///
/// The cause is folded into the message rather than kept as a source chain, because every one of
/// these is printed once at startup and a chain would only be flattened again.
///
/// ```
/// use swisha::error::Context;
///
/// let read = std::fs::read_to_string("/nonexistent/prod.env").context("Cannot read env file");
/// assert!(read.unwrap_err().to_string().starts_with("Cannot read env file: "));
/// ```
pub trait Context<T> {
    /// Describes the failure with a fixed message.
    ///
    /// # Errors
    ///
    /// The original failure, as `{message}: {cause}`.
    fn context(self, message: impl std::fmt::Display) -> Result<T>;
    /// Describes the failure with a message built only if there is one.
    ///
    /// # Errors
    ///
    /// The original failure, as `{message}: {cause}`.
    fn with_context<F, D>(self, f: F) -> Result<T>
    where
        F: FnOnce() -> D,
        D: std::fmt::Display;
}

impl<T, E: std::fmt::Display> Context<T> for std::result::Result<T, E> {
    fn context(self, message: impl std::fmt::Display) -> Result<T> {
        self.map_err(|e| err(format!("{message}: {e}")))
    }

    fn with_context<F, D>(self, f: F) -> Result<T>
    where
        F: FnOnce() -> D,
        D: std::fmt::Display,
    {
        self.map_err(|e| err(format!("{}: {e}", f())))
    }
}

/// A failure on its way to an HTTP caller.
///
/// The [`Display`](std::fmt::Display) text is what the caller receives in `{"error": "..."}`, so
/// these strings are part of the API contract. [`Internal`](ApiError::Internal) deliberately
/// renders a fixed message so no cause ever leaks.
#[derive(Debug)]
pub enum ApiError {
    /// No payout is stored under that reference, or no route matches the path. `404`.
    NotFound,
    /// The `x-api-secret` header is missing or wrong. `401`.
    Unauthorized,
    /// The `x-payer-key` header is missing, or holds no number's current key. `401`.
    UnknownKey,
    /// The key belongs to another number than the one the request is about. `403`.
    Forbidden(String),
    /// The caller exceeded the payout rate limit. `429`.
    TooManyRequests,
    /// A field fails validation: an amount out of range, an invalid Swish number or personnummer.
    /// `400`.
    BadRequest(String),
    /// The reference is already spent, or swisha refuses a confirm or a redo. `409`, and never a
    /// reason to resubmit.
    Conflict(String),
    /// Swish or the database is unreachable, or the payer's certificate has expired. `503`.
    ServiceUnavailable(String),
    /// Swish answered, and the answer was a refusal. `422` when Swish refused the payout's data,
    /// `502` when it refused swisha's own request, `503` when Swish itself failed.
    SwishRejected {
        /// The HTTP status Swish returned.
        code: u16,
        /// Swish's response body, which carries the error code.
        body: String,
    },
    /// A request the HTTP layer refuses before acting on it: a body that is not the expected JSON,
    /// a query or path it cannot decode, a method the route does not serve, or an event stream past
    /// its cap. Carries its own status.
    Rejected {
        /// The HTTP status to answer with.
        status: u16,
        /// What was wrong, for the caller.
        message: String,
    },
    /// An internal fault. `500`, with a fixed message.
    Internal(BoxError),
}

// These strings reach API callers, so they are part of the contract. Internal deliberately
// says nothing about the cause.
impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApiError::NotFound => f.write_str("Not found."),
            ApiError::Unauthorized => f.write_str("Unauthorized."),
            ApiError::UnknownKey => f.write_str(
                "Unauthorized: x-payer-key must hold the current key of the number this request is about.",
            ),
            ApiError::TooManyRequests => f.write_str("Too many requests."),
            ApiError::BadRequest(message)
            | ApiError::Forbidden(message)
            | ApiError::Conflict(message)
            | ApiError::ServiceUnavailable(message)
            | ApiError::Rejected { message, .. } => f.write_str(message),
            ApiError::SwishRejected { code, body } => {
                write!(f, "Swish rejected the request ({code}): {body}")
            }
            ApiError::Internal(_) => f.write_str("Internal server error."),
        }
    }
}

impl std::error::Error for ApiError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ApiError::Internal(cause) => Some(&**cause),
            _ => None,
        }
    }
}
