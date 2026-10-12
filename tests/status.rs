use swisha::domain::status::{
    ANSWERS, NO_ANSWER, PayoutStatus, TERMINAL, UNANSWERED, UNCONFIRMED, sql_list,
};

// These strings are the exact guard lists the SQL is built from. If any of these assertions
// changes, a payment guard changed with it.
#[test]
fn sets_render_to_the_expected_sql_lists() {
    assert_eq!(sql_list(TERMINAL), "'PAID', 'DEBITED'");
    assert_eq!(sql_list(UNCONFIRMED), "'CREATED', 'PENDING', 'ERROR', 'DEBITED'");
    assert_eq!(sql_list(ANSWERS), "'PAID', 'DEBITED', 'DECLINED', 'ERROR'");
    assert_eq!(sql_list(NO_ANSWER), "'PENDING', 'NEEDS_REVIEW'");
    assert_eq!(sql_list(UNANSWERED), "'CREATED', 'PENDING'");
}

// PAID is Swish's last word, so the sweep never asks about it again. DEBITED is not: the sweep
// keeps asking until Swish says PAID or ERROR, since either can follow.
#[test]
fn only_paid_ends_the_checks_on_its_own() {
    assert!(!UNCONFIRMED.contains(&PayoutStatus::Paid));
    assert!(UNCONFIRMED.contains(&PayoutStatus::Debited));
    assert!(!UNCONFIRMED.contains(&PayoutStatus::Declined), "nothing was stored to ask about");
}

// swisha stops chasing a NEEDS_REVIEW payout, but it is not a settled one. A repeat request is
// still refused, as for every status, which the conformance suite holds.
#[test]
fn needs_review_stops_the_sweep_without_freezing_the_payout() {
    let status = PayoutStatus::NeedsReview;

    assert!(
        !TERMINAL.contains(&status),
        "a genuine late PAID from Swish must still be able to settle the payout"
    );
    assert!(
        !UNCONFIRMED.contains(&status),
        "the sweep must not pick it up again once a person has been handed the payout"
    );
}

// The moves out of a terminal status, and their shape in SQL. Swish sends DEBITED and PAID as two
// callbacks seconds apart, and a late failure as DEBITED then ERROR.
#[test]
fn debited_advances_to_paid_or_error_and_nothing_goes_back() {
    use swisha::domain::status::{TERMINAL_ADVANCE, writable_condition};

    let (from, to) = TERMINAL_ADVANCE;
    assert_eq!(from, PayoutStatus::Debited);
    assert_eq!(to, [PayoutStatus::Paid, PayoutStatus::Error]);

    let sql = writable_condition("$1");
    assert_eq!(
        sql,
        "(status NOT IN ('PAID', 'DEBITED') OR (status = 'DEBITED' AND $1 IN ('PAID', 'ERROR'))) \
         AND ($1 <> 'DEBITED' OR status NOT IN ('PAID', 'ERROR')) \
         AND ($1 NOT IN ('PENDING', 'NEEDS_REVIEW') OR status IN ('CREATED', 'PENDING'))"
    );

    // Nothing in the condition lets PAID move at all.
    assert!(!sql.contains("status = 'PAID' AND"), "PAID must not be a source: {sql}");
}

#[test]
fn every_status_round_trips() {
    for status in [
        PayoutStatus::Created,
        PayoutStatus::Pending,
        PayoutStatus::Debited,
        PayoutStatus::Paid,
        PayoutStatus::Declined,
        PayoutStatus::Error,
        PayoutStatus::NeedsReview,
    ] {
        assert_eq!(PayoutStatus::parse(status.as_str()), Some(status));
        assert_eq!(status.to_string(), status.as_str());
    }
    assert_eq!(PayoutStatus::parse("NOPE"), None);
    assert_eq!(PayoutStatus::parse("paid"), None); // case sensitive, Swish sends uppercase
}

#[test]
fn the_terminal_predicate_matches_the_set() {
    assert!(PayoutStatus::Paid.is_terminal());
    assert!(PayoutStatus::Debited.is_terminal());
    assert!(!PayoutStatus::NeedsReview.is_terminal());
    assert!(!PayoutStatus::Declined.is_terminal());
    assert!(!PayoutStatus::Pending.is_terminal());
    assert!(!PayoutStatus::Created.is_terminal());
    assert!(!PayoutStatus::Error.is_terminal());
}

// The checks index covers exactly the payouts the checks ask about. Written into the schema as
// text, it would quietly stop serving the query if the set changed without it.
#[test]
fn the_checks_index_covers_exactly_the_unconfirmed_statuses() {
    let schema = include_str!("../schema/postgres.sql");
    let index = schema
        .split(';')
        .find(|statement| statement.contains("idx_swisha_payouts_due") && statement.contains("CREATE INDEX"))
        .expect("the checks index");
    assert!(index.contains(&format!("WHERE status IN ({})", sql_list(UNCONFIRMED))), "{index}");
}
