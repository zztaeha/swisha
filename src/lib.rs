#![forbid(unsafe_code)]
// The docs are the crate's front door: an undocumented item, a fallible call with no # Errors, or
// a broken link fails the build rather than reaching a reader.
#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]
#![deny(clippy::missing_errors_doc, clippy::missing_panics_doc, clippy::doc_markdown)]
#![cfg_attr(docsrs, feature(doc_cfg))]
//! Swish payouts as a service, and as a library.
//!
//! swisha speaks the Swish CPC Payouts protocol: it signs the payload the way the reference
//! requires, refuses to pay the same reference twice from the same payer, and recovers the
//! outcome when a callback never arrives. One process pays from any number of Swish numbers, each
//! stored with its own certificates and checked on its own, so no number ever waits on another.
//! The binary adds an HTTP API on top; without the `http` feature you get the payout engine, the
//! storage trait and the Swish client on their own.
//!
//! # The guarantee that shapes everything else
//!
//! **swisha never resubmits a payout on its own.** A resubmission needs a fresh
//! `payoutInstructionUUID`, which Swish cannot tie back to the original, so a payout that was in
//! fact already debited would be debited a second time. Checking therefore only ever reads: it
//! follows each payout to Swish's last word, and keeps asking about any Swish has not confirmed
//! for 48 hours. Paying again is a person's call, made as a new payout under a new reference.
//! It goes out only once Swish, asked right then, says nothing before it was paid, and the store
//! refuses it for a payout whose money left.
//!
//! # Running the service
//!
//! The `swisha` binary reads one env file, pays from the numbers stored in its database, and serves
//! two listeners: an internal API for your own backend, through which numbers are added and
//! changed too, and the callback Swish calls. Each number has its own key, and every request about
//! a number needs it, so the API secret alone reaches no number already stored.
//!
//! `swisha --print-schema` prints the PostgreSQL tables it needs. On the server, `--new-key`,
//! `--disable`, `--enable` and `--list-payers` act on any number, since there the env file already
//! means full control. The [README](https://github.com/zztaeha/swisha#readme) covers the rest:
//! certificates, the reverse proxy, the callback allowlist and deployment.
//!
//! # Using it as a library
//!
//! ```toml
//! # Not on crates.io. Pin a tag rather than tracking a branch: an unpinned dependency that
//! # moves money is one `cargo update` away from a payout path you have not read.
//! swisha = { git = "https://github.com/zztaeha/swisha", tag = "v1.0.0", default-features = false }
//! serde_json = "1"  # for the signing example below
//! ```
//!
//! | Feature | Default | Adds |
//! |---|---|---|
// `http` exists only with its feature, so the rows naming it link there only when it is built.
#![cfg_attr(feature = "http", doc = "| `http` | yes | The HTTP API in [`http`], and the `swisha` binary. Brings in axum |")]
#![cfg_attr(not(feature = "http"), doc = "| `http` | yes | The HTTP API in `http`, and the `swisha` binary. Brings in axum |")]
//!
//! Building and signing a payout instruction, the part of the protocol that is easiest to get
//! wrong:
//!
//! ```no_run
//! use swisha::swish::payload::{ExecutePayoutArgs, PayoutPayload};
//! use swisha::swish::sign::{rsa_key_pair, sign_payload};
//! use swisha::swish::{cert::read_cert_serial, random_payout_uuid};
//!
//! let signing = std::fs::read_to_string("/var/lib/swisha/1231234567/signing-1231234567-sthlm.pem")?;
//! let key = rsa_key_pair(&signing)?;
//! let serial = read_cert_serial(&signing)?;
//!
//! // One UUID per payout, forever: a second one is a second instruction, and a second debit.
//! let uuid = random_payout_uuid();
//! let args = ExecutePayoutArgs {
//!     reference: "INV-1042",
//!     payout_uuid: &uuid,
//!     payee_alias: "46701234567",
//!     payee_ssn: Some("196408233234"),
//!     amount: 250.00,
//!     message: "Faktura 1042",
//! };
//! let payload = PayoutPayload::new(&args, "1231234567", &serial, "2026-10-07T12:00:00Z".into());
//!
//! // Sign exactly the bytes that are sent.
//! let wire = serde_json::to_string(&payload)?;
//! let signature = sign_payload(&wire, &key)?;
//! # Ok::<(), swisha::error::BoxError>(())
//! ```
//!
//! swisha's calls return [`error::Result`], whose error is [`error::BoxError`], a
//! `Box<dyn Error + Send + Sync>`. Wrap the example in a function returning that, such as
//! `fn main() -> swisha::error::Result<()>`: with a plain `Box<dyn Error>`, its `?` does not
//! compile.
//!
//! [`swish::submit::sign_payout`] does the same from a configured payer, and
//! [`swish::submit::submit_payout`] sends what it signed over that payer's mTLS client: the only
//! place swisha ever posts an instruction.
//!
//! # Storage
//!
//! PostgreSQL 18, the only database swisha supports, reached through the
//! [`store::PayoutStore`] trait. The trait says what each operation must guarantee, and
//! [`store::conformance`] is that contract as executable checks.
//!
//! # Where to start
//!
//! | Want to | Look at |
//! |---|---|
//! | Build a payout payload and sign it | [`swish::payload`], [`swish::sign`] |
//! | Check a request the way Swish will | [`domain::validate`] |
//! | Understand the state machine | [`domain::status`] |
//! | Decide what a person may do about a payout | [`domain::review`] |
//! | Turn a Swish error code into something a person can act on | [`domain::errors`] |
//! | Read configuration from the environment | [`config::Config::from_env`] |
//! | Add and change the numbers to pay from, and their keys | [`payers`], [`state::Payers`] |
//! | See what the store must guarantee | [`store::PayoutStore`], then [`store::conformance`] |
#![cfg_attr(feature = "http", doc = "| Run the HTTP API in your own process | [`http::serve`] |")]
#![cfg_attr(feature = "http", doc = "| See who may act for which number | [`http::access`] |")]
//!
//! # Minimum supported Rust version
//!
//! Rust 1.99, as `rust-version` in `Cargo.toml` declares.
//!
//! # Security
//!
//! [SECURITY.md](https://github.com/zztaeha/swisha/blob/main/SECURITY.md) lists the properties
//! swisha guarantees and how to report a vulnerability. Please do not report one in a public
//! issue.

pub mod backend;
pub mod config;
pub mod domain;
pub mod env;
pub mod error;
pub mod events;
#[cfg(feature = "http")]
pub mod http;
pub mod notify;
pub mod payers;
pub mod state;
pub mod store;
pub mod swish;
// Public because the integration tests reach into it, and an integration test can only see
// public items. Not API: hidden from the docs and exempt from semver.
#[doc(hidden)]
pub mod util;
