//! Recurring cadence (daily/weekly) + due-date derivation. Ported from
//! `formatCadence`/`computeNextDue`/`formatDue` in
//! `website/features/ferment-tracker-app/server/fermentData.ts`. A
//! cadence/schedule concept is generic, not tied to any one domain.

use crate::datetime::DateTime;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cadence {
    Daily,
    Weekly,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Schedule {
    pub cadence: Cadence,
    pub interval: u32,
    pub time: Option<String>,
    pub day_of_week: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Good,
    Warn,
    Bad,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DueInfo {
    pub status: Status,
    pub status_text: String,
    pub due: String,
    /// Signed days until `next_due` (negative once overdue) — the same
    /// number `due`'s human-readable text is derived from, exposed
    /// separately so a caller can sort trackers by urgency without
    /// re-parsing "Overdue by 3d" / "Tue, in 5d" back into a number.
    /// `None` for a completed tracker, which has no next-due at all.
    pub due_days: Option<f64>,
}

fn join_non_empty(parts: &[Option<String>], sep: &str) -> String {
    parts
        .iter()
        .filter_map(|p| p.as_ref().filter(|s| !s.is_empty()))
        .cloned()
        .collect::<Vec<_>>()
        .join(sep)
}

pub fn format_cadence(schedule: &Schedule) -> String {
    match schedule.cadence {
        Cadence::Weekly => {
            let freq = if schedule.interval == 1 {
                "weekly".to_string()
            } else {
                format!("every {} weeks", schedule.interval)
            };
            let day = schedule
                .day_of_week
                .as_deref()
                .map(|d| d.chars().take(3).collect::<String>());
            let inner = join_non_empty(&[day, schedule.time.clone()], " ");
            join_non_empty(&[Some(freq), Some(inner)], " \u{b7} ")
        }
        Cadence::Daily => {
            let freq = if schedule.interval == 1 {
                "daily".to_string()
            } else {
                format!("every {} days", schedule.interval)
            };
            join_non_empty(&[Some(freq), schedule.time.clone()], " \u{b7} ")
        }
    }
}

fn parse_hh_mm(time: &str) -> (u32, u32) {
    let mut parts = time.split(':');
    let hh = parts.next().and_then(|s| s.parse::<u32>().ok()).unwrap_or(0);
    let mm = parts.next().and_then(|s| s.parse::<u32>().ok()).unwrap_or(0);
    (hh, mm)
}

/// Next-due counts forward from the last check-in (or, with no history yet,
/// from `created`) — never from "now", so a skipped check-in doesn't quietly
/// reset the clock.
pub fn compute_next_due(schedule: &Schedule, last_date: Option<DateTime>, created: DateTime) -> DateTime {
    let base = last_date.unwrap_or(created);
    let (hh, mm) = parse_hh_mm(schedule.time.as_deref().unwrap_or("09:00"));
    let days_to_add = match schedule.cadence {
        Cadence::Weekly => i64::from(schedule.interval) * 7,
        Cadence::Daily => i64::from(schedule.interval),
    };
    base.add_days(days_to_add).with_time(hh, mm)
}

pub fn format_due(next_due: DateTime, now: DateTime) -> DueInfo {
    let diff_days = next_due.diff_days(&now);
    let time = next_due.time_24h();

    let status = if diff_days < 0.0 {
        Status::Bad
    } else if diff_days <= 1.0 {
        Status::Warn
    } else {
        Status::Good
    };

    let due = if diff_days < 0.0 {
        let overdue_days = (-diff_days).ceil().max(1.0) as i64;
        format!("Overdue by {overdue_days}d")
    } else if next_due.same_calendar_day(&now) {
        format!("Today, {time}")
    } else {
        let tomorrow = now.add_days(1);
        if next_due.same_calendar_day(&tomorrow) {
            format!("Tomorrow, {time}")
        } else {
            format!("{}, in {}d", next_due.weekday_short(), diff_days.ceil() as i64)
        }
    };

    let status_text = match status {
        Status::Bad => "overdue".to_string(),
        Status::Warn => {
            if next_due.same_calendar_day(&now) {
                "due today".to_string()
            } else {
                "due soon".to_string()
            }
        }
        Status::Good => "on track".to_string(),
    };

    DueInfo { status, status_text, due, due_days: Some(diff_days) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dt(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> DateTime {
        DateTime::new(year, month, day, hour, minute)
    }

    mod format_cadence_tests {
        use super::*;

        #[test]
        fn says_weekly_rather_than_every_1_weeks() {
            let s = Schedule {
                cadence: Cadence::Weekly,
                interval: 1,
                time: Some("13:00".to_string()),
                day_of_week: Some("Saturday".to_string()),
            };
            assert_eq!(format_cadence(&s), "weekly \u{b7} Sat 13:00");
        }

        #[test]
        fn says_daily_rather_than_every_1_days() {
            let s = Schedule {
                cadence: Cadence::Daily,
                interval: 1,
                time: Some("20:00".to_string()),
                day_of_week: None,
            };
            assert_eq!(format_cadence(&s), "daily \u{b7} 20:00");
        }

        #[test]
        fn spells_out_multi_day_and_multi_week_intervals() {
            let daily = Schedule {
                cadence: Cadence::Daily,
                interval: 3,
                time: Some("08:00".to_string()),
                day_of_week: None,
            };
            assert_eq!(format_cadence(&daily), "every 3 days \u{b7} 08:00");

            let weekly = Schedule {
                cadence: Cadence::Weekly,
                interval: 2,
                time: Some("09:00".to_string()),
                day_of_week: Some("Monday".to_string()),
            };
            assert_eq!(format_cadence(&weekly), "every 2 weeks \u{b7} Mon 09:00");
        }

        #[test]
        fn omits_missing_parts_rather_than_printing_blanks() {
            let daily = Schedule { cadence: Cadence::Daily, interval: 1, time: None, day_of_week: None };
            assert_eq!(format_cadence(&daily), "daily");

            let weekly = Schedule { cadence: Cadence::Weekly, interval: 1, time: None, day_of_week: None };
            assert_eq!(format_cadence(&weekly), "weekly");
        }
    }

    mod compute_next_due_tests {
        use super::*;

        #[test]
        fn counts_forward_from_the_last_checkin() {
            let schedule = Schedule { cadence: Cadence::Daily, interval: 3, time: Some("08:00".to_string()), day_of_week: None };
            let next = compute_next_due(&schedule, Some(dt(2026, 7, 20, 12, 0)), dt(2026, 1, 1, 0, 0));
            assert_eq!((next.year, next.month, next.day, next.hour, next.minute), (2026, 7, 23, 8, 0));
        }

        #[test]
        fn falls_back_to_creation_date_when_there_is_no_history_yet() {
            let schedule = Schedule { cadence: Cadence::Weekly, interval: 1, time: Some("13:00".to_string()), day_of_week: None };
            let next = compute_next_due(&schedule, None, dt(2026, 7, 21, 0, 0));
            assert_eq!(next.day, 28);
            assert_eq!(next.hour, 13);
        }

        #[test]
        fn adds_whole_weeks_for_a_weekly_cadence() {
            let schedule = Schedule { cadence: Cadence::Weekly, interval: 2, time: Some("09:00".to_string()), day_of_week: None };
            let next = compute_next_due(&schedule, Some(dt(2026, 7, 1, 0, 0)), dt(2026, 1, 1, 0, 0));
            assert_eq!(next.day, 15);
        }
    }

    mod format_due_tests {
        use super::*;

        fn now() -> DateTime {
            dt(2026, 7, 28, 12, 0)
        }

        #[test]
        fn is_overdue_when_the_due_moment_has_passed() {
            let result = format_due(dt(2026, 7, 26, 8, 0), now());
            assert_eq!(result.status, Status::Bad);
            assert_eq!(result.status_text, "overdue");
            assert!(result.due.starts_with("Overdue by") && result.due.ends_with('d'));
            assert!(result.due_days.unwrap() < 0.0, "{:?}", result.due_days);
        }

        #[test]
        fn is_due_today_when_it_falls_later_the_same_day() {
            let result = format_due(dt(2026, 7, 28, 20, 0), now());
            assert_eq!(result.status, Status::Warn);
            assert_eq!(result.status_text, "due today");
            assert!(result.due.starts_with("Today, "));
            assert!(result.due_days.unwrap() >= 0.0, "{:?}", result.due_days);
        }

        #[test]
        fn is_on_track_when_comfortably_in_the_future() {
            let result = format_due(dt(2026, 8, 4, 13, 0), now());
            assert_eq!(result.status, Status::Good);
            assert_eq!(result.status_text, "on track");
            assert!(result.due.ends_with('d') && result.due.contains("in "));
            assert!(result.due_days.unwrap() > 1.0, "{:?}", result.due_days);
        }

        // Regression test for sorting by urgency: due_days must order the
        // same way the human-readable `due` text implies (more overdue =
        // more negative, further out = more positive), not just happen to
        // exist.
        #[test]
        fn due_days_orders_the_same_way_the_due_text_implies_urgency() {
            let very_overdue = format_due(dt(2026, 7, 20, 8, 0), now()).due_days.unwrap();
            let barely_overdue = format_due(dt(2026, 7, 27, 8, 0), now()).due_days.unwrap();
            let due_today = format_due(dt(2026, 7, 28, 20, 0), now()).due_days.unwrap();
            let due_next_week = format_due(dt(2026, 8, 4, 13, 0), now()).due_days.unwrap();
            assert!(very_overdue < barely_overdue, "{very_overdue} < {barely_overdue}");
            assert!(barely_overdue < due_today, "{barely_overdue} < {due_today}");
            assert!(due_today < due_next_week, "{due_today} < {due_next_week}");
        }

        #[test]
        fn names_tomorrow_explicitly() {
            let result = format_due(dt(2026, 7, 29, 20, 0), now());
            assert!(result.due.starts_with("Tomorrow, "));
        }

        #[test]
        fn never_reports_overdue_by_0d() {
            let result = format_due(dt(2026, 7, 28, 11, 55), now());
            assert_eq!(result.due, "Overdue by 1d");
        }
    }
}
