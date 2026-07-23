//! Formats a `SystemTime` as a UTC `YYYY-MM-DDTHH:MM:SSZ` string, entirely
//! with `std` - no time-formatting crate is in the tree, and per the
//! crate-checklist rule in `CLAUDE.md` one isn't getting added just to
//! print a log timestamp. The date part uses Howard Hinnant's
//! `civil_from_days` algorithm (a small, well-known piece of integer math
//! for converting a day count since the Unix epoch into a proleptic
//! Gregorian calendar date - see
//! <http://howardhinnant.github.io/date_algorithms.html>), which is easy
//! to unit-test against known reference dates rather than trusting the
//! arithmetic by inspection.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Converts a day count since 1970-01-01 (may be negative, for dates
/// before the epoch, though rustlogger will never see one) into a
/// `(year, month, day)` proleptic Gregorian calendar date.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // day of era, [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // year of era, [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // day of year, [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    let year = if month <= 2 { y + 1 } else { y };
    (year, month, day)
}

fn ymd_hms(t: SystemTime) -> (i64, u32, u32, i64, i64, i64) {
    let secs = t
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs() as i64;
    let days = secs.div_euclid(86_400);
    let time_of_day = secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = time_of_day / 3600;
    let minute = (time_of_day % 3600) / 60;
    let second = time_of_day % 60;
    (year, month, day, hour, minute, second)
}

/// Formats `t` as `YYYY-MM-DDTHH:MM:SSZ` (UTC). Falls back to the epoch if
/// `t` somehow predates it (never happens in practice - all timestamps
/// here come from `SystemTime::now()`).
pub fn format_utc(t: SystemTime) -> String {
    let (year, month, day, hour, minute, second) = ymd_hms(t);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// Formats `t` as `YYYYMMDD-HHMMSS` (UTC) - no colons or punctuation, for
/// use in a log file name.
pub fn format_utc_compact(t: SystemTime) -> String {
    let (year, month, day, hour, minute, second) = ymd_hms(t);
    format!("{year:04}{month:02}{day:02}-{hour:02}{minute:02}{second:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    // Expected strings cross-checked against `date -u -d @<epoch>`.
    #[test]
    fn matches_known_reference_dates() {
        let cases: &[(u64, &str)] = &[
            (0, "1970-01-01T00:00:00Z"),
            (1, "1970-01-01T00:00:01Z"),
            (86_399, "1970-01-01T23:59:59Z"),
            (86_400, "1970-01-02T00:00:00Z"),
            (1_700_000_000, "2023-11-14T22:13:20Z"),
            (1_712_345_678, "2024-04-05T19:34:38Z"),
            (951_782_400, "2000-02-29T00:00:00Z"), // leap day
            (1_735_689_599, "2024-12-31T23:59:59Z"),
        ];

        for &(secs, expected) in cases {
            let t = UNIX_EPOCH + Duration::from_secs(secs);
            assert_eq!(format_utc(t), expected, "mismatch for epoch {secs}");
        }
    }

    #[test]
    fn compact_form_matches_the_iso_form_digit_for_digit() {
        let t = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        assert_eq!(format_utc(t), "2023-11-14T22:13:20Z");
        assert_eq!(format_utc_compact(t), "20231114-221320");
    }
}
