//! What a person sends when they confirm or redo a payout: who they are, and why.

use crate::domain::validate::plain_text;
use crate::error::ApiError;

/// `value` trimmed, when it is fit to keep in the audit trail.
///
/// # Errors
///
/// A `400` naming `field` when it is blank, longer than `max` characters, or holds a control or
/// invisible character.
pub fn text(value: &str, field: &str, max: usize) -> Result<String, ApiError> {
    plain_text(value, field, max).map_err(ApiError::BadRequest)
}
