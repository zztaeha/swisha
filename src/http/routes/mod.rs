//! One module per endpoint, [`payers`] for the numbers, and [`person`], which confirm and redo
//! share.

pub mod callback;
pub mod confirm;
pub mod events;
pub mod health;
pub mod payers;
pub mod payout;
pub mod person;
pub mod status;

use crate::domain::validate::is_valid_reference;
use crate::error::ApiError;

// A path's reference, when a payout can be stored under it. Any other names none, and is answered
// 404 before the store is read or anything is logged: one holding a line break could forge a line.
fn payout_reference(reference: &str) -> Result<(), ApiError> {
    if is_valid_reference(reference) { Ok(()) } else { Err(ApiError::NotFound) }
}
