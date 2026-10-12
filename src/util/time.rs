//! A small clock, so nothing else has to depend on a date library.
//!
//! Swish is a Swedish-only service, so every timestamp this system produces is Stockholm local
//! time rather than UTC: CET (UTC+1), or CEST (UTC+2) during EU summer time.

use std::time::{SystemTime, UNIX_EPOCH};

/// A moment, broken into fields, with the offset it was rendered at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DateTime {
    /// Four-digit year.
    pub year: i32,
    /// Month, 1 to 12.
    pub month: u32,
    /// Day of month, 1 to 31.
    pub day: u32,
    /// Hour, 0 to 23.
    pub hour: u32,
    /// Minute, 0 to 59.
    pub minute: u32,
    /// Second, 0 to 59.
    pub second: u32,
    /// Offset from UTC in seconds. 3600 for CET, 7200 for CEST, 0 for UTC.
    pub offset_seconds: i32,
}

impl DateTime {
    /// The date as `YYYY-MM-DD`.
    pub fn date(&self) -> String {
        format!("{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }

    /// Date and time as `YYYY-MM-DD HH:MM:SS`, with no offset.
    pub fn date_time(&self) -> String {
        format!(
            "{} {:02}:{:02}:{:02}",
            self.date(),
            self.hour,
            self.minute,
            self.second
        )
    }

    /// RFC 3339 with the numeric offset, as `2026-08-27T09:15:04+02:00`.
    pub fn rfc3339(&self) -> String {
        let (sign, offset) = if self.offset_seconds < 0 {
            ('-', -self.offset_seconds)
        } else {
            ('+', self.offset_seconds)
        };
        format!(
            "{}T{:02}:{:02}:{:02}{}{:02}:{:02}",
            self.date(),
            self.hour,
            self.minute,
            self.second,
            sign,
            offset / 3600,
            (offset % 3600) / 60,
        )
    }

    // Swish wants instructionDate as an ISO timestamp in UTC. Only valid on a UTC value.
    /// RFC 3339 in UTC with the `Z` suffix. Swish wants instruction dates in this form.
    pub fn utc_z(&self) -> String {
        format!(
            "{}T{:02}:{:02}:{:02}Z",
            self.date(),
            self.hour,
            self.minute,
            self.second
        )
    }
}

/// Now, in Stockholm local time.
pub fn now() -> DateTime {
    to_stockholm(unix_now())
}

/// Now, in UTC.
pub fn now_utc() -> DateTime {
    from_unix(unix_now(), 0)
}

/// Converts a Unix timestamp to Stockholm local time.
pub fn to_stockholm(unix: i64) -> DateTime {
    from_unix(unix, offset_seconds(unix))
}

/// Whether a string is a real calendar date in `YYYY-MM-DD` form, leap years included.
pub fn is_valid_date(text: &str) -> bool {
    let bytes = text.as_bytes();
    if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return false;
    }
    if !bytes
        .iter()
        .enumerate()
        .all(|(i, b)| i == 4 || i == 7 || b.is_ascii_digit())
    {
        return false;
    }

    let year: i32 = text[0..4].parse().unwrap_or(0);
    let month: u32 = text[5..7].parse().unwrap_or(0);
    let day: u32 = text[8..10].parse().unwrap_or(0);
    (1..=12).contains(&month) && day >= 1 && day <= days_in_month(year, month)
}

/// Seconds since the Unix epoch for a UTC date and time.
pub fn unix_from_utc(year: i32, month: u32, day: u32, hour: u32, minute: u32, second: u32) -> i64 {
    days_from_civil(year, month, day) * 86_400
        + i64::from(hour) * 3600
        + i64::from(minute) * 60
        + i64::from(second)
}

/// Now, as seconds since the Unix epoch.
pub fn unix_now() -> i64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        Err(e) => -(e.duration().as_secs() as i64),
    }
}

fn from_unix(unix: i64, offset: i32) -> DateTime {
    let local = unix + offset as i64;
    let days = local.div_euclid(86_400);
    let seconds = local.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);

    DateTime {
        year,
        month,
        day,
        hour: (seconds / 3600) as u32,
        minute: ((seconds % 3600) / 60) as u32,
        second: (seconds % 60) as u32,
        offset_seconds: offset,
    }
}

// EU summer time runs from 01:00 UTC on the last Sunday in March to 01:00 UTC on the last
// Sunday in October. Both boundaries are defined in UTC, not in local time.
fn offset_seconds(unix: i64) -> i32 {
    let (year, _, _) = civil_from_days(unix.div_euclid(86_400));
    let starts = last_sunday(year, 3) * 86_400 + 3600;
    let ends = last_sunday(year, 10) * 86_400 + 3600;
    if unix >= starts && unix < ends { 7200 } else { 3600 }
}

fn last_sunday(year: i32, month: u32) -> i64 {
    let (next_year, next_month) = if month == 12 {
        (year + 1, 1)
    } else {
        (year, month + 1)
    };
    let last_day = days_from_civil(next_year, next_month, 1) - 1;
    last_day - weekday_from_sunday(last_day) as i64
}

fn weekday_from_sunday(days: i64) -> u32 {
    // Unix day 0 is 1970-01-01, a Thursday.
    (days + 4).rem_euclid(7) as u32
}

fn is_leap(year: i32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap(year) => 29,
        2 => 28,
        _ => 0,
    }
}

// Howard Hinnant's civil calendar algorithms, in days relative to 1970-01-01.
fn days_from_civil(year: i32, month: u32, day: u32) -> i64 {
    let year = year as i64 - i64::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let month = month as i64;
    let day_of_year = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day as i64 - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn civil_from_days(days: i64) -> (i32, u32, u32) {
    let days = days + 719_468;
    let era = if days >= 0 { days } else { days - 146_096 } / 146_097;
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    };
    ((year + i64::from(month <= 2)) as i32, month as u32, day as u32)
}
