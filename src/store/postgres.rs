//! The PostgreSQL [`PayoutStore`], the backend swisha ships.

use std::sync::Arc;

use sqlx::{AssertSqlSafe, PgPool};

use crate::domain::payer::{AddPayerOutcome, KeyHolder, LogLine, NewPayer, PayerRecord};
use crate::domain::payout::{
    ACCEPTED, CheckSchedule, ClaimOutcome, ConfirmOutcome, DueCheck, EventRecord, ManualConfirmation,
    NewPayout, NewRedo, PayoutSnapshot, RedoDetails, RedoOutcome, StatusView,
};
use crate::domain::review::CONFIRMED_PAID;
use crate::domain::status;
use crate::error::{Result, err};
use crate::store::PayoutStore;

/// PostgreSQL storage.
///
/// The atomic claim is one `INSERT ... ON CONFLICT DO UPDATE ... RETURNING`, which is what makes
/// concurrent submissions of one reference produce exactly one payout.
pub struct PostgresStore {
    pool: PgPool,
    payouts: String,
    events: String,
    logs: String,
    payers: String,
    // Built once, since the table names are fixed at startup.
    claim_sql: Arc<str>,
    claim_opening_sql: Arc<str>,
}

// Every column the store reads or writes, per table, so startup can prove the tables match.
const PAYOUT_COLUMNS: &str = "payer, reference, payer_alias, payee_alias, payee_ssn, amount, message, \
                              swish_ref, callback_identifier, status, replaces, checked_at, \
                              checks_ended, created_at, updated_at";
const EVENT_COLUMNS: &str = "id, payer, reference, swish_ref, event, status, amount, payee_alias, \
                             error_code, error_message, ip, actor, note, created_at";
const LOG_COLUMNS: &str = "id, level, message, context, ip, timestamp";
const PAYER_COLUMNS: &str =
    "name, nickname, number, added_by, created_at, updated_at, disabled_at, key_hash";

// Due a check: never checked and its last change $1 seconds old, or last checked $2 seconds ago.
// Kept apart so the (payer, checked_at) index serves each: as one, they read 201,000 rows, 30 ms.
fn due() -> (String, String) {
    let unconfirmed = format!("NOT checks_ended AND status IN ({})", status::sql_list(status::UNCONFIRMED));
    (
        format!("{unconfirmed} AND checked_at IS NULL AND updated_at < NOW() - make_interval(secs => $1)"),
        format!("{unconfirmed} AND checked_at < NOW() - make_interval(secs => $2)"),
    )
}

// Payout $2 of payer $1 and every payout it redoes, one redo back at a time. UNION stops at a row
// already found, so even a loop, which no redo can make, would end.
fn chain(payouts: &str) -> String {
    format!(
        "WITH RECURSIVE chain (reference, replaces) AS (
             SELECT reference, replaces FROM {payouts} WHERE payer = $1 AND reference = $2
             UNION
             SELECT p.reference, p.replaces FROM {payouts} p
               JOIN chain c ON p.payer = $1 AND p.reference = c.replaces)"
    )
}

// The columns an event insert names, in the order bind_event, append and claim_opening bind them.
const EVENT_INSERT: &str = "payer, reference, swish_ref, event, status, amount, payee_alias, error_code, \
                            error_message, ip, actor, note";

fn bind_event<'q>(
    query: sqlx::query::QueryScalar<'q, sqlx::Postgres, bool, sqlx::postgres::PgArguments>,
    event: &'q EventRecord,
) -> sqlx::query::QueryScalar<'q, sqlx::Postgres, bool, sqlx::postgres::PgArguments> {
    query
        .bind(&event.payer)
        .bind(&event.reference)
        .bind(&event.swish_ref)
        .bind(&event.event)
        .bind(&event.status)
        .bind(event.amount)
        .bind(&event.payee_alias)
        .bind(&event.error_code)
        .bind(&event.error_message)
        .bind(&event.ip)
        .bind(&event.actor)
        .bind(&event.note)
}

impl PostgresStore {
    // Values are always bound, and table names, which come from code, are checked here as plain
    // identifiers that cannot leave an identifier position: what AssertSqlSafe asserts below.
    /// Wraps a pool and the four table names.
    ///
    /// The service always uses `swisha_payouts`, `swisha_events`, `swisha_logs` and
    /// `swisha_payers`, the tables `swisha --print-schema` creates. Other names exist for code that
    /// keeps several stores in one database, as the test suite does.
    ///
    /// # Panics
    ///
    /// When a table name is anything but ASCII letters, digits and underscores.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn run() -> swisha::error::Result<()> {
    /// use swisha::store::{PayoutStore, postgres::PostgresStore};
    ///
    /// let pool = sqlx::PgPool::connect("postgres://swisha@localhost/swisha").await?;
    /// let tables = ("swisha_payouts", "swisha_events", "swisha_logs", "swisha_payers");
    /// let store = PostgresStore::new(pool, tables.0, tables.1, tables.2, tables.3);
    /// store.verify_schema().await?;
    /// let view = store.status_view("sthlm", "INV-1042").await?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn new(pool: PgPool, payouts: &str, events: &str, logs: &str, payers: &str) -> Self {
        for name in [payouts, events, logs, payers] {
            assert!(
                !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'),
                "table name {name:?} must be ASCII letters, digits and underscores"
            );
        }
        // ON CONFLICT makes the claim atomic. The update is a deliberate no-op so RETURNING reports
        // the existing row in one round trip: an existing reference is never rewritten.
        let claim_sql = format!(
            "INSERT INTO {payouts}
                (payer, reference, payer_alias, payee_alias, payee_ssn, amount, message,
                 swish_ref, callback_identifier, status)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, 'CREATED')
             ON CONFLICT (payer, reference) DO UPDATE SET status = {payouts}.status
             RETURNING swish_ref, status"
        );
        // The same claim, and the trail's first event beside it only when this call won: one
        // statement, so a payout whose first event cannot be stored is not claimed either.
        let claim_opening_sql = format!(
            "WITH claimed AS ({claim_sql}),
             opened AS (
                 INSERT INTO {events} ({EVENT_INSERT})
                 SELECT $10, $11, $12, $13, $14, $15, $16, $17, $18, $19, $20, $21 FROM claimed
                  WHERE claimed.status = 'CREATED' AND claimed.swish_ref = $8)
             SELECT swish_ref, status FROM claimed"
        );
        Self {
            pool,
            payouts: payouts.to_string(),
            events: events.to_string(),
            logs: logs.to_string(),
            payers: payers.to_string(),
            claim_sql: claim_sql.into(),
            claim_opening_sql: claim_opening_sql.into(),
        }
    }

    /// The underlying pool, so a caller can share one connection pool with its own queries.
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    // One service log line, on whatever connection the caller writes with.
    async fn log<'e>(&self, executor: impl sqlx::PgExecutor<'e>, line: &LogLine<'_>) -> Result<()> {
        sqlx::query(AssertSqlSafe(format!(
            "INSERT INTO {} (level, message, context, ip) VALUES ($1, $2, $3, $4)",
            self.logs
        )))
        .bind(line.level)
        .bind(line.message)
        .bind(line.context)
        .bind(line.ip)
        .execute(executor)
        .await?;
        Ok(())
    }

    // A change to a number and the line that logs it, committed together: the line only beside a
    // change that applied, and the change only with its line.
    async fn logged(
        &self,
        change: sqlx::query::Query<'_, sqlx::Postgres, sqlx::postgres::PgArguments>,
        line: &LogLine<'_>,
    ) -> Result<bool> {
        let mut tx = self.pool.begin().await?;
        let applied = change.execute(&mut *tx).await?.rows_affected() > 0;
        if applied {
            self.log(&mut *tx, line).await?;
        }
        tx.commit().await?;
        Ok(applied)
    }

    // One event onto the trail, through the pool or inside a transaction.
    async fn append<'e>(&self, executor: impl sqlx::PgExecutor<'e>, event: &EventRecord) -> Result<()> {
        sqlx::query(AssertSqlSafe(format!(
            "INSERT INTO {} ({EVENT_INSERT}) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)",
            self.events
        )))
        .bind(&event.payer)
        .bind(&event.reference)
        .bind(&event.swish_ref)
        .bind(&event.event)
        .bind(&event.status)
        .bind(event.amount)
        .bind(&event.payee_alias)
        .bind(&event.error_code)
        .bind(&event.error_message)
        .bind(&event.ip)
        .bind(&event.actor)
        .bind(&event.note)
        .execute(executor)
        .await?;
        Ok(())
    }

    /// Proves each table has every column the store uses, and the unique rules the guards rely on.
    /// `CREATE TABLE IF NOT EXISTS` leaves a table from another version as it is: every payout
    /// would then fail on a missing column, or a missing rule would let a number be stored twice.
    ///
    /// # Errors
    ///
    /// Naming the first table that is missing or does not match, and the rule it lacks.
    pub async fn verify_schema(&self) -> Result<()> {
        for (table, columns) in [
            (&self.payouts, PAYOUT_COLUMNS),
            (&self.events, EVENT_COLUMNS),
            (&self.logs, LOG_COLUMNS),
            (&self.payers, PAYER_COLUMNS),
        ] {
            sqlx::query(AssertSqlSafe(format!("SELECT {columns} FROM {table} LIMIT 0")))
                .execute(&self.pool)
                .await
                .map_err(|e| {
                    err(format!(
                        "The table {table} does not match the schema this swisha uses ({e}). \
                         Compare it with: swisha --print-schema"
                    ))
                })?;
        }

        // The guards lean on these: one payout per reference, one redo per payout, and one name,
        // number and key per Swish number. A valid unique index on exactly those columns is each,
        // filtered on nothing, or for redos on replaces alone: a payout that redoes none collides
        // with nothing, while any other filter would leave some redos out of the rule.
        for (table, columns, filter) in [
            (&self.payouts, &["payer", "reference"][..], None),
            (&self.payouts, &["payer", "replaces"][..], Some("(replaces IS NOT NULL)")),
            (&self.payers, &["name"][..], None),
            (&self.payers, &["number"][..], None),
            (&self.payers, &["key_hash"][..], None),
        ] {
            let unique: bool = sqlx::query_scalar(
                "SELECT EXISTS (
                     SELECT 1 FROM pg_index i
                      WHERE i.indrelid = $1::text::regclass AND i.indisunique AND i.indisvalid
                        AND (i.indpred IS NULL OR pg_get_expr(i.indpred, i.indrelid) = $3)
                        AND (SELECT array_agg(a.attname::text ORDER BY k.n)
                               FROM unnest(i.indkey::int2[]) WITH ORDINALITY AS k(attnum, n)
                               JOIN pg_attribute a ON a.attrelid = i.indrelid AND a.attnum = k.attnum) = $2)",
            )
            .bind(table)
            .bind(columns)
            .bind(filter)
            .fetch_one(&self.pool)
            .await?;
            if !unique {
                return Err(err(format!(
                    "The table {table} has no unique index on ({}), which the double-payout guard \
                     relies on. Compare it with: swisha --print-schema",
                    columns.join(", ")
                )));
            }
        }
        Ok(())
    }
}

impl PayoutStore for PostgresStore {
    async fn claim(&self, new: &NewPayout<'_>) -> Result<ClaimOutcome> {
        let (swish_ref, status): (Option<String>, String) = sqlx::query_as(AssertSqlSafe(self.claim_sql.clone()))
            .bind(new.payer)
            .bind(new.reference)
            .bind(new.payer_alias)
            .bind(new.payee_alias)
            .bind(new.payee_ssn)
            .bind(new.amount)
            .bind(new.message)
            .bind(new.swish_ref)
            .bind(new.callback_identifier)
            .fetch_one(&self.pool)
            .await?;

        Ok(ClaimOutcome { status, swish_ref })
    }

    async fn claim_opening(&self, new: &NewPayout<'_>, opened: &EventRecord) -> Result<ClaimOutcome> {
        let (swish_ref, status): (Option<String>, String) =
            sqlx::query_as(AssertSqlSafe(self.claim_opening_sql.clone()))
                .bind(new.payer)
                .bind(new.reference)
                .bind(new.payer_alias)
                .bind(new.payee_alias)
                .bind(new.payee_ssn)
                .bind(new.amount)
                .bind(new.message)
                .bind(new.swish_ref)
                .bind(new.callback_identifier)
                .bind(&opened.payer)
                .bind(&opened.reference)
                .bind(&opened.swish_ref)
                .bind(&opened.event)
                .bind(&opened.status)
                .bind(opened.amount)
                .bind(&opened.payee_alias)
                .bind(&opened.error_code)
                .bind(&opened.error_message)
                .bind(&opened.ip)
                .bind(&opened.actor)
                .bind(&opened.note)
                .fetch_one(&self.pool)
                .await?;

        Ok(ClaimOutcome { status, swish_ref })
    }

    async fn snapshot(&self, payer: &str, reference: &str) -> Result<Option<PayoutSnapshot>> {
        type Row = (Option<String>, Option<String>, Option<String>);
        let row: Option<Row> = sqlx::query_as(AssertSqlSafe(format!(
            "SELECT status, swish_ref, callback_identifier FROM {}
             WHERE payer = $1 AND reference = $2 LIMIT 1",
            self.payouts
        )))
        .bind(payer)
        .bind(reference)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|(status, swish_ref, callback_identifier)| PayoutSnapshot {
            status,
            swish_ref,
            callback_identifier,
        }))
    }

    async fn status_view(&self, payer: &str, reference: &str) -> Result<Option<StatusView>> {
        type Row = (
            Option<String>, f64, Option<String>, Option<String>, bool, Option<String>, Option<String>, bool,
            bool, bool, bool, bool, Option<String>, Option<String>, Option<i64>,
        );
        // The payout's events are read once, into t: read once per column they cost 43 buffers a
        // call, against 16.
        let row: Option<Row> = sqlx::query_as(AssertSqlSafe(format!(
            "{chain}
             SELECT p.status, p.amount::FLOAT8, p.swish_ref, t.error_code, p.checks_ended, p.replaces,
                    (SELECT r.reference FROM {payouts} r
                      WHERE r.payer = p.payer AND r.replaces = p.reference),
                    coalesce(t.was_debited, FALSE),
                    EXISTS (SELECT 1 FROM chain c JOIN {payouts} o ON o.payer = p.payer AND o.reference = c.reference
                             WHERE c.reference <> p.reference AND o.status IN ({paid})),
                    coalesce(t.accepted, FALSE),
                    coalesce(t.send_unaccepted, FALSE),
                    coalesce(t.failed_last > t.confirmed_last, FALSE),
                    t.actor, t.note, FLOOR(EXTRACT(EPOCH FROM t.confirmed_at))::BIGINT
               FROM {payouts} p
               LEFT JOIN LATERAL (
                    SELECT (array_agg(e.error_code ORDER BY e.id DESC) FILTER (WHERE e.error_code IS NOT NULL))[1] AS error_code,
                           bool_or(e.status = 'DEBITED') AS was_debited,
                           bool_or(e.event IN ('{ACCEPTED}', 'CALLBACK') OR e.status IN ({held})) AS accepted,
                           bool_or(e.event IN ({unaccepted})) AS send_unaccepted,
                           max(e.id) FILTER (WHERE e.status IN ({failed})) AS failed_last,
                           max(e.id) FILTER (WHERE e.event = '{CONFIRMED_PAID}') AS confirmed_last,
                           (array_agg(e.actor ORDER BY e.id DESC) FILTER (WHERE e.event = '{CONFIRMED_PAID}'))[1] AS actor,
                           (array_agg(e.note ORDER BY e.id DESC) FILTER (WHERE e.event = '{CONFIRMED_PAID}'))[1] AS note,
                           (array_agg(e.created_at ORDER BY e.id DESC) FILTER (WHERE e.event = '{CONFIRMED_PAID}'))[1] AS confirmed_at
                      FROM {events} e WHERE e.payer = p.payer AND e.reference = p.reference) t ON TRUE
              WHERE p.payer = $1 AND p.reference = $2 LIMIT 1",
            chain = chain(&self.payouts),
            events = self.events,
            payouts = self.payouts,
            paid = status::sql_list(status::TERMINAL),
            held = status::sql_list(status::HELD),
            unaccepted = status::sql_list(&[status::PayoutStatus::Pending, status::PayoutStatus::Declined]),
            failed = status::sql_list(&[status::PayoutStatus::Error, status::PayoutStatus::Declined]),
        )))
        .bind(payer)
        .bind(reference)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(
            |(
                status, amount, swish_ref, error_code, checks_ended, replaces, replaced_by, was_debited,
                earlier_paid, accepted, send_unaccepted, contradicted, by, note, at,
            )| {
                StatusView {
                    status,
                    amount,
                    swish_ref,
                    error_code,
                    checks_ended,
                    replaces,
                    replaced_by,
                    was_debited,
                    earlier_paid,
                    accepted,
                    send_unaccepted,
                    contradicted,
                    manual_confirmation: match (by, note, at) {
                        (Some(by), Some(note), Some(at)) => Some(ManualConfirmation { by, note, at }),
                        _ => None,
                    },
                }
            },
        ))
    }

    async fn payers_with_due_checks(&self, schedule: &CheckSchedule) -> Result<Vec<String>> {
        let (unchecked, rechecked) = due();
        // DISTINCT hashes the payers; a UNION sorted every due payout, 133 ms at 200,000 due.
        Ok(sqlx::query_scalar(AssertSqlSafe(format!(
            "SELECT DISTINCT payer FROM (SELECT payer FROM {payouts} WHERE {unchecked}
             UNION ALL SELECT payer FROM {payouts} WHERE {rechecked}) due",
            payouts = self.payouts,
        )))
        .bind(schedule.first_after.as_secs_f64())
        .bind(schedule.every.as_secs_f64())
        .fetch_all(&self.pool)
        .await?)
    }

    async fn claim_due_checks(
        &self,
        payer: &str,
        schedule: &CheckSchedule,
        limit: usize,
    ) -> Result<Vec<DueCheck>> {
        // SKIP LOCKED keeps two instances off one payout. Each way of being due locks up to the
        // limit, the longest due of those are claimed, and the rest unlock as the statement ends.
        let (unchecked, rechecked) = due();
        let rows: Vec<(String, String, bool)> = sqlx::query_as(AssertSqlSafe(format!(
            "WITH unchecked AS (
                  SELECT payer, reference, updated_at + make_interval(secs => $1) AS due FROM {payouts}
                   WHERE payer = $5 AND {unchecked} ORDER BY updated_at LIMIT $4 FOR UPDATE SKIP LOCKED),
                  rechecked AS (
                  SELECT payer, reference, checked_at + make_interval(secs => $2) AS due FROM {payouts}
                   WHERE payer = $5 AND {rechecked} ORDER BY checked_at LIMIT $4 FOR UPDATE SKIP LOCKED),
                  claimed AS (
                  SELECT payer, reference FROM (SELECT * FROM unchecked UNION ALL SELECT * FROM rechecked) d
                   ORDER BY due LIMIT $4)
             UPDATE {payouts} p SET checked_at = NOW() FROM claimed
              WHERE p.payer = claimed.payer AND p.reference = claimed.reference
             RETURNING p.payer, p.reference, NOW() >= p.created_at + make_interval(secs => $3)",
            payouts = self.payouts,
        )))
        .bind(schedule.first_after.as_secs_f64())
        .bind(schedule.every.as_secs_f64())
        .bind(schedule.window.as_secs_f64())
        .bind(i64::try_from(limit).unwrap_or(i64::MAX))
        .bind(payer)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(payer, reference, last)| DueCheck { payer, reference, last })
            .collect())
    }

    async fn confirm_paid(&self, confirmation: &EventRecord) -> Result<ConfirmOutcome> {
        let (payer, reference) = (&confirmation.payer, &confirmation.reference);
        let mut tx = self.pool.begin().await?;

        // Lock first, then look for a redo in a statement of its own: a check made while waiting on
        // a redo's lock would read from before that redo existed, and confirm a payout paid again.
        let found: Option<(String, bool)> = sqlx::query_as(AssertSqlSafe(format!(
            "SELECT status, checks_ended FROM {} WHERE payer = $1 AND reference = $2 FOR UPDATE",
            self.payouts
        )))
        .bind(payer)
        .bind(reference)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((found, checks_ended)) = found else {
            return Ok(ConfirmOutcome::NotFound);
        };

        let redo: Option<String> = sqlx::query_scalar(AssertSqlSafe(format!(
            "SELECT reference FROM {} WHERE payer = $1 AND replaces = $2",
            self.payouts
        )))
        .bind(payer)
        .bind(reference)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(redo) = redo {
            return Ok(ConfirmOutcome::Redone(redo));
        }

        let confirmable = status::PayoutStatus::parse(&found).is_some_and(|s| {
            status::CONFIRMABLE.contains(&s) || (checks_ended && status::CONFIRMABLE_AFTER_CHECKS.contains(&s))
        });
        if !confirmable {
            return Ok(ConfirmOutcome::Refused(found));
        }
        sqlx::query(AssertSqlSafe(format!(
            "UPDATE {} SET status = 'PAID', updated_at = NOW() WHERE payer = $1 AND reference = $2",
            self.payouts
        )))
        .bind(payer)
        .bind(reference)
        .execute(&mut *tx)
        .await?;
        self.append(&mut *tx, confirmation).await?;
        tx.commit().await?;
        Ok(ConfirmOutcome::Confirmed)
    }

    async fn claim_redo(&self, redo: &NewRedo<'_>, redone: &EventRecord) -> Result<RedoOutcome> {
        let mut tx = self.pool.begin().await?;

        // The lock makes two redos of one payout take turns: the second finds the first's redo.
        type Original = (String, String, Option<String>, f64, String);
        let original: Option<Original> = sqlx::query_as(AssertSqlSafe(format!(
            "SELECT status, payee_alias, payee_ssn, amount::FLOAT8, message FROM {}
             WHERE payer = $1 AND reference = $2 FOR UPDATE",
            self.payouts
        )))
        .bind(redo.payer)
        .bind(redo.replaces)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((status, payee_alias, payee_ssn, amount, message)) = original else {
            return Ok(RedoOutcome::NotFound);
        };
        let redoable = status::PayoutStatus::parse(&status).is_some_and(|s| status::REDOABLE.contains(&s));
        if !redoable {
            return Ok(RedoOutcome::NotRedoable(status));
        }

        let earlier: Option<(String, Option<String>)> = sqlx::query_as(AssertSqlSafe(format!(
            "SELECT reference, swish_ref FROM {} WHERE payer = $1 AND replaces = $2",
            self.payouts
        )))
        .bind(redo.payer)
        .bind(redo.replaces)
        .fetch_optional(&mut *tx)
        .await?;
        // This very redo, asked for again after its answer was lost, is told by its own instruction.
        let own = match earlier {
            Some((reference, swish_ref))
                if reference == redo.reference && swish_ref.as_deref() == Some(redo.swish_ref) =>
            {
                true
            }
            Some((reference, _)) => return Ok(RedoOutcome::AlreadyRedone(reference)),
            None => false,
        };

        // A payout this one redoes may since read PAID, DEBITED or a newer swisha's status. Locked,
        // these read as last committed, and a PAID still to land waits for this redo, then sees it.
        let before: Vec<(String, String)> = sqlx::query_as(AssertSqlSafe(format!(
            "{chain}
             SELECT p.reference, p.status FROM {payouts} p
              WHERE p.payer = $1 AND p.reference IN (SELECT reference FROM chain WHERE reference <> $2)
              FOR SHARE OF p",
            chain = chain(&self.payouts),
            payouts = self.payouts
        )))
        .bind(redo.payer)
        .bind(redo.replaces)
        .fetch_all(&mut *tx)
        .await?;
        let not_failed = before.into_iter().find(|(_, status)| {
            !status::PayoutStatus::parse(status).is_some_and(|s| status::REDOABLE.contains(&s))
        });
        if let Some((reference, status)) = not_failed {
            return Ok(RedoOutcome::EarlierPaid { reference, status });
        }
        // Checked again as a new claim is, so a payout paid since stops it all the same.
        if own {
            return Ok(RedoOutcome::Claimed(RedoDetails { payee_alias, payee_ssn, amount, message }));
        }

        // The same atomic claim as a caller's payout, with what it replaces recorded beside it.
        let (swish_ref, claimed_status): (Option<String>, String) = sqlx::query_as(AssertSqlSafe(format!(
            "INSERT INTO {payouts}
                (payer, reference, payer_alias, payee_alias, payee_ssn, amount, message,
                 swish_ref, callback_identifier, status, replaces)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, 'CREATED', $10)
             ON CONFLICT (payer, reference) DO UPDATE SET status = {payouts}.status
             RETURNING swish_ref, status",
            payouts = self.payouts
        )))
        .bind(redo.payer)
        .bind(redo.reference)
        .bind(redo.payer_alias)
        .bind(&payee_alias)
        .bind(&payee_ssn)
        .bind(amount)
        .bind(&message)
        .bind(redo.swish_ref)
        .bind(redo.callback_identifier)
        .bind(redo.replaces)
        .fetch_one(&mut *tx)
        .await?;

        // A spent reference is left as it was, and so is the trail of the payout to redo.
        let claim = ClaimOutcome { status: claimed_status, swish_ref };
        if !claim.claimed_by(redo.swish_ref) {
            return Ok(RedoOutcome::ReferenceTaken(claim));
        }
        self.append(&mut *tx, redone).await?;
        tx.commit().await?;
        Ok(RedoOutcome::Claimed(RedoDetails { payee_alias, payee_ssn, amount, message }))
    }

    async fn record_event(&self, event: &EventRecord) -> Result<()> {
        self.append(&self.pool, event).await
    }

    async fn record_status(&self, status: &str, event: &EventRecord, always: bool) -> Result<bool> {
        // One statement, so the status and its event commit together or not at all.
        let sql = format!(
            "WITH written AS (
                 UPDATE {payouts} SET status = $13, updated_at = NOW()
                  WHERE payer = $1 AND reference = $2 AND {writable}
                 RETURNING 1
             ), recorded AS (
                 INSERT INTO {events} ({EVENT_INSERT})
                 SELECT $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12
                  WHERE $14 OR EXISTS (SELECT 1 FROM written)
             )
             SELECT EXISTS (SELECT 1 FROM written)",
            payouts = self.payouts,
            events = self.events,
            writable = status::writable_condition("$13"),
        );
        let query = bind_event(sqlx::query_scalar(AssertSqlSafe(sql)), event);
        Ok(query.bind(status).bind(always).fetch_one(&self.pool).await?)
    }

    async fn end_checks(&self, status: &str, event: &EventRecord) -> Result<bool> {
        let sql = format!(
            "WITH ended AS (
                 UPDATE {payouts} SET checks_ended = TRUE
                  WHERE payer = $1 AND reference = $2 AND status = $13 AND NOT checks_ended
                 RETURNING 1
             ), recorded AS (
                 INSERT INTO {events} ({EVENT_INSERT})
                 SELECT $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12
                  WHERE EXISTS (SELECT 1 FROM ended)
             )
             SELECT EXISTS (SELECT 1 FROM ended)",
            payouts = self.payouts,
            events = self.events,
        );
        let query = bind_event(sqlx::query_scalar(AssertSqlSafe(sql)), event);
        Ok(query.bind(status).fetch_one(&self.pool).await?)
    }

    async fn record_log(
        &self,
        level: &str,
        message: &str,
        context: Option<&str>,
        ip: Option<&str>,
    ) -> Result<()> {
        self.log(&self.pool, &LogLine { level, message, context, ip }).await
    }

    async fn paid_twice(&self) -> Result<i64> {
        // From the redos, through the partial index: 7 ms at a million payouts, against 150 ms
        // from the payouts paid, which hash-joins every one of them.
        Ok(sqlx::query_scalar(AssertSqlSafe(format!(
            "SELECT count(*) FROM {payouts} r JOIN {payouts} p ON p.payer = r.payer AND p.reference = r.replaces
              WHERE r.replaces IS NOT NULL AND p.status IN ('PAID', 'DEBITED')",
            payouts = self.payouts,
        )))
        .fetch_one(&self.pool)
        .await?)
    }

    async fn payers(&self) -> Result<Vec<PayerRecord>> {
        type Row = (String, String, String, String, i64, i64, bool);
        let rows: Vec<Row> = sqlx::query_as(AssertSqlSafe(format!(
            "SELECT name, nickname, number, added_by, FLOOR(EXTRACT(EPOCH FROM created_at))::BIGINT,
                    (EXTRACT(EPOCH FROM updated_at) * 1000000)::BIGINT, disabled_at IS NOT NULL
               FROM {} ORDER BY name",
            self.payers
        )))
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(name, nickname, number, added_by, created_at, updated_at, disabled)| PayerRecord {
                name,
                nickname,
                number,
                added_by,
                created_at,
                updated_at,
                disabled,
            })
            .collect())
    }

    async fn add_payer(&self, new: &NewPayer<'_>, logged: &LogLine<'_>) -> Result<AddPayerOutcome> {
        let mut tx = self.pool.begin().await?;

        let paid_as: Option<String> = sqlx::query_scalar(AssertSqlSafe(format!(
            "SELECT payer FROM {} WHERE payer_alias = $1 AND payer <> $2 LIMIT 1",
            self.payouts
        )))
        .bind(new.number)
        .bind(new.name)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(other) = paid_as {
            return Ok(AddPayerOutcome::NumberPaidAs(other));
        }

        // Either unique key refuses the insert. A concurrent add of the same name or number waits
        // for the first to commit, then finds its row below.
        let added: Option<String> = sqlx::query_scalar(AssertSqlSafe(format!(
            "INSERT INTO {} (name, nickname, number, added_by, key_hash) VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT DO NOTHING RETURNING name",
            self.payers
        )))
        .bind(new.name)
        .bind(new.nickname)
        .bind(new.number)
        .bind(new.added_by)
        .bind(new.key_hash)
        .fetch_optional(&mut *tx)
        .await?;
        if added.is_some() {
            self.log(&mut *tx, logged).await?;
            tx.commit().await?;
            return Ok(AddPayerOutcome::Added);
        }

        let holder: Option<String> = sqlx::query_scalar(AssertSqlSafe(format!(
            "SELECT name FROM {} WHERE name = $1 OR number = $2 ORDER BY name = $1 DESC LIMIT 1",
            self.payers
        )))
        .bind(new.name)
        .bind(new.number)
        .fetch_optional(&mut *tx)
        .await?;
        match holder {
            Some(holder) if holder == new.name => Ok(AddPayerOutcome::NameTaken),
            Some(holder) => Ok(AddPayerOutcome::NumberTaken(holder)),
            None => Err(err("the payer was neither stored nor found under its name or number")),
        }
    }

    async fn set_payer_nickname(&self, name: &str, nickname: &str, logged: &LogLine<'_>) -> Result<bool> {
        let change = sqlx::query(AssertSqlSafe(format!(
            "UPDATE {} SET nickname = $2, updated_at = NOW() WHERE name = $1",
            self.payers
        )))
        .bind(name)
        .bind(nickname);
        self.logged(change, logged).await
    }

    async fn set_payer_disabled(&self, name: &str, disabled: bool, logged: &LogLine<'_>) -> Result<bool> {
        let change = sqlx::query(AssertSqlSafe(format!(
            "UPDATE {} SET disabled_at = CASE WHEN $2 THEN COALESCE(disabled_at, NOW()) END,
                           updated_at = NOW()
              WHERE name = $1",
            self.payers
        )))
        .bind(name)
        .bind(disabled);
        self.logged(change, logged).await
    }

    async fn touch_payer(&self, name: &str, logged: &LogLine<'_>) -> Result<bool> {
        let change = sqlx::query(AssertSqlSafe(format!(
            "UPDATE {} SET updated_at = NOW() WHERE name = $1",
            self.payers
        )))
        .bind(name);
        self.logged(change, logged).await
    }

    async fn payer_by_key(&self, key_hash: &str) -> Result<Option<KeyHolder>> {
        let row: Option<(String, bool)> = sqlx::query_as(AssertSqlSafe(format!(
            "SELECT name, disabled_at IS NOT NULL FROM {} WHERE key_hash = $1",
            self.payers
        )))
        .bind(key_hash)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|(name, disabled)| KeyHolder { name, disabled }))
    }

    async fn key_holders(&self, key_hashes: &[String]) -> Result<Vec<(String, String)>> {
        Ok(sqlx::query_as(AssertSqlSafe(format!(
            "SELECT key_hash, name FROM {} WHERE key_hash = ANY($1)",
            self.payers
        )))
        .bind(key_hashes)
        .fetch_all(&self.pool)
        .await?)
    }

    // The key has nothing to reload, so updated_at stays where it is.
    async fn replace_payer_key(
        &self,
        name: &str,
        current: Option<&str>,
        new: &str,
        logged: &LogLine<'_>,
    ) -> Result<bool> {
        let change = sqlx::query(AssertSqlSafe(format!(
            "UPDATE {} SET key_hash = $3 WHERE name = $1 AND ($2::TEXT IS NULL OR key_hash = $2)",
            self.payers
        )))
        .bind(name)
        .bind(current)
        .bind(new);
        self.logged(change, logged).await
    }
}
