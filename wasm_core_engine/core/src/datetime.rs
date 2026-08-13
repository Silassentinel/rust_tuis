//! A minimal, dependency-free civil (proleptic Gregorian) date + time,
//! precise to the minute — just enough to port `computeNextDue`/`formatDue`,
//! which only ever operate on explicit `Date` values the caller supplies
//! (never the system clock). No date/time crate is approved yet (see
//! `docs/crate-checklist.md`), and none is needed for this: everything here
//! is pure calendar arithmetic.
//!
//! The day <-> civil-date conversion is Howard Hinnant's well-known
//! `days_from_civil`/`civil_from_days` algorithm (public domain), which
//! correctly handles the proleptic Gregorian calendar, including leap years,
//! with plain integer math.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DateTime {
    pub year: i32,
    pub month: u32, // 1-12
    pub day: u32,   // 1-31
    pub hour: u32,  // 0-23
    pub minute: u32, // 0-59
}

impl DateTime {
    pub fn new(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> Self {
        Self { year, month, day, hour, minute }
    }

    pub fn with_time(&self, hour: u32, minute: u32) -> Self {
        Self { hour, minute, ..*self }
    }

    pub fn add_days(&self, days: i64) -> Self {
        let (year, month, day) = civil_from_days(days_from_civil(self.year, self.month, self.day) + days);
        Self { year, month, day, hour: self.hour, minute: self.minute }
    }

    pub fn same_calendar_day(&self, other: &Self) -> bool {
        self.year == other.year && self.month == other.month && self.day == other.day
    }

    /// `self - other`, in days (fractional).
    pub fn diff_days(&self, other: &Self) -> f64 {
        let self_minutes = days_from_civil(self.year, self.month, self.day) * 24 * 60 + (self.hour * 60 + self.minute) as i64;
        let other_minutes = days_from_civil(other.year, other.month, other.day) * 24 * 60 + (other.hour * 60 + other.minute) as i64;
        (self_minutes - other_minutes) as f64 / (24.0 * 60.0)
    }

    pub fn weekday_short(&self) -> &'static str {
        const NAMES: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
        let days = days_from_civil(self.year, self.month, self.day);
        // 1970-01-01 (day 0) was a Thursday (index 4).
        let idx = (((days % 7) + 7 + 4) % 7) as usize;
        NAMES[idx]
    }

    pub fn time_24h(&self) -> String {
        format!("{:02}:{:02}", self.hour, self.minute)
    }

    /// Parses a `"YYYY-MM-DD"` date-only string — the shape ISO date
    /// strings take in `tracker.yml`'s `created:` field and check-in
    /// frontmatter/filenames in the ferment-tracker domain layer — into
    /// midnight of that day. `None` for anything that isn't exactly four
    /// digits, a dash, two digits, a dash, two digits, with an in-range
    /// month and day; never panics on malformed input, matching the rest
    /// of this crate's parse-and-skip posture (see `field_label.rs`,
    /// `mix.rs`). No calendar-length validation beyond `1..=31` — same
    /// looseness as the source, which hands the raw string to the
    /// platform's `Date` constructor without validating it either.
    pub fn parse_ymd(s: &str) -> Option<Self> {
        let mut parts = s.splitn(3, '-');
        let y = parts.next()?;
        let m = parts.next()?;
        let d = parts.next()?;
        if y.len() != 4 || m.len() != 2 || d.len() != 2 {
            return None;
        }
        let year = y.parse::<i32>().ok()?;
        let month = m.parse::<u32>().ok()?;
        let day = d.parse::<u32>().ok()?;
        if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
            return None;
        }
        Some(Self { year, month, day, hour: 0, minute: 0 })
    }
}

fn days_from_civil(y: i32, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y as i64 - 1 } else { y as i64 };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let mp = (m as i64 + 9) % 12; // [0, 11]
    let doy = (153 * mp + 2) / 5 + d as i64 - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146097 + doe - 719468
}

fn civil_from_days(z: i64) -> (i32, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32; // [1, 12]
    let y = if m <= 2 { y + 1 } else { y };
    (y as i32, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Implementation sanity checks for the Hinnant algorithm port — not from
    // the source TS test suite (which relies on the native `Date` object and
    // never exercises this arithmetic directly), but necessary since Rust
    // has no built-in equivalent.
    #[test]
    fn epoch_round_trips() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(civil_from_days(0), (1970, 1, 1));
    }

    #[test]
    fn epoch_day_is_a_thursday() {
        assert_eq!(DateTime::new(1970, 1, 1, 0, 0).weekday_short(), "Thu");
    }

    #[test]
    fn add_days_rolls_over_a_leap_year_february() {
        let d = DateTime::new(2024, 2, 28, 0, 0).add_days(1);
        assert_eq!((d.year, d.month, d.day), (2024, 2, 29));
    }

    #[test]
    fn add_days_rolls_over_a_non_leap_year_february() {
        let d = DateTime::new(2023, 2, 28, 0, 0).add_days(1);
        assert_eq!((d.year, d.month, d.day), (2023, 3, 1));
    }

    #[test]
    fn add_days_rolls_over_a_year_boundary() {
        let d = DateTime::new(2025, 12, 31, 0, 0).add_days(1);
        assert_eq!((d.year, d.month, d.day), (2026, 1, 1));
    }

    mod parse_ymd_tests {
        use super::*;

        #[test]
        fn parses_a_well_formed_date_to_midnight() {
            let d = DateTime::parse_ymd("2026-07-01").unwrap();
            assert_eq!((d.year, d.month, d.day, d.hour, d.minute), (2026, 7, 1, 0, 0));
        }

        #[test]
        fn rejects_missing_leading_zeros() {
            assert_eq!(DateTime::parse_ymd("2026-7-1"), None);
        }

        #[test]
        fn rejects_an_out_of_range_month_or_day() {
            assert_eq!(DateTime::parse_ymd("2026-13-01"), None);
            assert_eq!(DateTime::parse_ymd("2026-01-32"), None);
            assert_eq!(DateTime::parse_ymd("2026-00-01"), None);
        }

        #[test]
        fn rejects_non_numeric_or_malformed_input() {
            assert_eq!(DateTime::parse_ymd("not-a-date"), None);
            assert_eq!(DateTime::parse_ymd("2026-07-01T00:00:00Z"), None);
            assert_eq!(DateTime::parse_ymd(""), None);
        }
    }
}
