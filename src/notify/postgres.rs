//! Publishing through `pg_notify`, heard by any session running `LISTEN` on the channel at that
//! moment.

use sqlx::PgPool;

use crate::error::Result;

/// Publishes through PostgreSQL `NOTIFY`, so listeners in other processes hear updates.
///
/// Best effort and lossy: only sessions listening at that moment hear one, and nothing replays a
/// missed message. That is why `GET /swish/status/{payer}/{reference}` exists, and why nothing
/// depends on delivery.
pub struct PostgresNotifier {
    pool: PgPool,
}

impl PostgresNotifier {
    /// Wraps a connection pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Publishes one payload on one channel. A failure is logged by the caller, never fatal: a
    /// lost notification must not fail a payout.
    ///
    /// # Errors
    ///
    /// When PostgreSQL cannot be reached or refuses the channel name or payload.
    pub async fn publish(&self, channel: &str, payload: &str) -> Result<()> {
        sqlx::query("SELECT pg_notify($1, $2)")
            .bind(channel)
            .bind(payload)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Publishes two payloads in one statement, so they commit together, in this order.
    ///
    /// # Errors
    ///
    /// As [`publish`](Self::publish).
    pub async fn publish_both(&self, first: (&str, &str), second: (&str, &str)) -> Result<()> {
        sqlx::query("SELECT pg_notify($1, $2), pg_notify($3, $4)")
            .bind(first.0)
            .bind(first.1)
            .bind(second.0)
            .bind(second.1)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}
