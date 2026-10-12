//! Telling other processes that a payout changed.
//!
//! PostgreSQL `LISTEN`/`NOTIFY`, through [`postgres::PostgresNotifier`]. Separate from
//! [`events`](crate::events), which feeds subscribers inside this process.

pub mod postgres;
