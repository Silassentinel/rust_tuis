//! `rustmon --summary`: a fast, shell-prompt-friendly one-liner.
//!
//! Built for something like an oh-my-posh command segment — run before
//! every prompt, so it must be well under 100ms. That rules out anything
//! this crate already knows can be slow: `disk`'s `fs-capacity` feature (up
//! to half a second per mount on a hanging filesystem, see
//! `collectors::disk`'s `STATVFS_TIMEOUT`) and a `/proc/<pid>` fd-table
//! walk (a future `connections` collector). So this module hardcodes
//! exactly `cpu` + `memory` + `thermal` — the three collectors with zero
//! exposure to either risk — regardless of any `--only`/`--skip` the user
//! also passed. `--summary` is not a restricted `--once`; it is its own
//! fixed, fast collection.
//!
//! Status/headline reuse the one severity concept the crate already has,
//! [`TempSeverity`], plus a non-empty [`Snapshot::errors`] (a collector
//! failing this run). Deliberately not inventing new thresholds for memory
//! or CPU usage — nothing elsewhere in this crate defines "high memory
//! usage," and doing so here first would be scope creep this module has no
//! business owning.
//!
//! When nothing is worth surfacing, [`run`] prints nothing and returns exit
//! code 1 — silence, not an empty JSON object — so a shell-prompt
//! integration that hides its segment on a nonzero exit code or empty
//! output (oh-my-posh's command segment does this natively) needs no extra
//! configuration to disappear when there's nothing to say.

use std::io::Write;
use std::path::PathBuf;

use crate::collector::{Collector, Registry};
use crate::collectors::cpu::CpuCollector;
use crate::collectors::memory::MemoryCollector;
use crate::collectors::thermal::ThermalCollector;
use crate::config::Config;
use crate::error::{Error, Result};
use crate::render::json::JsonWriter;
use crate::sample::{Snapshot, TempSeverity};
use crate::sysfs::SysfsReader;

/// Run `--summary`. Returns the process exit code directly (not wrapped in
/// the JSON body) — `0` if something was printed, `1` if there was nothing
/// worth surfacing.
pub fn run(config: &Config) -> Result<i32> {
    let reader = SysfsReader::with_root(config.sysfs_root.clone())?;

    let collectors: Vec<Box<dyn Collector>> = vec![
        Box::new(CpuCollector::new()),
        Box::new(MemoryCollector::new()),
        Box::new(ThermalCollector::new()),
    ];
    let mut registry = Registry::from_collectors(collectors, reader);
    let snapshot = registry.collect_all()?;

    let (status, headline) = summarise(&snapshot);
    if status == Status::Ok {
        return Ok(1);
    }

    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    write_summary(&mut handle, status, &headline)?;
    Ok(0)
}

fn write_summary(out: &mut dyn Write, status: Status, headline: &str) -> Result<()> {
    let mut w = JsonWriter::new(out);
    w.begin_object()?;
    w.key("status")?;
    w.str_value(status.as_str())?;
    w.key("headline")?;
    w.str_value(headline)?;
    w.end_object()?;
    // Same newline-terminated convention as `--once --format json`.
    out.write_all(b"\n").map_err(io_err)?;
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    Ok,
    Warning,
    Critical,
}

impl Status {
    fn as_str(self) -> &'static str {
        match self {
            Status::Ok => "ok",
            Status::Warning => "warning",
            Status::Critical => "critical",
        }
    }
}

/// Pure function of a [`Snapshot`] — no I/O, so it's testable against a
/// hand-built snapshot with no filesystem involved, same as every other
/// piece of derived logic in this crate.
fn summarise(snapshot: &Snapshot) -> (Status, String) {
    let mut critical = 0u32;
    let mut warning = 0u32;

    if let Some(thermal) = &snapshot.thermal {
        for chip in &thermal.chips {
            for t in &chip.temps {
                match t.severity() {
                    TempSeverity::Critical => critical += 1,
                    TempSeverity::Warning => warning += 1,
                    TempSeverity::Normal | TempSeverity::Unknown => {}
                }
            }
        }
    }

    let collector_errors = snapshot.errors.len();

    let status = if critical > 0 {
        Status::Critical
    } else if warning > 0 || collector_errors > 0 {
        Status::Warning
    } else {
        Status::Ok
    };

    let mut parts = Vec::new();
    if critical > 0 {
        parts.push(format!("{critical} critical"));
    }
    if warning > 0 {
        parts.push(plural(warning, "warning"));
    }
    if collector_errors > 0 {
        parts.push(plural(collector_errors as u32, "collector error"));
    }

    (status, parts.join(", "))
}

fn plural(n: u32, noun: &str) -> String {
    if n == 1 {
        format!("{n} {noun}")
    } else {
        format!("{n} {noun}s")
    }
}

/// Same reasoning as every other renderer's `io_err`: `out` is an arbitrary
/// [`Write`] (stdout in production, a `Vec<u8>` in tests), so there's no
/// real file path to report.
fn io_err(source: std::io::Error) -> Error {
    Error::Io {
        path: PathBuf::from("<output>"),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sample::{CollectorError, HwmonChip, TempSensor, ThermalSample};
    use crate::units::MilliCelsius;

    fn sensor(value: i64, max: Option<i64>, crit: Option<i64>) -> TempSensor {
        TempSensor {
            label: "test".to_string(),
            value: MilliCelsius::from_millidegrees(value),
            max: max.map(MilliCelsius::from_millidegrees),
            crit: crit.map(MilliCelsius::from_millidegrees),
        }
    }

    fn snapshot_with_temps(temps: Vec<TempSensor>) -> Snapshot {
        let mut s = Snapshot::now();
        s.thermal = Some(ThermalSample {
            chips: vec![HwmonChip {
                name: "chip0".to_string(),
                temps,
                fans: Vec::new(),
            }],
        });
        s
    }

    #[test]
    fn a_snapshot_with_nothing_notable_is_ok_with_no_headline() {
        let snapshot = snapshot_with_temps(vec![sensor(40_000, Some(90_000), Some(100_000))]);
        let (status, headline) = summarise(&snapshot);
        assert_eq!(status, Status::Ok);
        assert!(headline.is_empty());
    }

    #[test]
    fn an_empty_snapshot_is_ok() {
        assert_eq!(summarise(&Snapshot::now()).0, Status::Ok);
    }

    #[test]
    fn a_warning_sensor_yields_warning_status_and_a_count() {
        let snapshot = snapshot_with_temps(vec![sensor(90_000, Some(90_000), Some(100_000))]);
        let (status, headline) = summarise(&snapshot);
        assert_eq!(status, Status::Warning);
        assert_eq!(headline, "1 warning");
    }

    #[test]
    fn a_critical_sensor_outranks_warning_status() {
        let snapshot = snapshot_with_temps(vec![
            sensor(90_000, Some(90_000), Some(100_000)), // warning
            sensor(150_000, Some(90_000), Some(100_000)), // critical
        ]);
        let (status, headline) = summarise(&snapshot);
        assert_eq!(status, Status::Critical);
        assert_eq!(headline, "1 critical, 1 warning");
    }

    #[test]
    fn multiple_warnings_pluralise_correctly() {
        let snapshot = snapshot_with_temps(vec![
            sensor(90_000, Some(90_000), Some(100_000)),
            sensor(91_000, Some(90_000), Some(100_000)),
        ]);
        assert_eq!(summarise(&snapshot).1, "2 warnings");
    }

    /// A collector failing this run is worth surfacing even with every
    /// sensor reading normal — a `--summary` that stays silent while a
    /// collector is actually broken would defeat the point of the segment.
    #[test]
    fn a_collector_error_alone_yields_warning_status() {
        let mut snapshot = Snapshot::now();
        snapshot.errors.push(CollectorError {
            collector: "thermal",
            message: "boom".to_string(),
        });
        let (status, headline) = summarise(&snapshot);
        assert_eq!(status, Status::Warning);
        assert_eq!(headline, "1 collector error");
    }

    #[test]
    fn unknown_severity_sensors_do_not_count_as_warnings() {
        // No max/crit at all -> TempSeverity::Unknown, deliberately not
        // treated as noteworthy (see TempSensor::severity's own doc: "we
        // don't know" is a different claim from "this is fine", but it's
        // also a different claim from "this is a problem").
        let snapshot = snapshot_with_temps(vec![sensor(90_000, None, None)]);
        assert_eq!(summarise(&snapshot).0, Status::Ok);
    }

    #[test]
    fn write_summary_produces_valid_newline_terminated_json() {
        let mut buf = Vec::new();
        write_summary(&mut buf, Status::Warning, "2 warnings").expect("writes");
        let text = String::from_utf8(buf).expect("utf8");
        assert!(text.ends_with('\n'));
        assert!(text.contains("\"status\""));
        assert!(text.contains("\"warning\""));
        assert!(text.contains("\"headline\""));
        assert!(text.contains("2 warnings"));
    }
}
