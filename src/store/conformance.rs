//! The storage contract as executable checks.

use std::time::Duration;

use crate::domain::payer::{AddPayerOutcome, KeyHolder, LogLine, NewPayer};
use crate::domain::payout::{
    CheckSchedule, ConfirmOutcome, EventRecord, ManualConfirmation, NewPayout, NewRedo, RedoOutcome,
};
use crate::error::{Result, err};
use crate::store::PayoutStore;

/// Runs every check against one store, returning the names of those that passed.
///
/// Written against the trait alone, so it states the guarantees rather than how PostgreSQL keeps
/// them. `prefix` namespaces the references and numbers the checks create, so repeated runs stay
/// independent. Keep it short and unique per run: each check appends a suffix, a reference holds
/// at most 35 characters, and a number 20.
///
/// # Errors
///
/// The first guarantee the store breaks, named, or the store's own error.
///
/// # Examples
///
/// ```no_run
/// # async fn run(pool: sqlx::PgPool) -> swisha::error::Result<()> {
/// use swisha::store::postgres::PostgresStore;
///
/// let tables = ("swisha_payouts", "swisha_events", "swisha_logs", "swisha_payers");
/// let store = PostgresStore::new(pool, tables.0, tables.1, tables.2, tables.3);
/// let passed = swisha::store::conformance::run(&store, "conf").await?;
/// println!("{} checks passed", passed.len());
/// # Ok(())
/// # }
/// ```
pub async fn run<S: PayoutStore>(store: &S, prefix: &str) -> Result<Vec<&'static str>> {
    let mut passed = Vec::new();

    claim_is_idempotent(store, prefix).await?;
    passed.push("claim_is_idempotent");

    a_claim_opens_its_trail_only_when_won(store, prefix).await?;
    passed.push("a_claim_opens_its_trail_only_when_won");

    concurrent_claims_yield_one_winner(store, prefix).await?;
    passed.push("concurrent_claims_yield_one_winner");

    the_same_reference_under_two_payers_is_two_payouts(store, prefix).await?;
    passed.push("the_same_reference_under_two_payers_is_two_payouts");

    a_claim_never_rewrites_an_existing_reference(store, prefix).await?;
    passed.push("a_claim_never_rewrites_an_existing_reference");

    terminal_status_is_never_overwritten(store, prefix).await?;
    passed.push("terminal_status_is_never_overwritten");

    debited_advances_to_paid_or_error_and_never_back(store, prefix).await?;
    passed.push("debited_advances_to_paid_or_error_and_never_back");

    no_answer_never_replaces_an_answer(store, prefix).await?;
    passed.push("no_answer_never_replaces_an_answer");

    checks_claim_only_unconfirmed_payouts(store, prefix).await?;
    passed.push("checks_claim_only_unconfirmed_payouts");

    checks_follow_the_schedule_and_end_once(store, prefix).await?;
    passed.push("checks_follow_the_schedule_and_end_once");

    a_status_and_its_event_are_written_together(store, prefix).await?;
    passed.push("a_status_and_its_event_are_written_together");

    the_checks_end_once_on_the_status_held(store, prefix).await?;
    passed.push("the_checks_end_once_on_the_status_held");

    concurrent_checks_never_claim_a_payout_twice(store, prefix).await?;
    passed.push("concurrent_checks_never_claim_a_payout_twice");

    checks_are_claimed_one_payer_at_a_time(store, prefix).await?;
    passed.push("checks_are_claimed_one_payer_at_a_time");

    confirm_paid_only_where_swisha_has_stopped_asking(store, prefix).await?;
    passed.push("confirm_paid_only_where_swisha_has_stopped_asking");

    a_redo_is_claimed_once_and_never_where_money_left(store, prefix).await?;
    passed.push("a_redo_is_claimed_once_and_never_where_money_left");

    nothing_is_redone_once_a_payout_before_it_was_paid(store, prefix).await?;
    passed.push("nothing_is_redone_once_a_payout_before_it_was_paid");

    the_status_view_shows_what_a_person_needs(store, prefix).await?;
    passed.push("the_status_view_shows_what_a_person_needs");

    events_round_trip(store, prefix).await?;
    passed.push("events_round_trip");

    a_payer_name_and_number_are_each_stored_once(store, prefix).await?;
    passed.push("a_payer_name_and_number_are_each_stored_once");

    a_number_paid_under_one_name_is_never_added_under_another(store, prefix).await?;
    passed.push("a_number_paid_under_one_name_is_never_added_under_another");

    concurrent_adds_of_one_payer_store_one(store, prefix).await?;
    passed.push("concurrent_adds_of_one_payer_store_one");

    payer_changes_are_kept_and_move_updated_at(store, prefix).await?;
    passed.push("payer_changes_are_kept_and_move_updated_at");

    a_key_finds_its_payer_until_it_is_replaced(store, prefix).await?;
    passed.push("a_key_finds_its_payer_until_it_is_replaced");

    keys_are_checked_together(store, prefix).await?;
    passed.push("keys_are_checked_together");

    Ok(passed)
}

// Two payers, so the checks can prove that one payer's payouts never touch the other's.
const PAYER: &str = "conf";
const OTHER_PAYER: &str = "conf-other";

fn payout<'a>(reference: &'a str, swish_ref: &'a str) -> NewPayout<'a> {
    payout_of(PAYER, reference, swish_ref)
}

fn payout_of<'a>(payer: &'a str, reference: &'a str, swish_ref: &'a str) -> NewPayout<'a> {
    NewPayout {
        payer,
        reference,
        payer_alias: "1234679304",
        payee_alias: "46701234567",
        payee_ssn: None,
        amount: 100.0,
        message: "conformance",
        swish_ref,
        callback_identifier: None,
    }
}

fn check(condition: bool, what: &str) -> Result<()> {
    if condition {
        Ok(())
    } else {
        Err(err(format!("conformance: {what}")))
    }
}

// A second claim for a reference already in flight must not take ownership.
async fn claim_is_idempotent<S: PayoutStore>(store: &S, prefix: &str) -> Result<()> {
    let reference = format!("{prefix}-idem");
    let first = store.claim(&payout(&reference, "AAAA0001")).await?;
    check(first.claimed_by("AAAA0001"), "first claim should win")?;

    let second = store.claim(&payout(&reference, "BBBB0002")).await?;
    check(!second.claimed_by("BBBB0002"), "second claim must not win")?;
    check(
        second.swish_ref.as_deref() == Some("AAAA0001"),
        "the original swish_ref must survive a second claim",
    )
}

// The trail's first event lands beside the claim that won, never beside a repeat or a race's
// loser. Each opening carries its own code, so the view's latest code says whose was written.
async fn a_claim_opens_its_trail_only_when_won<S: PayoutStore>(store: &S, prefix: &str) -> Result<()> {
    let opened = |reference: &str, code: &str| answer(reference, "INITIATED", "CREATED", Some(code));

    let reference = format!("{prefix}-open");
    let first = store.claim_opening(&payout(&reference, "OPEN0001"), &opened(&reference, "OPEN1")).await?;
    check(first.claimed_by("OPEN0001"), "the first claim should win")?;
    check(latest_code(store, &reference).await?.as_deref() == Some("OPEN1"), "a won claim opens its trail")?;
    let repeat = store.claim_opening(&payout(&reference, "OPEN0002"), &opened(&reference, "OPEN2")).await?;
    check(!repeat.claimed_by("OPEN0002"), "a repeat claim must not win")?;
    check(latest_code(store, &reference).await?.as_deref() == Some("OPEN1"), "a repeat claim must write no event")?;

    let raced = format!("{prefix}-open-race");
    let (a, b) = (payout(&raced, "OPEN0003"), payout(&raced, "OPEN0004"));
    let (ea, eb) = (opened(&raced, "OPEN3"), opened(&raced, "OPEN4"));
    let (one, two) = tokio::join!(store.claim_opening(&a, &ea), store.claim_opening(&b, &eb));
    let (one, two) = (one?.claimed_by("OPEN0003"), two?.claimed_by("OPEN0004"));
    check(one != two, "exactly one concurrent claim must win")?;
    let winner = if one { "OPEN3" } else { "OPEN4" };
    check(latest_code(store, &raced).await?.as_deref() == Some(winner), "the race's loser must write no event")
}

async fn latest_code<S: PayoutStore>(store: &S, reference: &str) -> Result<Option<String>> {
    Ok(store.status_view(PAYER, reference).await?.and_then(|view| view.error_code))
}

// The double-payout guard. Two callers racing on one reference must produce one submission.
async fn concurrent_claims_yield_one_winner<S: PayoutStore>(store: &S, prefix: &str) -> Result<()> {
    let reference = format!("{prefix}-race");
    let first = payout(&reference, "CCCC0001");
    let second = payout(&reference, "DDDD0002");
    let (a, b) = tokio::join!(store.claim(&first), store.claim(&second));
    let winners = [a?.claimed_by("CCCC0001"), b?.claimed_by("DDDD0002")]
        .into_iter()
        .filter(|won| *won)
        .count();
    check(winners == 1, "exactly one concurrent claim must win")
}

// References are unique per payer, not across payers: INV-1042 in two cities is two payouts,
// each claimed, settled and refused a second time on its own.
async fn the_same_reference_under_two_payers_is_two_payouts<S: PayoutStore>(
    store: &S,
    prefix: &str,
) -> Result<()> {
    let reference = format!("{prefix}-shared");
    let other = |swish_ref| NewPayout {
        payer: OTHER_PAYER,
        payer_alias: "1239999999",
        ..payout(&reference, swish_ref)
    };
    let first = store.claim(&payout(&reference, "MMMM0001")).await?;
    let second = store.claim(&other("NNNN0002")).await?;
    check(
        first.claimed_by("MMMM0001") && second.claimed_by("NNNN0002"),
        "each payer must win its own claim on a shared reference",
    )?;

    write(store, &reference, "PAID", false).await?;
    check(
        store.snapshot(OTHER_PAYER, &reference).await?.and_then(|s| s.status).as_deref()
            == Some("CREATED"),
        "settling one payer's payout must not touch the other's",
    )?;

    let repeat = store.claim(&other("OOOO0003")).await?;
    check(!repeat.claimed_by("OOOO0003"), "each payer's reference stays spent on its own")
}

// One reference is one instruction, whatever the row holds, including a status a newer swisha
// sharing the database could write. A guard listing the statuses it locks leaves that one open.
async fn a_claim_never_rewrites_an_existing_reference<S: PayoutStore>(
    store: &S,
    prefix: &str,
) -> Result<()> {
    let statuses = [
        "CREATED", "PENDING", "DEBITED", "PAID", "DECLINED", "ERROR", "NEEDS_REVIEW",
        "FROM_A_NEWER_SWISHA",
    ];
    for (i, status) in statuses.into_iter().enumerate() {
        let reference = format!("{prefix}-keep{i}");
        store.claim(&payout(&reference, "KKKK0001")).await?;
        if status != "CREATED" {
            write(store, &reference, status, false).await?;
        }

        let repeat = store.claim(&payout(&reference, "KKKK0002")).await?;
        check(
            !repeat.claimed_by("KKKK0002"),
            &format!("a claim on an existing {status} reference must not win"),
        )?;

        let snapshot = store.snapshot(PAYER, &reference).await?;
        check(
            snapshot.as_ref().and_then(|s| s.swish_ref.as_deref()) == Some("KKKK0001"),
            &format!("the original instruction must survive a repeat claim on {status}"),
        )?;
        check(
            snapshot.and_then(|s| s.status).as_deref() == Some(status),
            &format!("a repeat claim must leave {status} alone"),
        )?;
    }
    Ok(())
}

async fn terminal_status_is_never_overwritten<S: PayoutStore>(store: &S, prefix: &str) -> Result<()> {
    let reference = format!("{prefix}-terminal");
    store.claim(&payout(&reference, "EEEE0001")).await?;

    check(
        write(store, &reference, "PAID", false).await?,
        "a CREATED payout should accept PAID",
    )?;
    check(
        refuses(store, &reference, "ERROR").await?,
        "a PAID payout must refuse ERROR",
    )?;
    check(
        refuses(store, &reference, "DECLINED").await?,
        "a PAID payout must refuse DECLINED and report no write",
    )?;

    let snapshot = store.snapshot(PAYER, &reference).await?;
    check(
        snapshot.and_then(|s| s.status).as_deref() == Some("PAID"),
        "the payout must still be PAID",
    )
}

// DEBITED moves on to PAID, or to ERROR when the bank refused the money. Never back: a delayed
// DEBITED arriving after that ERROR would make a returned payout look paid.
async fn debited_advances_to_paid_or_error_and_never_back<S: PayoutStore>(
    store: &S,
    prefix: &str,
) -> Result<()> {
    for (i, last) in ["PAID", "ERROR"].into_iter().enumerate() {
        let reference = format!("{prefix}-advance{i}");
        store.claim(&payout(&reference, "FFFF0001")).await?;
        write(store, &reference, "DEBITED", false).await?;

        for refused in ["CREATED", "PENDING", "DECLINED", "NEEDS_REVIEW"] {
            check(
                refuses(store, &reference, refused).await?,
                &format!("a DEBITED payout must refuse {refused}"),
            )?;
        }
        check(
            write(store, &reference, last, false).await?,
            &format!("a DEBITED payout must accept the {last} that follows it"),
        )?;
        check(
            refuses(store, &reference, "DEBITED").await?,
            &format!("a {last} payout must never fall back to DEBITED"),
        )?;
        check(
            status_of(store, &reference).await?.as_deref() == Some(last),
            &format!("the payout must end up {last}"),
        )?;
    }
    Ok(())
}

// PENDING and NEEDS_REVIEW are swisha's own "no answer yet" and "no answer, stopped asking". Over
// an answer either would hide what Swish said, so each replaces only CREATED and PENDING.
async fn no_answer_never_replaces_an_answer<S: PayoutStore>(store: &S, prefix: &str) -> Result<()> {
    for (i, answer) in ["DEBITED", "ERROR", "DECLINED"].into_iter().enumerate() {
        let reference = format!("{prefix}-answer{i}");
        store.claim(&payout(&reference, "PPPP0001")).await?;
        for _ in 0..2 {
            check(
                write(store, &reference, "PENDING", false).await?,
                "PENDING should replace CREATED and PENDING",
            )?;
        }
        check(
            write(store, &reference, answer, false).await?,
            &format!("{answer} should replace PENDING"),
        )?;
        for guess in ["PENDING", "NEEDS_REVIEW"] {
            check(
                refuses(store, &reference, guess).await?,
                &format!("{guess} must not replace {answer}"),
            )?;
        }
        check(
            status_of(store, &reference).await?.as_deref() == Some(answer),
            &format!("{answer} must survive"),
        )?;
    }

    let reference = format!("{prefix}-review");
    store.claim(&payout(&reference, "PPPP0002")).await?;
    write(store, &reference, "PENDING", false).await?;
    check(
        write(store, &reference, "NEEDS_REVIEW", false).await?,
        "NEEDS_REVIEW should replace PENDING",
    )?;
    check(
        refuses(store, &reference, "PENDING").await?,
        "PENDING must not replace NEEDS_REVIEW",
    )
}

// A schedule short enough to run here: first check after 100ms, then every 300ms, for 800ms.
const SCHEDULE: CheckSchedule = CheckSchedule {
    first_after: Duration::from_millis(100),
    every: Duration::from_millis(300),
    window: Duration::from_millis(800),
};

// Everything Swish has not confirmed as PAID is checked; PAID, DECLINED and NEEDS_REVIEW never
// are. Claiming a check must not invent a status: only Swish knows what happened.
async fn checks_claim_only_unconfirmed_payouts<S: PayoutStore>(
    store: &S,
    prefix: &str,
) -> Result<()> {
    let statuses = ["CREATED", "PENDING", "ERROR", "DEBITED", "PAID", "DECLINED", "NEEDS_REVIEW"];
    for (i, status) in statuses.into_iter().enumerate() {
        let reference = format!("{prefix}-due{i}");
        store.claim(&payout(&reference, "HHHH0001")).await?;
        if status != "CREATED" {
            write(store, &reference, status, false).await?;
        }
    }

    // A real age rather than zero. Zero makes the row's timestamp and the cutoff race for the same
    // instant, which then passes or fails on clock granularity alone.
    tokio::time::sleep(Duration::from_millis(150)).await;
    let due = due_checks(store, &SCHEDULE).await?;
    for (i, status) in statuses.into_iter().enumerate() {
        let reference = format!("{prefix}-due{i}");
        let checked = due.iter().any(|(r, _)| *r == reference);
        let expected = !matches!(status, "PAID" | "DECLINED" | "NEEDS_REVIEW");
        let must = if expected { "must" } else { "must never" };
        check(checked == expected, &format!("a {status} payout {must} be checked"))?;
        check(
            status_of(store, &reference).await?.as_deref() == Some(status),
            &format!("claiming a check must leave {status} alone"),
        )?;
    }
    Ok(())
}

// Wide, so a busy database's statement latency, which adds up across the steps, stays well inside
// every margin. The narrower SCHEDULE failed from about 70 ms a statement.
const STEPS: CheckSchedule = CheckSchedule {
    first_after: Duration::from_millis(300),
    every: Duration::from_millis(900),
    window: Duration::from_millis(2400),
};

// Checks wait first_after, then every, and are last once the window has passed. They end only once
// the last answer is recorded: until then even a payout holding Swish's word keeps its last check.
async fn checks_follow_the_schedule_and_end_once<S: PayoutStore>(
    store: &S,
    prefix: &str,
) -> Result<()> {
    let (answered, unanswered) = (format!("{prefix}-sched"), format!("{prefix}-sched-u"));
    store.claim(&payout(&answered, "JJJJ0001")).await?;
    write(store, &answered, "DEBITED", false).await?;
    store.claim(&payout(&unanswered, "JJJJ0002")).await?;
    // Per reference: None when not due, otherwise whether this is the last check.
    let due = || async {
        let due = due_checks(store, &STEPS).await?;
        let of = |reference: &str| due.iter().find(|(r, _)| r == reference).map(|(_, last)| *last);
        Ok::<_, crate::error::BoxError>((of(&answered), of(&unanswered)))
    };

    check(due().await? == (None, None), "a payout must not be checked before first_after")?;
    tokio::time::sleep(Duration::from_millis(450)).await;
    check(due().await? == (Some(false), Some(false)), "the first check must come after first_after")?;
    tokio::time::sleep(Duration::from_millis(450)).await;
    check(due().await? == (None, None), "a later check must wait every, not first_after")?;
    tokio::time::sleep(Duration::from_millis(600)).await;
    check(due().await? == (Some(false), Some(false)), "a later check must come after every")?;
    tokio::time::sleep(Duration::from_millis(1050)).await;
    check(due().await? == (Some(true), Some(true)), "the check after the window must be the last")?;
    tokio::time::sleep(Duration::from_millis(1050)).await;
    check(due().await? == (Some(true), Some(true)), "a last check that recorded nothing must run again")?;
    let ended = answer(&answered, "CHECKS_ENDED", "DEBITED", None);
    check(store.end_checks("DEBITED", &ended).await?, "ending the checks must apply to the status held")?;
    tokio::time::sleep(Duration::from_millis(1050)).await;
    let (answered_due, unanswered_due) = due().await?;
    check(answered_due.is_none(), "a payout whose checks ended must never be due again")?;
    check(
        unanswered_due == Some(true),
        "a payout without an answer must keep its last check until its status moves",
    )
}

fn answer(reference: &str, event: &str, status: &str, code: Option<&str>) -> EventRecord {
    EventRecord {
        payer: PAYER.into(),
        reference: reference.into(),
        swish_ref: None,
        event: event.into(),
        status: Some(status.into()),
        amount: None,
        payee_alias: None,
        error_code: code.map(Into::into),
        error_message: None,
        ip: None,
        actor: None,
        note: None,
    }
}

// A status and the event that explains it are one write. An answer from Swish joins the trail even
// where the status rules refuse it; swisha's own NEEDS_REVIEW only beside the status it sets.
async fn a_status_and_its_event_are_written_together<S: PayoutStore>(store: &S, prefix: &str) -> Result<()> {
    let reference = format!("{prefix}-together");
    store.claim(&payout(&reference, "TTTT0001")).await?;
    let view = || async {
        let view = store.status_view(PAYER, &reference).await?;
        Ok::<_, crate::error::BoxError>(view.map(|v| (v.status, v.error_code, v.was_debited)))
    };
    let held = |status: &str, code: Option<&str>, debited: bool| {
        Some((Some(status.to_string()), code.map(str::to_string), debited))
    };

    let debited = answer(&reference, "CALLBACK", "DEBITED", None);
    check(store.record_status("DEBITED", &debited, true).await?, "an allowed status must apply")?;
    check(view().await? == held("DEBITED", None, true), "a status must land with its event")?;

    let paid = answer(&reference, "PAID", "PAID", None);
    check(store.record_status("PAID", &paid, true).await?, "PAID must follow DEBITED")?;

    let late = answer(&reference, "ERROR", "ERROR", Some("RF07"));
    check(!store.record_status("ERROR", &late, true).await?, "nothing may leave PAID")?;
    check(
        view().await? == held("PAID", Some("RF07"), true),
        "an answer the status rules refuse must still join the trail",
    )?;

    let review = answer(&reference, "NEEDS_REVIEW", "NEEDS_REVIEW", Some("XX01"));
    check(!store.record_status("NEEDS_REVIEW", &review, false).await?, "no review over PAID")?;
    check(
        view().await? == held("PAID", Some("RF07"), true),
        "an event recorded only beside its status must not be recorded without it",
    )
}

// The checks end on the status the last check found, once: never on one a callback has since
// replaced, and an end recorded twice is one end.
async fn the_checks_end_once_on_the_status_held<S: PayoutStore>(store: &S, prefix: &str) -> Result<()> {
    let reference = format!("{prefix}-end");
    store.claim(&payout(&reference, "EEEE0001")).await?;
    write(store, &reference, "DEBITED", false).await?;
    let code = || async {
        Ok::<_, crate::error::BoxError>(store.status_view(PAYER, &reference).await?.and_then(|v| v.error_code))
    };

    let moved_on = answer(&reference, "CHECKS_ENDED", "ERROR", Some("NO01"));
    check(!store.end_checks("ERROR", &moved_on).await?, "the checks must not end on a status not held")?;
    let ended = answer(&reference, "CHECKS_ENDED", "DEBITED", None);
    check(store.end_checks("DEBITED", &ended).await?, "the checks must end on the status held")?;
    let again = answer(&reference, "CHECKS_ENDED", "DEBITED", Some("NO02"));
    check(!store.end_checks("DEBITED", &again).await?, "ended checks must not end again")?;
    check(code().await?.is_none(), "an end that did not apply must leave no event")
}

// Two instances share one database. A payout claimed by one must not also be claimed by the
// other, or Swish is asked twice; and a call never claims more than its limit.
async fn concurrent_checks_never_claim_a_payout_twice<S: PayoutStore>(
    store: &S,
    prefix: &str,
) -> Result<()> {
    let ours: Vec<String> = (0..6).map(|i| format!("{prefix}-race{i}")).collect();
    for reference in &ours {
        store.claim(&payout(reference, "QQQQ0001")).await?;
    }
    tokio::time::sleep(Duration::from_millis(150)).await;

    let limited = store.claim_due_checks(PAYER, &SCHEDULE, 2).await?;
    check(limited.len() <= 2, "a call must not claim more than its limit")?;

    let (a, b) = tokio::join!(
        store.claim_due_checks(PAYER, &SCHEDULE, 1000),
        store.claim_due_checks(PAYER, &SCHEDULE, 1000),
    );
    let (a, b) = (a?, b?);
    let mut seen = std::collections::HashSet::new();
    for claimed in a.iter().chain(&b).chain(&limited) {
        check(seen.insert(claimed.reference.clone()), "no payout may be claimed twice")?;
    }
    check(ours.iter().all(|r| seen.contains(r)), "every due payout must be claimed by one call")
}

// Each number is its own account at Swish, and is checked on its own: a claim for one payer never
// takes another's payout, and the payers listed as due are those with a payout due, each once.
async fn checks_are_claimed_one_payer_at_a_time<S: PayoutStore>(
    store: &S,
    prefix: &str,
) -> Result<()> {
    let (busy, quiet) = (format!("{prefix}-busy"), format!("{prefix}-quiet"));
    let (busy, quiet) = (busy.as_str(), quiet.as_str());
    for i in 0..4 {
        let reference = format!("{prefix}-busy{i}");
        store.claim(&payout_of(busy, &reference, "TTTT0001")).await?;
    }
    let reference = format!("{prefix}-quiet0");
    store.claim(&payout_of(quiet, &reference, "TTTT0002")).await?;
    tokio::time::sleep(Duration::from_millis(150)).await;

    let due = store.payers_with_due_checks(&SCHEDULE).await?;
    check(due.iter().any(|p| p == busy) && due.iter().any(|p| p == quiet), "both payers must be listed due")?;
    check(due.iter().filter(|p| *p == busy).count() == 1, "a payer with many payouts due is listed once")?;

    let claimed = store.claim_due_checks(quiet, &SCHEDULE, 1000).await?;
    check(
        claimed.len() == 1 && claimed.iter().all(|c| c.payer == quiet),
        "a claim for one payer must take that payer's payouts and nothing else",
    )?;
    let due = store.payers_with_due_checks(&SCHEDULE).await?;
    check(!due.iter().any(|p| p == quiet), "a payer whose checks were claimed must not be listed due")?;
    check(due.iter().any(|p| p == busy), "another payer's backlog must stay due, untouched")?;

    let claimed = store.claim_due_checks(busy, &SCHEDULE, 3).await?;
    check(claimed.len() == 3 && claimed.iter().all(|c| c.payer == busy), "a payer's own claim keeps to its limit")
}

async fn due_checks<S: PayoutStore>(store: &S, schedule: &CheckSchedule) -> Result<Vec<(String, bool)>> {
    Ok(store
        .claim_due_checks(PAYER, schedule, 1000)
        .await?
        .into_iter()
        .map(|due| (due.reference, due.last))
        .collect())
}

async fn status_of<S: PayoutStore>(store: &S, reference: &str) -> Result<Option<String>> {
    Ok(store.snapshot(PAYER, reference).await?.and_then(|s| s.status))
}

// A status written as the service writes one. Its trail row names no status, so it changes
// nothing the status view reads from the trail.
async fn write<S: PayoutStore>(store: &S, reference: &str, status: &str, always: bool) -> Result<bool> {
    let event = EventRecord { status: None, ..answer(reference, "CONFORMANCE", "", None) };
    store.record_status(status, &event, always).await
}

// Whether the store refuses a status both ways, with its event on the trail and without.
async fn refuses<S: PayoutStore>(store: &S, reference: &str, status: &str) -> Result<bool> {
    Ok(!write(store, reference, status, false).await? && !write(store, reference, status, true).await?)
}

// What a person does, as confirm and redo record it: who, and why.
fn by_a_person(reference: &str, event: &str, status: Option<&str>) -> EventRecord {
    EventRecord {
        status: status.map(Into::into),
        actor: Some("Anna".into()),
        note: Some("bank".into()),
        ..answer(reference, event, "", None)
    }
}

fn confirmation(reference: &str) -> EventRecord {
    by_a_person(reference, crate::domain::review::CONFIRMED_PAID, Some("PAID"))
}

fn redone(reference: &str) -> EventRecord {
    by_a_person(reference, crate::domain::review::REDONE, None)
}

// A person's word lands only where swisha has stopped asking: NEEDS_REVIEW at once, DEBITED or
// ERROR once their checks have ended. Never on a refusal that moved no money, nor twice, nor after
// a redo, and who and why land with it or not at all.
async fn confirm_paid_only_where_swisha_has_stopped_asking<S: PayoutStore>(
    store: &S,
    prefix: &str,
) -> Result<()> {
    let cases = [
        ("NEEDS_REVIEW", false, true),
        ("DEBITED", false, false),
        ("ERROR", false, false),
        ("DEBITED", true, true),
        ("ERROR", true, true),
        ("CREATED", true, false),
        ("PENDING", true, false),
        ("DECLINED", true, false),
        ("PAID", true, false),
    ];
    for (i, (status, ended, confirms)) in cases.into_iter().enumerate() {
        let reference = format!("{prefix}-confirm{i}");
        store.claim(&payout(&reference, "GGGG0001")).await?;
        if status != "CREATED" {
            write(store, &reference, status, false).await?;
        }
        if ended {
            store.end_checks(status, &answer(&reference, "CHECKS_ENDED", status, None)).await?;
        }
        let outcome = store.confirm_paid(&confirmation(&reference)).await?;
        let (expected, after) = if confirms {
            (ConfirmOutcome::Confirmed, "PAID")
        } else {
            (ConfirmOutcome::Refused(status.into()), status)
        };
        let when = if ended { "after the checks end" } else { "before the checks end" };
        check(outcome == expected, &format!("confirming {status} {when}: {outcome:?}"))?;
        let view = store.status_view(PAYER, &reference).await?.ok_or_else(|| err("conformance: no view"))?;
        check(
            view.status.as_deref() == Some(after),
            &format!("confirming {status} {when} must leave it {after}"),
        )?;
        check(
            view.manual_confirmation.is_some() == confirms,
            &format!("confirming {status} {when} must record who and why with it, and never without"),
        )?;
    }
    check(
        store.confirm_paid(&confirmation(&format!("{prefix}-nothing"))).await? == ConfirmOutcome::NotFound,
        "confirming a payout that does not exist must say so",
    )?;

    // A person who paid it again has closed it: the redo carries it on.
    let redone_payout = format!("{prefix}-confirm-r");
    store.claim(&payout(&redone_payout, "GGGG0002")).await?;
    write(store, &redone_payout, "NEEDS_REVIEW", false).await?;
    let again = format!("{prefix}-confirm-r2");
    store.claim_redo(&redo(&redone_payout, &again, "GGGG0003"), &redone(&redone_payout)).await?;
    check(
        store.confirm_paid(&confirmation(&redone_payout)).await? == ConfirmOutcome::Redone(again),
        "a payout that was redone must never be confirmed, and must name its redo",
    )?;
    check(
        status_of(store, &redone_payout).await?.as_deref() == Some("NEEDS_REVIEW"),
        "a refused confirmation must leave the payout alone",
    )
}

fn redo<'a>(replaces: &'a str, reference: &'a str, swish_ref: &'a str) -> NewRedo<'a> {
    NewRedo {
        payer: PAYER,
        replaces,
        reference,
        payer_alias: "1234679304",
        swish_ref,
        callback_identifier: None,
    }
}

// A redo copies the payout it replaces, happens once even when two people press at once, and
// never where the money left or Swish may still pay.
async fn a_redo_is_claimed_once_and_never_where_money_left<S: PayoutStore>(
    store: &S,
    prefix: &str,
) -> Result<()> {
    let original = format!("{prefix}-orig");
    let detailed = NewPayout {
        payee_ssn: Some("196408233234"),
        amount: 250.5,
        message: "redo me",
        ..payout(&original, "RRRR0001")
    };
    store.claim(&detailed).await?;
    write(store, &original, "ERROR", false).await?;

    let first = format!("{prefix}-orig-r1");
    let RedoOutcome::Claimed(details) = store.claim_redo(&redo(&original, &first, "RRRR0002"), &redone(&original)).await?
    else {
        return Err(err("conformance: a redo of an ERROR payout must be claimed"));
    };
    check(
        details.payee_alias == "46701234567"
            && details.payee_ssn.as_deref() == Some("196408233234")
            && (details.amount - 250.5).abs() < 1e-9
            && details.message == "redo me",
        "a redo must copy the recipient, amount and message of the payout it replaces",
    )?;
    let claimed = store.snapshot(PAYER, &first).await?;
    check(
        claimed.as_ref().and_then(|s| s.status.as_deref()) == Some("CREATED")
            && claimed.as_ref().and_then(|s| s.swish_ref.as_deref()) == Some("RRRR0002"),
        "a redo must be claimed as a new payout with its own instruction",
    )?;

    // The same redo asked for again, as after its answer was lost, is its own claim.
    let repeated = store.claim_redo(&redo(&original, &first, "RRRR0002"), &redone(&original)).await?;
    check(
        matches!(&repeated, RedoOutcome::Claimed(again) if again.payee_alias == details.payee_alias
            && again.payee_ssn == details.payee_ssn
            && again.message == details.message),
        "a redo asked for again with its own instruction must be its own claim, with the same details",
    )?;
    let replaces = store.status_view(PAYER, &first).await?.and_then(|view| view.replaces);
    check(replaces.as_deref() == Some(original.as_str()), "and it must leave the claim as it was")?;

    let second = format!("{prefix}-orig-r2");
    let again = store.claim_redo(&redo(&original, &second, "RRRR0003"), &redone(&original)).await?;
    check(
        matches!(&again, RedoOutcome::AlreadyRedone(r) if *r == first),
        "a payout must be redone once, and say under which reference",
    )?;
    check(store.snapshot(PAYER, &second).await?.is_none(), "a refused redo must claim nothing")?;

    // Two people press at once: one payout.
    let raced = format!("{prefix}-race-orig");
    store.claim(&payout(&raced, "RRRR0004")).await?;
    write(store, &raced, "DECLINED", false).await?;
    let (a, b) = (format!("{prefix}-race-r1"), format!("{prefix}-race-r2"));
    let (first_press, second_press) = (redo(&raced, &a, "RRRR0005"), redo(&raced, &b, "RRRR0006"));
    let record = redone(&raced);
    let (one, two) = tokio::join!(store.claim_redo(&first_press, &record), store.claim_redo(&second_press, &record));
    let winners = [one?, two?].iter().filter(|o| matches!(o, RedoOutcome::Claimed(_))).count();
    check(winners == 1, "two concurrent redos of one payout must yield one new payout")?;

    for (i, status) in ["DEBITED", "PAID", "CREATED", "PENDING"].into_iter().enumerate() {
        let reference = format!("{prefix}-kept{i}");
        store.claim(&payout(&reference, "RRRR0007")).await?;
        if status != "CREATED" {
            write(store, &reference, status, false).await?;
        }
        let new = format!("{prefix}-kept{i}-r");
        let outcome = store.claim_redo(&redo(&reference, &new, "RRRR0008"), &redone(&reference)).await?;
        check(
            matches!(&outcome, RedoOutcome::NotRedoable(s) if s == status),
            &format!("a {status} payout must never be redone"),
        )?;
        check(store.snapshot(PAYER, &new).await?.is_none(), "a refused redo must claim nothing")?;
    }

    // A spent reference stays spent, and the payout can still be redone under a free one.
    let taken = format!("{prefix}-tk");
    store.claim(&payout(&taken, "RRRR0009")).await?;
    write(store, &taken, "NEEDS_REVIEW", false).await?;
    let outcome = store.claim_redo(&redo(&taken, &first, "RRRR0010"), &redone(&taken)).await?;
    check(matches!(outcome, RedoOutcome::ReferenceTaken(_)), "a redo must not take a spent reference")?;
    let free = format!("{prefix}-tk-r");
    let outcome = store.claim_redo(&redo(&taken, &free, "RRRR0011"), &redone(&taken)).await?;
    check(matches!(outcome, RedoOutcome::Claimed(_)), "and the payout can still be redone")?;

    // A redo that failed too is redone in turn.
    write(store, &first, "ERROR", false).await?;
    let outcome = store.claim_redo(&redo(&first, &second, "RRRR0012"), &redone(&first)).await?;
    check(matches!(outcome, RedoOutcome::Claimed(_)), "a failed redo must itself be redoable")?;

    let (none, none_redo) = (format!("{prefix}-none"), format!("{prefix}-none-r"));
    let missing = store.claim_redo(&redo(&none, &none_redo, "RRRR0013"), &redone(&none)).await?;
    check(matches!(missing, RedoOutcome::NotFound), "redoing a payout that does not exist must say so")?;

    // One person confirms the payout paid while another redoes it. Both would be a second payment
    // in the books, so exactly one may land, however the two interleave.
    for round in 0..20 {
        let (contested, its_redo) = (format!("{prefix}-cr{round}"), format!("{prefix}-cr{round}-r"));
        store.claim(&payout(&contested, "RRRR0014")).await?;
        write(store, &contested, "NEEDS_REVIEW", false).await?;
        let press = redo(&contested, &its_redo, "RRRR0015");
        let (said, record) = (confirmation(&contested), redone(&contested));
        let (confirmed, redid) = tokio::join!(store.confirm_paid(&said), store.claim_redo(&press, &record));
        let confirmed = confirmed? == ConfirmOutcome::Confirmed;
        let redid = matches!(redid?, RedoOutcome::Claimed(_));
        check(confirmed != redid, "a confirmation and a redo of one payout must never both land")?;
    }
    Ok(())
}

async fn earlier_paid<S: PayoutStore>(store: &S, reference: &str) -> Result<bool> {
    Ok(store.status_view(PAYER, reference).await?.is_some_and(|view| view.earlier_paid))
}

// Swish may pay a payout after it was redone. Nothing after it is redone again then, since that
// would pay a third time, and every payout after it says so.
async fn nothing_is_redone_once_a_payout_before_it_was_paid<S: PayoutStore>(
    store: &S,
    prefix: &str,
) -> Result<()> {
    let first = format!("{prefix}-chain");
    let (second, third) = (format!("{first}-r1"), format!("{first}-r2"));
    store.claim(&payout(&first, "CCCC0001")).await?;
    write(store, &first, "ERROR", false).await?;
    let outcome = store.claim_redo(&redo(&first, &second, "CCCC0002"), &redone(&first)).await?;
    check(matches!(outcome, RedoOutcome::Claimed(_)), "a failed payout must be redoable")?;
    write(store, &second, "ERROR", false).await?;
    check(!earlier_paid(store, &second).await?, "nothing before the redo is paid yet")?;

    let counted = store.paid_twice().await?;
    write(store, &first, "PAID", false).await?;
    check(earlier_paid(store, &second).await?, "a redo must say a payout before it was paid")?;
    check(store.paid_twice().await? == counted + 1, "a payout paid after its redo must be counted")?;
    check(!earlier_paid(store, &first).await?, "the payout paid has nothing before it")?;
    let outcome = store.claim_redo(&redo(&second, &third, "CCCC0003"), &redone(&second)).await?;
    check(
        matches!(&outcome, RedoOutcome::EarlierPaid { reference, status } if *reference == first && status == "PAID"),
        "a redo must be refused, naming the payout paid, once one before it was paid",
    )?;
    check(store.snapshot(PAYER, &third).await?.is_none(), "a refused redo must claim nothing")?;

    // A status a newer swisha wrote may mean paid: only a failure lets the chain be paid again.
    let (newer, after) = (format!("{prefix}-chain-n"), format!("{prefix}-chain-n-r1"));
    store.claim(&payout(&newer, "CCCC0004")).await?;
    write(store, &newer, "ERROR", false).await?;
    store.claim_redo(&redo(&newer, &after, "CCCC0005"), &redone(&newer)).await?;
    write(store, &after, "ERROR", false).await?;
    write(store, &newer, "SETTLED_LATER", false).await?;
    let outcome = store.claim_redo(&redo(&after, &format!("{after}-r2"), "CCCC0006"), &redone(&after)).await?;
    check(
        matches!(&outcome, RedoOutcome::EarlierPaid { reference, status } if *reference == newer && status == "SETTLED_LATER"),
        "a redo must be refused while a payout before it holds a status this version does not know",
    )
}

// What a person and a redo need beside the status: the amount, the checks, whether Swish held it
// or the send ended without it, the redo chain, and a confirmation with what Swish said since.
async fn the_status_view_shows_what_a_person_needs<S: PayoutStore>(
    store: &S,
    prefix: &str,
) -> Result<()> {
    type Person<'a> = Option<&'a str>;
    let event = |reference: &str, event: &str, status: &str, actor: Person, note: Person| EventRecord {
        payer: PAYER.into(),
        reference: reference.into(),
        swish_ref: None,
        event: event.into(),
        status: Some(status.into()),
        amount: None,
        payee_alias: None,
        error_code: None,
        error_message: None,
        ip: None,
        actor: actor.map(Into::into),
        note: note.map(Into::into),
    };

    let returned = format!("{prefix}-view");
    store.claim(&payout(&returned, "VVVV0001")).await?;
    write(store, &returned, "DEBITED", false).await?;
    store.record_event(&event(&returned, "CALLBACK", "DEBITED", None, None)).await?;
    write(store, &returned, "ERROR", false).await?;
    let view = store.status_view(PAYER, &returned).await?.ok_or_else(|| err("conformance: no view"))?;
    check(view.amount == 100.0, "the view must carry the amount")?;
    check(view.was_debited, "a payout Swish reported DEBITED must say so")?;
    check(view.accepted, "a payout Swish called back about must show Swish holds it")?;
    check(!view.checks_ended, "a payout still checked must say so")?;
    store.end_checks("ERROR", &answer(&returned, "CHECKS_ENDED", "ERROR", None)).await?;
    let view = store.status_view(PAYER, &returned).await?.ok_or_else(|| err("conformance: no view"))?;
    check(view.checks_ended, "a payout whose checks ended must say so")?;

    let redo_of_it = format!("{prefix}-view-r");
    store.claim_redo(&redo(&returned, &redo_of_it, "VVVV0002"), &redone(&returned)).await?;
    let view = store.status_view(PAYER, &returned).await?.ok_or_else(|| err("conformance: no view"))?;
    check(view.replaced_by.as_deref() == Some(redo_of_it.as_str()), "the payout replaced must name its redo")?;
    let view = store.status_view(PAYER, &redo_of_it).await?.ok_or_else(|| err("conformance: no view"))?;
    check(view.replaces.as_deref() == Some(returned.as_str()), "the redo must name what it replaces")?;
    check(!view.was_debited, "a redo starts with nothing of its own debited")?;

    let accepted = format!("{prefix}-view-a");
    store.claim(&payout(&accepted, "VVVV0004")).await?;
    let held = || async { Ok::<_, crate::error::BoxError>(store.status_view(PAYER, &accepted).await?.is_some_and(|v| v.accepted)) };
    check(!held().await?, "a payout Swish never answered about must not show Swish holds it")?;
    store.record_event(&answer(&accepted, crate::domain::payout::ACCEPTED, "", None)).await?;
    check(held().await?, "a payout whose acceptance is on its trail must show Swish holds it")?;

    let ended = format!("{prefix}-view-s");
    store.claim(&payout(&ended, "VVVV0005")).await?;
    let unaccepted = || async {
        Ok::<_, crate::error::BoxError>(store.status_view(PAYER, &ended).await?.is_some_and(|v| v.send_unaccepted))
    };
    store.record_event(&answer(&ended, "INITIATED", "", None)).await?;
    store.record_event(&answer(&ended, "CALLBACK", "PENDING", None)).await?;
    check(!unaccepted().await?, "only an event the send ended with may show it ended without Swish")?;
    store.record_event(&answer(&ended, "PENDING", "PENDING", None)).await?;
    check(unaccepted().await?, "a send that ended without an answer must say so")?;

    let confirmed = format!("{prefix}-view-c");
    store.claim(&payout(&confirmed, "VVVV0003")).await?;
    write(store, &confirmed, "NEEDS_REVIEW", false).await?;
    store.record_event(&answer(&confirmed, "CALLBACK", "ERROR", None)).await?;
    store.confirm_paid(&confirmation(&confirmed)).await?;
    let view = store.status_view(PAYER, &confirmed).await?.ok_or_else(|| err("conformance: no view"))?;
    check(
        matches!(
            view.manual_confirmation,
            Some(ManualConfirmation { ref by, ref note, .. }) if by == "Anna" && note == "bank"
        ),
        "a payout a person confirmed must say who, and why",
    )?;
    check(!view.contradicted, "a failure before the confirmation does not contradict it")?;
    store.record_event(&answer(&confirmed, "CALLBACK", "PAID", None)).await?;
    let view = store.status_view(PAYER, &confirmed).await?.ok_or_else(|| err("conformance: no view"))?;
    check(!view.contradicted, "only a failure after the confirmation contradicts it")?;
    store.record_event(&answer(&confirmed, "CALLBACK", "ERROR", None)).await?;
    let view = store.status_view(PAYER, &confirmed).await?.ok_or_else(|| err("conformance: no view"))?;
    check(view.contradicted, "a failure Swish reported after a confirmation must show it")
}

async fn events_round_trip<S: PayoutStore>(store: &S, prefix: &str) -> Result<()> {
    let reference = format!("{prefix}-events");
    store.claim(&payout(&reference, "LLLL0001")).await?;
    let latest_code = || async {
        Ok::<_, crate::error::BoxError>(store.status_view(PAYER, &reference).await?.and_then(|v| v.error_code))
    };
    check(
        latest_code().await?.is_none(),
        "a payout with no events must report no error code",
    )?;

    for code in ["TA01", "RF07"] {
        store
            .record_event(&EventRecord {
                payer: PAYER.into(),
                reference: reference.clone(),
                swish_ref: Some("LLLL0001".into()),
                event: "CALLBACK".into(),
                status: Some("ERROR".into()),
                amount: Some(100.0),
                payee_alias: Some("46701234567".into()),
                error_code: Some(code.into()),
                error_message: Some("conformance".into()),
                ip: None,
                actor: None,
                note: None,
            })
            .await?;
    }
    check(
        latest_code().await?.as_deref() == Some("RF07"),
        "the most recent error code must be returned",
    )?;

    // The same reference under another payer is another payout, with its own trail.
    store
        .record_event(&EventRecord {
            payer: OTHER_PAYER.into(),
            reference: reference.clone(),
            swish_ref: Some("LLLL0002".into()),
            event: "CALLBACK".into(),
            status: Some("ERROR".into()),
            amount: Some(100.0),
            payee_alias: Some("46701234567".into()),
            error_code: Some("ACMT07".into()),
            error_message: Some("conformance".into()),
            ip: None,
            actor: None,
            note: None,
        })
        .await?;
    check(
        latest_code().await?.as_deref() == Some("RF07"),
        "another payer's error must never be reported as this payer's",
    )?;

    store.record_log("info", "conformance", None, None).await
}

fn payer<'a>(name: &'a str, number: &'a str, key_hash: &'a str) -> NewPayer<'a> {
    NewPayer { name, nickname: "Conformance", number, added_by: "conformance", key_hash }
}

// The line a change to a number is written with.
const LOGGED: LogLine<'static> = LogLine { level: "info", message: "conformance", context: None, ip: None };

// A fresh key's hash, as an add would store it.
fn key_hash() -> String {
    crate::payers::new_key().hash
}

// A name is the double-payout key and a number finds its payer on a callback, so each is stored
// once, disabled numbers included.
async fn a_payer_name_and_number_are_each_stored_once<S: PayoutStore>(store: &S, prefix: &str) -> Result<()> {
    let (name, number) = (format!("{prefix}-one"), format!("{prefix}n1"));
    check(
        store.add_payer(&payer(&name, &number, &key_hash()), &LOGGED).await? == AddPayerOutcome::Added,
        "a new payer must be added",
    )?;
    check(
        store.add_payer(&payer(&name, &format!("{prefix}n2"), &key_hash()), &LOGGED).await?
            == AddPayerOutcome::NameTaken,
        "a second payer under a stored name must be refused",
    )?;
    store.set_payer_disabled(&name, true, &LOGGED).await?;
    check(
        store.add_payer(&payer(&format!("{prefix}-two"), &number, &key_hash()), &LOGGED).await?
            == AddPayerOutcome::NumberTaken(name.clone()),
        "a stored number must be refused under another name, even while disabled, and name its payer",
    )?;
    let stored = store.payers().await?;
    check(
        stored.iter().filter(|p| p.number == number).count() == 1,
        "the number must be stored once",
    )
}

// Under a new name a number would pay every reference it already paid again, so a number with
// payouts on record is only ever added under the name it paid as.
async fn a_number_paid_under_one_name_is_never_added_under_another<S: PayoutStore>(
    store: &S,
    prefix: &str,
) -> Result<()> {
    let (paid_as, number, reference) = (format!("{prefix}-paid"), format!("{prefix}n3"), format!("{prefix}-was"));
    store
        .claim(&NewPayout { payer_alias: &number, ..payout_of(&paid_as, &reference, "UUUU0001") })
        .await?;
    check(
        store.add_payer(&payer(&format!("{prefix}-renamed"), &number, &key_hash()), &LOGGED).await?
            == AddPayerOutcome::NumberPaidAs(paid_as.clone()),
        "a number paid under one name must be refused under another, naming the first",
    )?;
    check(
        store.add_payer(&payer(&paid_as, &number, &key_hash()), &LOGGED).await? == AddPayerOutcome::Added,
        "a number must be added under the name it paid as",
    )
}

async fn concurrent_adds_of_one_payer_store_one<S: PayoutStore>(store: &S, prefix: &str) -> Result<()> {
    let (name, four, five) = (format!("{prefix}-race"), format!("{prefix}n4"), format!("{prefix}n5"));
    let keys = (key_hash(), key_hash());
    let (first, second) = (payer(&name, &four, &keys.0), payer(&name, &five, &keys.1));
    let (a, b) = tokio::join!(store.add_payer(&first, &LOGGED), store.add_payer(&second, &LOGGED));
    let added = [a?, b?].iter().filter(|outcome| **outcome == AddPayerOutcome::Added).count();
    check(added == 1, "two concurrent adds of one name must store one payer")?;

    let (number, one, two) = (format!("{prefix}n6"), format!("{prefix}-r1"), format!("{prefix}-r2"));
    let keys = (key_hash(), key_hash());
    let (first, second) = (payer(&one, &number, &keys.0), payer(&two, &number, &keys.1));
    let (a, b) = tokio::join!(store.add_payer(&first, &LOGGED), store.add_payer(&second, &LOGGED));
    let added = [a?, b?].iter().filter(|outcome| **outcome == AddPayerOutcome::Added).count();
    check(added == 1, "two concurrent adds of one number must store one payer")
}

// A running swisha reloads a number when its updated_at moves, so every change must move it.
async fn payer_changes_are_kept_and_move_updated_at<S: PayoutStore>(store: &S, prefix: &str) -> Result<()> {
    let name = format!("{prefix}-change");
    store.add_payer(&payer(&name, &format!("{prefix}n7"), &key_hash()), &LOGGED).await?;
    let read = || async {
        let stored = store.payers().await?;
        stored
            .into_iter()
            .find(|p| p.name == name)
            .ok_or_else(|| err("conformance: a stored payer must be listed"))
    };
    let mut before = read().await?;
    check(before.nickname == "Conformance" && !before.disabled, "a payer must read back as stored")?;

    for change in ["nickname", "disable", "enable", "touch"] {
        tokio::time::sleep(Duration::from_millis(5)).await;
        let found = match change {
            "nickname" => store.set_payer_nickname(&name, "Renamed", &LOGGED).await?,
            "disable" => store.set_payer_disabled(&name, true, &LOGGED).await?,
            "enable" => store.set_payer_disabled(&name, false, &LOGGED).await?,
            _ => store.touch_payer(&name, &LOGGED).await?,
        };
        check(found, &format!("{change} must find the payer"))?;
        let after = read().await?;
        check(after.updated_at > before.updated_at, &format!("{change} must move updated_at"))?;
        match change {
            "nickname" => check(after.nickname == "Renamed", "the nickname must be kept")?,
            "disable" => check(after.disabled, "a disabled payer must read back disabled")?,
            "enable" => check(!after.disabled, "an enabled payer must read back enabled")?,
            _ => {}
        }
        check(after.name == before.name && after.number == before.number, "name and number never change")?;
        before = after;
    }
    let missing = format!("{prefix}-missing");
    check(
        !store.set_payer_nickname(&missing, "x", &LOGGED).await?
            && !store.set_payer_disabled(&missing, true, &LOGGED).await?
            && !store.touch_payer(&missing, &LOGGED).await?,
        "a change to a payer that does not exist must say so",
    )
}

// A key is looked up on every request, so a replaced key must find nothing at once, and of two
// replacements made with one key only one may win: the loser no longer holds a key.
async fn a_key_finds_its_payer_until_it_is_replaced<S: PayoutStore>(store: &S, prefix: &str) -> Result<()> {
    let name = format!("{prefix}-key");
    let first = key_hash();
    store.add_payer(&payer(&name, &format!("{prefix}n8"), &first), &LOGGED).await?;
    let holder = KeyHolder { name: name.clone(), disabled: false };
    check(store.payer_by_key(&first).await? == Some(holder), "a key must find its payer")?;
    check(store.payer_by_key(&key_hash()).await?.is_none(), "a key no payer holds must find nothing")?;
    store.set_payer_disabled(&name, true, &LOGGED).await?;
    check(
        store.payer_by_key(&first).await?.is_some_and(|holder| holder.disabled),
        "a key must read its payer's disabled flag as it now stands",
    )?;
    store.set_payer_disabled(&name, false, &LOGGED).await?;

    let second = key_hash();
    check(
        !store.replace_payer_key(&name, Some(&second), &second, &LOGGED).await?,
        "a replacement made with a key that is not the payer's must be refused",
    )?;
    check(
        store.replace_payer_key(&name, Some(&first), &second, &LOGGED).await?,
        "the payer's own key must replace it",
    )?;
    check(store.payer_by_key(&first).await?.is_none(), "a replaced key must find nothing")?;
    check(
        store.payer_by_key(&second).await?.is_some_and(|holder| holder.name == name),
        "the new key must find the payer",
    )?;

    let mut current = second;
    for _ in 0..5 {
        let (one, two) = (key_hash(), key_hash());
        let (a, b) = tokio::join!(
            store.replace_payer_key(&name, Some(&current), &one, &LOGGED),
            store.replace_payer_key(&name, Some(&current), &two, &LOGGED),
        );
        let (a, b) = (a?, b?);
        check(a != b, "of two replacements made with one key, exactly one must succeed")?;
        current = if a { one } else { two };
    }
    check(
        store.payer_by_key(&current).await?.is_some_and(|holder| holder.name == name),
        "the winning replacement's key must find the payer",
    )?;

    check(
        store.replace_payer_key(&name, None, &key_hash(), &LOGGED).await?,
        "a replacement on the server needs no key",
    )?;
    check(store.payer_by_key(&current).await?.is_none(), "a key replaced on the server must find nothing")?;
    check(
        !store.replace_payer_key(&format!("{prefix}-missing"), None, &key_hash(), &LOGGED).await?,
        "a replacement for a payer that does not exist must say so",
    )
}

// Every open event stream on an instance is checked in one question: the keys still held come back
// with their numbers, and a replaced or unknown one does not.
async fn keys_are_checked_together<S: PayoutStore>(store: &S, prefix: &str) -> Result<()> {
    let (one, two) = (format!("{prefix}-k1"), format!("{prefix}-k2"));
    let (first, second, unknown) = (key_hash(), key_hash(), key_hash());
    store.add_payer(&payer(&one, &format!("{prefix}n9"), &first), &LOGGED).await?;
    store.add_payer(&payer(&two, &format!("{prefix}n10"), &second), &LOGGED).await?;

    let asked = [first.clone(), second.clone(), unknown];
    let mut found = store.key_holders(&asked).await?;
    found.sort();
    let mut expected = vec![(first.clone(), one.clone()), (second.clone(), two.clone())];
    expected.sort();
    check(found == expected, "every key still held, with its number, and nothing for an unknown one")?;

    check(store.replace_payer_key(&one, Some(&first), &key_hash(), &LOGGED).await?, "the replace must apply")?;
    let found = store.key_holders(&asked).await?;
    check(found == vec![(second, two)], "a replaced key must not be found together with the others")?;
    check(store.key_holders(&[]).await?.is_empty(), "no keys asked, none found")
}
