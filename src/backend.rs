//! Opening the PostgreSQL connection pool and the notifier.

use crate::config::{Config, DbAddress, DbTls};
use crate::error::{Context, Result};
use crate::state::{Notifications, Store};

/// Opens the PostgreSQL connection pool, and the store and notifier that share it.
///
/// Every connection runs with a 10-second statement timeout and the TLS mode
/// [`Config::db_tls`] names; `PGSSLMODE` in the environment is not consulted. The password is
/// `DB_PASS` alone, and no `.pgpass` file is read. Follow it with [`verify_store`] before serving,
/// as the binary does.
///
/// # Errors
///
/// When `DB_HOST` cannot be read, PostgreSQL cannot be reached with the configured credentials,
/// or the TLS mode demands what the server does not offer.
///
/// # Examples
///
/// ```no_run
/// # async fn run() -> swisha::error::Result<()> {
/// let config = swisha::config::Config::from_env(&swisha::env::Env::discover()?)?;
/// let (store, notifier) = swisha::backend::connect(&config).await?;
/// swisha::backend::verify_store(&store).await?;
/// # Ok(())
/// # }
/// ```
pub async fn connect(config: &Config) -> Result<(Store, Notifications)> {
    use std::time::Duration;
    use sqlx::postgres::{PgConnectOptions, PgPoolOptions, PgSslMode};

    // new() reads ~/.pgpass before DB_PASS replaces whatever it found, so it could only warn: under
    // the shipped unit, at every start, since ProtectHome hides the file.
    let mut options = match config.db_address()? {
        DbAddress::Socket(directory) => PgConnectOptions::new_without_pgpass().socket(directory),
        DbAddress::Tcp(host, port) => PgConnectOptions::new_without_pgpass().host(&host).port(port),
    }
    .username(&config.db_user)
    .password(config.db_pass.expose())
    .database(&config.db_name)
    .ssl_mode(match config.db_tls {
        DbTls::Disable => PgSslMode::Disable,
        DbTls::Prefer => PgSslMode::Prefer,
        DbTls::Require => PgSslMode::Require,
        DbTls::VerifyCa => PgSslMode::VerifyCa,
        DbTls::VerifyFull => PgSslMode::VerifyFull,
    });
    if let Some(root) = &config.db_ssl_root_cert {
        options = options.ssl_root_cert(root);
    }

    let pool = PgPoolOptions::new()
        .max_connections(config.db_pool_size)
        .min_connections(2.min(config.db_pool_size))
        .acquire_timeout(Duration::from_secs(5))
        .max_lifetime(Duration::from_secs(1800))
        .test_before_acquire(false)
        .idle_timeout(Duration::from_secs(300))
        // Cap query execution so a runaway statement cannot hold a connection indefinitely.
        .after_connect(|conn, _| {
            Box::pin(async move {
                sqlx::query("SET statement_timeout = '10s'")
                    .execute(&mut *conn)
                    .await?;
                Ok(())
            })
        })
        .connect_with(options)
        .await
        .context("Cannot connect to PostgreSQL")?;

    let store = Store::new(
        pool.clone(),
        &config.table_payouts,
        &config.table_events,
        &config.table_logs,
        &config.table_payers,
    );
    Ok((store, Notifications::new(pool)))
}

/// What startup checks in the database before serving: the tables match the schema this swisha
/// ships, columns and the unique rules the guards rely on.
///
/// # Errors
///
/// For a table that does not match, naming it.
pub async fn verify_store(store: &Store) -> Result<()> {
    store.verify_schema().await
}
