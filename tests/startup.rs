// What swisha checks in the database before it serves: that the tables are the ones this version
// expects.

mod common;

use sqlx::AssertSqlSafe;

#[tokio::test]
async fn a_fresh_database_passes() {
    let state = common::state_with(common::config()).await;
    swisha::backend::verify_store(&state.store).await.expect("fresh tables");
}

// Tables made by another version are kept as they are by CREATE TABLE IF NOT EXISTS, and every
// payout would then fail on a missing column. Startup names the table instead.
#[tokio::test]
async fn tables_that_do_not_match_the_schema_stop_startup() {
    for (table, column) in [
        ("payouts", "callback_identifier"),
        ("events", "payer"),
        ("logs", "context"),
        ("payers", "nickname"),
    ] {
        let config = common::config();
        let (state, pool) = common::state_and_pool(config.clone()).await;
        let name = match table {
            "payouts" => &config.table_payouts,
            "events" => &config.table_events,
            "payers" => &config.table_payers,
            _ => &config.table_logs,
        };
        sqlx::query(AssertSqlSafe(format!("ALTER TABLE {name} DROP COLUMN {column} CASCADE")))
            .execute(&pool)
            .await
            .expect("alter");
        let error = swisha::backend::verify_store(&state.store)
            .await
            .expect_err(column)
            .to_string();
        assert!(error.contains(name.as_str()) && error.contains(column) && error.contains("--print-schema"), "{error}");
    }
}

// The guards lean on unique rules, not just columns: one payout per reference, one redo per payout,
// one name, one number and one key per Swish number. A table without one stops startup, named.
#[tokio::test]
async fn tables_without_the_unique_rules_the_guards_need_stop_startup() {
    for (table, columns) in [
        ("payouts", "payer, reference"),
        ("payouts", "payer, replaces"),
        ("payers", "name"),
        ("payers", "number"),
        ("payers", "key_hash"),
    ] {
        let config = common::config();
        let (state, pool) = common::state_and_pool(config.clone()).await;
        let name = if table == "payouts" { &config.table_payouts } else { &config.table_payers };
        let dropped = match columns {
            "payer, reference" | "name" => format!("ALTER TABLE {name} DROP CONSTRAINT {name}_pkey"),
            "payer, replaces" => format!("DROP INDEX idx_{name}_redo"),
            single => format!("ALTER TABLE {name} DROP CONSTRAINT {name}_{single}_key"),
        };
        sqlx::query(AssertSqlSafe(dropped)).execute(&pool).await.expect("drop");
        let error = swisha::backend::verify_store(&state.store).await.expect_err(columns).to_string();
        assert!(error.contains(name.as_str()) && error.contains(columns), "{columns}: {error}");
    }
}

// The one-redo rule may leave out payouts that redo nothing, which collide with nothing, and no
// others: a rule filtered on anything else would let a payout it leaves out be redone twice.
#[tokio::test]
async fn a_redo_rule_filtered_on_anything_but_replaces_stops_startup() {
    let config = common::config();
    let (state, pool) = common::state_and_pool(config.clone()).await;
    let name = &config.table_payouts;
    swisha::backend::verify_store(&state.store).await.expect("the shipped schema");
    let rebuild = |filter: &str| {
        let (pool, name) = (pool.clone(), name.clone());
        let filter = filter.to_string();
        async move {
            sqlx::query(AssertSqlSafe(format!("DROP INDEX idx_{name}_redo"))).execute(&pool).await.expect("drop");
            sqlx::query(AssertSqlSafe(format!("CREATE UNIQUE INDEX idx_{name}_redo ON {name} (payer, replaces) {filter}")))
                .execute(&pool)
                .await
                .expect("create");
        }
    };

    rebuild("WHERE status <> 'PAID'").await;
    let error = swisha::backend::verify_store(&state.store).await.expect_err("filtered on status").to_string();
    assert!(error.contains("payer, replaces"), "{error}");
    rebuild("").await;
    swisha::backend::verify_store(&state.store).await.expect("unfiltered, as before the upgrade");
}

// Running the schema again ends no check: a last check claimed and cut off before its answer was
// recorded stays due. Only a database from before checks_ended has its finished payouts marked.
#[tokio::test]
async fn running_the_schema_again_ends_no_check() {
    use swisha::store::PayoutStore;
    let config = common::config();
    let (state, pool) = common::state_and_pool(config.clone()).await;
    let table = &config.table_payouts;
    let seed = format!(
        "INSERT INTO {table} (payer, reference, payer_alias, payee_alias, amount, message, swish_ref, status,
                              checked_at, created_at)
         VALUES ($1, 'CUT-1', '1231111111', '46701234567', 100, 'm', 'A1', 'DEBITED', NOW(), NOW() - interval '49 hours'),
                ($1, 'YOUNG-1', '1231111111', '46701234567', 100, 'm', 'A2', 'DEBITED', NOW(), NOW() - interval '1 hour')"
    );
    sqlx::query(AssertSqlSafe(seed)).bind(common::PAYER).execute(&pool).await.expect("seed");

    common::apply_schema(&pool, &config).await;
    assert_eq!(checks_ended(&pool, table).await, [("CUT-1".into(), false), ("YOUNG-1".into(), false)]);
    let now = swisha::domain::payout::CheckSchedule {
        first_after: std::time::Duration::ZERO,
        every: std::time::Duration::ZERO,
        window: swisha::swish::reconcile::CHECKS.window,
    };
    let due = state.store.claim_due_checks(common::PAYER, &now, 5).await.expect("claim");
    assert!(due.iter().any(|check| check.reference == "CUT-1" && check.last), "its last check is still due");

    sqlx::query(AssertSqlSafe(format!("ALTER TABLE {table} DROP COLUMN checks_ended CASCADE")))
        .execute(&pool)
        .await
        .expect("a table from before checks_ended");
    common::apply_schema(&pool, &config).await;
    assert_eq!(checks_ended(&pool, table).await, [("CUT-1".into(), true), ("YOUNG-1".into(), false)]);
}

// Running the schema again on a database that has everything takes no lock on any swisha table:
// an ALTER TABLE locks before it looks, and live payouts would queue behind it.
#[tokio::test]
async fn running_the_schema_again_takes_no_lock() {
    let config = common::config();
    let (_state, pool) = common::state_and_pool(config.clone()).await;
    let mut run = pool.begin().await.expect("begin");
    for statement in common::schema_statements(&config) {
        sqlx::query(AssertSqlSafe(statement)).execute(&mut *run).await.expect("statement");
    }
    let tables = [&config.table_payouts, &config.table_events, &config.table_logs, &config.table_payers];
    let held: Vec<(String, String)> = sqlx::query_as(
        "SELECT c.relname::text, l.mode FROM pg_locks l JOIN pg_class c ON c.oid = l.relation
          WHERE l.pid = pg_backend_pid() AND c.relname = ANY($1) ORDER BY 1, 2",
    )
    .bind(tables.map(|t| t.to_string()).to_vec())
    .fetch_all(&mut *run)
    .await
    .expect("locks");
    run.rollback().await.expect("rollback");
    assert!(held.is_empty(), "{held:?}");
}

// A schema step that cannot get its lock gives up, and the one-redo rule survives it: on a database
// whose rule is the older index, that index stays until the new one is made.
#[tokio::test]
async fn a_schema_step_that_times_out_leaves_the_redo_rule_in_place() {
    let config = common::config();
    let (state, pool) = common::state_and_pool(config.clone()).await;
    let table = &config.table_payouts;
    for statement in [
        format!("DROP INDEX idx_{table}_redo"),
        format!("CREATE UNIQUE INDEX idx_{table}_replaces ON {table} (payer, replaces)"),
    ] {
        sqlx::query(AssertSqlSafe(statement)).execute(&pool).await.expect("an older rule");
    }

    // A payout write holds its lock past the schema's 5 seconds.
    let mut writing = pool.begin().await.expect("begin");
    sqlx::query(AssertSqlSafe(format!(
        "INSERT INTO {table} (payer, reference, payer_alias, payee_alias, amount, message, status)
         VALUES ($1, 'HELD-1', '1231111111', '46701234567', 100, 'm', 'CREATED')"
    )))
    .bind(common::PAYER)
    .execute(&mut *writing)
    .await
    .expect("write");
    let release = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(7)).await;
        writing.commit().await.expect("commit");
    });

    let mut failed = Vec::new();
    for statement in common::schema_statements(&config) {
        if let Err(e) = sqlx::query(AssertSqlSafe(statement)).execute(&pool).await {
            failed.push(e.to_string());
        }
    }
    release.await.expect("join");
    assert!(failed.iter().any(|e| e.contains("lock timeout")), "the redo index waited: {failed:?}");

    let rules: Vec<String> = sqlx::query_scalar(
        "SELECT indexname::text FROM pg_indexes WHERE tablename = $1 AND indexdef LIKE 'CREATE UNIQUE INDEX%(payer, replaces)%'",
    )
    .bind(table)
    .fetch_all(&pool)
    .await
    .expect("indexes");
    assert_eq!(rules, [format!("idx_{table}_replaces")], "the older rule stays");
    swisha::backend::verify_store(&state.store).await.expect("startup still finds a redo rule");

    common::apply_schema(&pool, &config).await;
    let verified = swisha::backend::verify_store(&state.store).await;
    assert!(verified.is_ok(), "a later run finishes it: {verified:?}");
}

async fn checks_ended(pool: &sqlx::PgPool, table: &str) -> Vec<(String, bool)> {
    let query = format!("SELECT reference, checks_ended FROM {table} ORDER BY reference");
    sqlx::query_as(AssertSqlSafe(query)).fetch_all(pool).await.expect("read")
}

// Table names are written into statements, so the store refuses anything but a plain identifier.
#[tokio::test]
#[should_panic(expected = "must be ASCII letters, digits and underscores")]
async fn a_table_name_that_is_not_a_plain_identifier_is_refused() {
    let pool = sqlx::postgres::PgPoolOptions::new().connect_lazy(&common::database_url()).expect("pool");
    swisha::store::postgres::PostgresStore::new(pool, "payouts; DROP TABLE x", "events", "logs", "payers");
}

// The settings that reach the test database, whatever the test URL names: its host, port,
// database, user and password.
fn connecting() -> swisha::config::Config {
    let url = reqwest::Url::parse(&common::database_url()).expect("the test database URL");
    let mut config = common::config();
    let host = url.host_str().expect("host").replace("localhost", "127.0.0.1");
    config.db_host = format!("{host}:{}", url.port().unwrap_or(5432));
    config.db_name = url.path().trim_start_matches('/').to_string();
    config.db_user = url.username().to_string();
    config.db_pass = url.password().unwrap_or_default().into();
    config
}

// DB_SSLMODE reaches the connection: require refuses a server that offers no TLS rather than
// connecting in plaintext without a word.
#[tokio::test]
async fn db_sslmode_require_refuses_a_plaintext_server() {
    let mut config = connecting();
    config.db_tls = swisha::config::DbTls::Require;
    let error = swisha::backend::connect(&config).await.err().expect("refused").to_string();
    assert!(error.to_lowercase().contains("tls"), "{error}");

    config.db_tls = swisha::config::DbTls::Prefer;
    assert!(swisha::backend::connect(&config).await.is_ok(), "prefer still connects on this host");
}

// DB_POOL_SIZE reaches the pool: 10 connections carried about 400 payouts a second, and a larger
// pool is how one instance takes more.
#[tokio::test]
async fn db_pool_size_reaches_the_pool() {
    for size in [1, 32] {
        let mut config = connecting();
        config.db_pool_size = size;
        let (store, _) = swisha::backend::connect(&config).await.expect("connect");
        assert_eq!(store.pool().options().get_max_connections(), size);
    }
}

// Every connection carries a 10-second statement timeout, so no statement stuck behind a lock can
// hold a payout, and the connection it took, for longer.
#[tokio::test]
async fn every_connection_carries_the_statement_timeout() {
    let (store, _) = swisha::backend::connect(&connecting()).await.expect("connect");
    let timeout: String = sqlx::query_scalar("SHOW statement_timeout").fetch_one(store.pool()).await.expect("show");
    assert_eq!(timeout, "10s");
}
