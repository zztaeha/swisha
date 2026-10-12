//! Small self-contained helpers with no swisha-specific logic.
//!
//! **Not public API.** Visible only because the integration tests reach into it. Nothing here is
//! covered by semver, and none of these types appear in any public signature elsewhere in the
//! crate, so hiding the module costs a caller nothing.

pub mod base64;
pub mod fs;
pub mod hex;
pub mod net;
pub mod time;
