// The conformance suite is the contract, executed. Any adapter that passes it is supported.
//
// Standalone on purpose: it builds its own store rather than going through tests/common, so it
// proves the contract against a bare database rather than against the test harness.

use sqlx::AssertSqlSafe;

#[tokio::test]
async fn postgres_passes_the_conformance_suite() {
    let url = std::env::var("SWISHA_TEST_DATABASE_URL").unwrap_or_else(|_| {
        panic!(
            "\n\nSWISHA_TEST_DATABASE_URL is not set, and the conformance suite needs a \
             database.\n\n  docker run -d --name swisha-pg -p 5433:5432 \\\n    \
             -e POSTGRES_USER=swisha -e POSTGRES_PASSWORD=swisha \\\n    \
             -e POSTGRES_DB=swisha_test postgres:18\n\n  \
             export SWISHA_TEST_DATABASE_URL=postgres://swisha:swisha@localhost:5433/swisha_test\n"
        )
    });

    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(8)
        .connect(&url)
        .await
        .expect("connect to the test database");

    // Own tables, so a repeated run cannot see the last one's rows and a bare database works.
    let prefix = format!("c{}", swisha::swish::random_payout_uuid()[..12].to_lowercase());
    let ddl = include_str!("../schema/postgres.sql")
        .lines()
        .filter(|line| !line.trim_start().starts_with("--"))
        .collect::<Vec<_>>()
        .join("\n")
        .replace("swisha_", &format!("{prefix}_"));

    for statement in statements(&ddl) {
        sqlx::query(AssertSqlSafe(statement.clone()))
            .execute(&pool)
            .await
            .unwrap_or_else(|e| panic!("schema statement failed: {e}\n{statement}"));
    }

    let store = swisha::store::postgres::PostgresStore::new(
        pool.clone(),
        &format!("{prefix}_payouts"),
        &format!("{prefix}_events"),
        &format!("{prefix}_logs"),
        &format!("{prefix}_payers"),
    );

    // Short enough that the longest suffix still fits a 35-character reference and a 20-character
    // number.
    let passed = swisha::store::conformance::run(&store, "conf")
        .await
        .expect("conformance suite");

    assert_eq!(passed.len(), 25, "checks that ran: {passed:?}");

    for table in ["payouts", "events", "logs", "payers"] {
        let _ = sqlx::query(AssertSqlSafe(format!("DROP TABLE IF EXISTS {prefix}_{table} CASCADE")))
            .execute(&pool)
            .await;
    }
}

// The schema's statements, split at each `;` outside a `$$` body, as psql splits them.
fn statements(ddl: &str) -> Vec<String> {
    let mut statements = vec![String::new()];
    for (i, part) in ddl.split("$$").enumerate() {
        let current = statements.last_mut().expect("never empty");
        if i % 2 == 1 {
            current.push_str(&format!("$${part}$$"));
            continue;
        }
        let mut pieces = part.split(';');
        current.push_str(pieces.next().unwrap_or_default());
        statements.extend(pieces.map(str::to_string));
    }
    statements.into_iter().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()
}
