use swisha::util::time::{is_valid_date, to_stockholm};

// Unix seconds for the UTC instant named in each constant.
const JAN_15_1200: i64 = 1_768_478_400; // 2026-01-15T12:00:00Z
const JUL_15_1200: i64 = 1_784_116_800; // 2026-07-15T12:00:00Z
const JUL_15_2330: i64 = 1_784_158_200; // 2026-07-15T23:30:00Z
const DEC_31_2300: i64 = 1_798_758_000; // 2026-12-31T23:00:00Z
const SPRING_BEFORE: i64 = 1_774_745_999; // 2026-03-29T00:59:59Z
const SPRING_AT: i64 = 1_774_746_000; // 2026-03-29T01:00:00Z
const AUTUMN_BEFORE: i64 = 1_792_889_999; // 2026-10-25T00:59:59Z
const AUTUMN_AT: i64 = 1_792_890_000; // 2026-10-25T01:00:00Z
const SPRING_2027: i64 = 1_806_195_600; // 2027-03-28T01:00:00Z
const AUTUMN_2027: i64 = 1_824_944_400; // 2027-10-31T01:00:00Z

#[test]
fn winter_is_cet() {
    assert_eq!(to_stockholm(JAN_15_1200).rfc3339(), "2026-01-15T13:00:00+01:00");
    assert_eq!(to_stockholm(DEC_31_2300).rfc3339(), "2027-01-01T00:00:00+01:00");
}

#[test]
fn summer_is_cest() {
    assert_eq!(to_stockholm(JUL_15_1200).rfc3339(), "2026-07-15T14:00:00+02:00");
}

#[test]
fn switchover_happens_on_the_last_sunday_at_0100_utc() {
    assert_eq!(to_stockholm(SPRING_BEFORE).rfc3339(), "2026-03-29T01:59:59+01:00");
    assert_eq!(to_stockholm(SPRING_AT).rfc3339(), "2026-03-29T03:00:00+02:00");
    assert_eq!(to_stockholm(AUTUMN_BEFORE).rfc3339(), "2026-10-25T02:59:59+02:00");
    assert_eq!(to_stockholm(AUTUMN_AT).rfc3339(), "2026-10-25T02:00:00+01:00");
}

#[test]
fn boundary_dates_move_with_the_year() {
    assert_eq!(to_stockholm(SPRING_2027 - 1).offset_seconds, 3600);
    assert_eq!(to_stockholm(SPRING_2027).offset_seconds, 7200);
    assert_eq!(to_stockholm(AUTUMN_2027 - 1).offset_seconds, 7200);
    assert_eq!(to_stockholm(AUTUMN_2027).offset_seconds, 3600);
}

#[test]
fn a_local_date_can_differ_from_the_utc_date() {
    assert_eq!(to_stockholm(JUL_15_2330).date(), "2026-07-16");
}

#[test]
fn formats_render_as_the_wire_expects() {
    let t = to_stockholm(JUL_15_1200);
    assert_eq!(t.date(), "2026-07-15");
    assert_eq!(t.date_time(), "2026-07-15 14:00:00");
    assert_eq!(t.utc_z(), "2026-07-15T14:00:00Z");
}

#[test]
fn date_validation_accepts_real_dates() {
    for d in ["2026-01-01", "2026-12-31", "2024-02-29", "2000-02-29", "1970-01-01"] {
        assert!(is_valid_date(d), "{d} should be valid");
    }
}

#[test]
fn date_validation_rejects_impossible_and_misshapen_dates() {
    for d in [
        "2026-02-29", // not a leap year
        "1900-02-29", // century non-leap
        "2026-13-01", // month out of range
        "2026-00-10", // month zero
        "2026-04-31", // April has 30
        "2026-01-32",
        "2026-01-00",
        "2026-1-05",  // not zero padded
        "26-01-05",   // short year
        "2026/01/05", // wrong separator
        "2026-01-05x",
        "",
        "not a date",
    ] {
        assert!(!is_valid_date(d), "{d:?} should be rejected");
    }
}
