//! CPU: utilisation counters, frequency, load average, model name.
//!
//! Sources:
//! - `proc/stat` — jiffy counters, aggregate `cpu ` line plus one `cpuN` line
//!   per logical core. Monotonic; utilisation is a delta ([`crate::delta`]).
//! - `proc/loadavg` — the three load averages.
//! - `proc/cpuinfo` — model name, for the header.
//! - `sys/devices/system/cpu/cpuN/cpufreq/scaling_cur_freq` — current kHz.
//!   Absent on many systems (no cpufreq driver, or a VM); that's `None`, not an
//!   error.
//!
//! The one non-obvious correctness trap: the `guest` and `guest_nice` fields of
//! `/proc/stat` are **already included** in `user` and `nice`. Summing all ten
//! fields double-counts them and understates busy% on a VM host. See
//! [`crate::sample::CpuTimes::total`].

//! # Which failures are fatal to this collector
//!
//! `/proc/stat` is the point of the collector, so a parse failure there
//! propagates and the registry records the whole collector as failed.
//! Everything else here — load average, model name, per-core frequency — is a
//! decoration, and losing it must not cost the caller the utilisation counters
//! it actually came for. Those failures are pushed onto
//! [`Snapshot::errors`][crate::sample::Snapshot::errors] so `--verbose` can
//! show them, and collection continues.

use std::path::{Path, PathBuf};

use crate::collector::Collector;
use crate::error::{Error, Result};
use crate::sample::{CollectorError, CpuSample, CpuTimes, Snapshot};
use crate::sysfs::{sanitize_kernel_string, SysfsReader, DEFAULT_MAX_LINES};
use crate::units::KiloHertz;

pub const NAME: &str = "cpu";

const PROC_STAT: &str = "proc/stat";
const PROC_LOADAVG: &str = "proc/loadavg";
const PROC_CPUINFO: &str = "proc/cpuinfo";
const SYS_CPU_DIR: &str = "sys/devices/system/cpu";

/// Upper bound on the core index we will honour from `/proc/stat`.
///
/// Cores are indexed by the number in `cpuN`, and that number comes from a
/// file. Without a cap, a line reading `cpu4000000000 ...` would ask us to
/// allocate a four-billion-entry vector. Linux's own `CONFIG_NR_CPUS` maxes
/// out well below this on any real build. Security model item 8.
const MAX_CORES: usize = 4_096;

/// The four fields (`user`, `nice`, `system`, `idle`) every kernel that has
/// ever shipped `/proc/stat` reports. Fields beyond these were added over
/// time, so missing *trailing* fields are tolerated and default to zero —
/// but a line with fewer than four is malformed, not old.
const MIN_CPU_FIELDS: usize = 4;

/// Cap on a model-name string before it reaches a terminal.
const MAX_MODEL_LEN: usize = 128;

#[derive(Debug, Default)]
pub struct CpuCollector {
    /// Cached core count, so we don't re-enumerate `sys/devices/system/cpu`
    /// on every refresh.
    core_count: Option<usize>,
    /// Cached model string — it never changes at runtime.
    model: Option<String>,
}

impl CpuCollector {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a non-fatal sub-read failure and carry on. See the module note
    /// on which failures are fatal.
    fn note(snapshot: &mut Snapshot, e: Error) {
        snapshot.errors.push(CollectorError {
            collector: NAME,
            message: e.to_string(),
        });
    }
}

impl Collector for CpuCollector {
    fn name(&self) -> &'static str {
        NAME
    }

    fn probe(&self, reader: &SysfsReader) -> bool {
        reader.exists(Path::new(PROC_STAT))
    }

    fn collect(&mut self, reader: &SysfsReader, snapshot: &mut Snapshot) -> Result<()> {
        let stat_path = Path::new(PROC_STAT);
        let stat = match reader.read_to_string(stat_path)? {
            Ok(contents) => contents,
            // No `/proc/stat` at all: nothing to report, which is absence
            // rather than failure.
            Err(_) => return Ok(()),
        };

        let (total, per_core) = parse_proc_stat(stat_path, &stat)?;

        // Core count never changes at runtime short of hotplug, and
        // re-enumerating `/sys` every refresh would be the most expensive
        // thing this collector does. `per_core.len()` is the floor because
        // `/proc/stat` is authoritative about how many we actually parsed.
        let core_count = match self.core_count {
            Some(cached) => cached.max(per_core.len()),
            None => {
                let counted = count_cores(reader)?.max(per_core.len());
                self.core_count = Some(counted);
                counted
            }
        };

        let freq_khz = read_frequencies(reader, core_count)?;

        let load_avg = match reader.read_to_string(Path::new(PROC_LOADAVG)) {
            Ok(Ok(contents)) => match parse_loadavg(Path::new(PROC_LOADAVG), &contents) {
                Ok(load) => Some(load),
                Err(e) => {
                    Self::note(snapshot, e);
                    None
                }
            },
            Ok(Err(_)) => None,
            Err(e) => {
                Self::note(snapshot, e);
                None
            }
        };

        // The model string never changes, so read it once and keep it.
        if self.model.is_none() {
            match reader.read_to_string(Path::new(PROC_CPUINFO)) {
                Ok(Ok(contents)) => self.model = parse_model_name(&contents),
                Ok(Err(_)) => {}
                Err(e) => Self::note(snapshot, e),
            }
        }

        snapshot.cpu = Some(CpuSample {
            total,
            per_core,
            freq_khz,
            load_avg,
            model: self.model.clone(),
            ctxt: parse_scalar_line(&stat, "ctxt"),
            btime: parse_scalar_line(&stat, "btime"),
        });

        Ok(())
    }
}

/// Pull a bare `key value` scalar out of `/proc/stat` (`ctxt`, `btime`).
///
/// Absent or unparseable is `None`: these drive an uptime display, and a
/// missing one is not worth failing a CPU reading over.
fn parse_scalar_line(contents: &str, key: &str) -> Option<u64> {
    contents
        .lines()
        .take(DEFAULT_MAX_LINES)
        .filter_map(|line| line.split_once(char::is_whitespace))
        .find(|(name, _)| *name == key)
        .and_then(|(_, value)| value.trim().parse::<u64>().ok())
}

/// Parse `/proc/stat` into the aggregate line and the per-core lines.
///
/// Returns `(total, per_core)`. Cores are indexed by the number in `cpuN`, not
/// by line order — offline cores leave gaps, and assuming contiguity would
/// misattribute counters after a core is hot-unplugged.
pub fn parse_proc_stat(path: &Path, contents: &str) -> Result<(CpuTimes, Vec<CpuTimes>)> {
    let mut total: Option<CpuTimes> = None;
    let mut cores: Vec<Option<CpuTimes>> = Vec::new();

    for (idx, line) in contents.lines().take(DEFAULT_MAX_LINES).enumerate() {
        let line_no = idx + 1;

        let Some(rest) = line.strip_prefix("cpu") else {
            continue;
        };

        // `cpu  1 2 3` (two spaces) is the aggregate; `cpu0 1 2 3` is a core.
        if let Some(fields) = rest.strip_prefix(' ') {
            total = Some(parse_cpu_times(path, line_no, fields)?);
            continue;
        }

        let Some((index_text, fields)) = rest.split_once(char::is_whitespace) else {
            // A bare `cpu` with no fields at all. Nothing to attribute.
            continue;
        };

        // `/proc/stat` has no other line starting with `cpu`, so anything that
        // isn't `cpu<number>` here is a malformed file rather than a line we
        // should be skipping.
        let Ok(index) = index_text.parse::<usize>() else {
            return Err(Error::parse(
                path,
                Some(line_no),
                format!(
                    "expected a cpu index, got {:?}",
                    sanitize_kernel_string(index_text, 16)
                ),
            ));
        };

        if index >= MAX_CORES {
            return Err(Error::parse(
                path,
                Some(line_no),
                format!("cpu index {index} exceeds the {MAX_CORES}-core cap"),
            ));
        }

        let times = parse_cpu_times(path, line_no, fields)?;

        // Indexed by core number, not line order: an offline core leaves a gap
        // in the numbering, and packing them by order would silently
        // reattribute every later core's counters to the wrong CPU.
        if cores.len() <= index {
            cores.resize(index + 1, None);
        }
        if let Some(slot) = cores.get_mut(index) {
            *slot = Some(times);
        }
    }

    let total = total.ok_or_else(|| {
        Error::parse(path, None, "no aggregate `cpu ` line")
    })?;

    // A gap (offline core) becomes a zeroed entry. Its `total()` is then zero,
    // which `Percent::from_ratio` already reports as "no reading" rather than
    // as 0% busy.
    let per_core = cores.into_iter().map(Option::unwrap_or_default).collect();

    Ok((total, per_core))
}

/// Parse one `cpu`/`cpuN` line's fields into [`CpuTimes`].
pub fn parse_cpu_times(path: &Path, line_no: usize, fields: &str) -> Result<CpuTimes> {
    let mut values = [0u64; 10];
    let mut seen = 0usize;

    // Zipping against the fixed-size array bounds the loop at 10 fields and
    // removes any indexing by a parsed value. Extra fields from a future
    // kernel are ignored rather than being an error.
    for (idx, (slot, field)) in values
        .iter_mut()
        .zip(fields.split_whitespace())
        .enumerate()
    {
        *slot = field.parse::<u64>().map_err(|_| {
            Error::parse(
                path,
                Some(line_no),
                format!(
                    "field {} is not an unsigned integer: {:?}",
                    idx + 1,
                    sanitize_kernel_string(field, 32)
                ),
            )
        })?;
        seen = idx + 1;
    }

    if seen < MIN_CPU_FIELDS {
        return Err(Error::parse(
            path,
            Some(line_no),
            format!("expected at least {MIN_CPU_FIELDS} fields, got {seen}"),
        ));
    }

    Ok(CpuTimes {
        user: values[0],
        nice: values[1],
        system: values[2],
        idle: values[3],
        iowait: values[4],
        irq: values[5],
        softirq: values[6],
        steal: values[7],
        guest: values[8],
        guest_nice: values[9],
    })
}

/// Parse `/proc/loadavg`'s first three whitespace-separated floats.
pub fn parse_loadavg(path: &Path, contents: &str) -> Result<[f64; 3]> {
    let line = contents.lines().next().unwrap_or("");
    let mut load = [0.0f64; 3];
    let mut seen = 0usize;

    for (idx, (slot, field)) in load.iter_mut().zip(line.split_whitespace()).enumerate() {
        let parsed = field.parse::<f64>().map_err(|_| {
            Error::parse(
                path,
                Some(1),
                format!(
                    "load average {} is not a number: {:?}",
                    idx + 1,
                    sanitize_kernel_string(field, 16)
                ),
            )
        })?;

        // `"nan"` and `"inf"` parse happily as f64. Letting either through
        // would put a non-finite straight into the renderer.
        if !parsed.is_finite() {
            return Err(Error::parse(
                path,
                Some(1),
                format!("load average {} is not finite", idx + 1),
            ));
        }

        *slot = parsed;
        seen = idx + 1;
    }

    if seen < 3 {
        return Err(Error::parse(
            path,
            Some(1),
            format!("expected 3 load averages, got {seen}"),
        ));
    }

    Ok(load)
}

/// Pull `model name` out of `/proc/cpuinfo`, sanitised for display.
pub fn parse_model_name(contents: &str) -> Option<String> {
    for line in contents.lines().take(DEFAULT_MAX_LINES) {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };

        if key.trim() != "model name" {
            continue;
        }

        // Sanitised because this string goes straight to a terminal, and
        // `/proc/cpuinfo` on some virtualised hosts reports whatever the
        // hypervisor chose to put there.
        let cleaned = sanitize_kernel_string(value.trim(), MAX_MODEL_LEN);
        return if cleaned.is_empty() { None } else { Some(cleaned) };
    }

    None
}

/// Read `scaling_cur_freq` for each core.
///
/// `Vec` index = core number; `None` where no cpufreq driver is present.
/// Never errors on a missing file — most VMs have none.
pub fn read_frequencies(reader: &SysfsReader, core_count: usize) -> Result<Vec<Option<KiloHertz>>> {
    let count = core_count.min(MAX_CORES);
    let mut freqs = Vec::with_capacity(count);

    for core in 0..count {
        let rel = PathBuf::from(format!("{SYS_CPU_DIR}/cpu{core}/cpufreq/scaling_cur_freq"));

        freqs.push(match reader.read_u64(&rel) {
            Ok(Ok(khz)) => Some(KiloHertz::from_khz(khz)),
            // Absent (no cpufreq driver, which is most VMs) or unparseable.
            // Either way frequency is a decoration, and `Option` already says
            // "unknown" — losing it must not cost the caller the utilisation
            // counters this collector exists for.
            Ok(Err(_)) | Err(_) => None,
        });
    }

    Ok(freqs)
}

/// Count logical cores by enumerating `sys/devices/system/cpu/cpuN`.
///
/// Falls back to counting `cpuN` lines in `/proc/stat` if `/sys` is masked
/// (which happens in some container configurations).
pub fn count_cores(reader: &SysfsReader) -> Result<usize> {
    // `cpuN` only — the directory also holds `cpufreq`, `cpuidle`, `online`,
    // `possible` and friends, none of which are cores.
    let is_core_dir = |name: &str| {
        name.strip_prefix("cpu")
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
    };

    if let Ok(Ok(names)) = reader.list_dir(Path::new(SYS_CPU_DIR), &is_core_dir) {
        if !names.is_empty() {
            return Ok(names.len().min(MAX_CORES));
        }
    }

    // `/sys` is masked in some container configurations. `/proc/stat` is
    // almost always still there, and it already tells us how many cores the
    // kernel is accounting for.
    let contents = match reader.read_to_string(Path::new(PROC_STAT))? {
        Ok(contents) => contents,
        Err(_) => return Ok(0),
    };

    let counted = contents
        .lines()
        .take(DEFAULT_MAX_LINES)
        .filter(|line| {
            line.strip_prefix("cpu")
                .is_some_and(|rest| rest.starts_with(|c: char| c.is_ascii_digit()))
        })
        .count();

    Ok(counted.min(MAX_CORES))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Captured from a real 24-thread AMD box on 2026-08-04, trimmed to three
    /// cores. The `guest` column is deliberately non-zero — a fixture of all
    /// zeroes there could not tell a correct `total()` from one that
    /// double-counts.
    const REAL_PROC_STAT: &str = "\
cpu  716788 2411 217900 54326362 611468 0 7764 0 3632 0
cpu0 34662 182 13033 2248192 27383 0 6306 0 198 0
cpu1 225997 340 39071 1809314 244553 0 822 0 1126 0
cpu2 168489 571 35741 2008529 107718 0 328 0 857 0
intr 1234567 0 0 0
ctxt 347270246
btime 1785853821
processes 47471
procs_running 2
procs_blocked 0
";

    fn p() -> &'static Path {
        Path::new("proc/stat")
    }

    #[test]
    fn parses_a_real_proc_stat() {
        let (total, per_core) = parse_proc_stat(p(), REAL_PROC_STAT).expect("real fixture parses");

        assert_eq!(total.user, 716_788);
        assert_eq!(total.idle, 54_326_362);
        assert_eq!(total.guest, 3_632);
        assert_eq!(per_core.len(), 3);
        assert_eq!(per_core[0].user, 34_662);
        assert_eq!(per_core[2].softirq, 328);
    }

    #[test]
    fn scalar_lines_are_picked_out_of_proc_stat() {
        assert_eq!(parse_scalar_line(REAL_PROC_STAT, "ctxt"), Some(347_270_246));
        assert_eq!(parse_scalar_line(REAL_PROC_STAT, "btime"), Some(1_785_853_821));
        assert_eq!(parse_scalar_line(REAL_PROC_STAT, "nosuchkey"), None);
    }

    /// Cores are indexed by the number in `cpuN`. Packing them by line order
    /// would hand core 5's counters to core 1 the moment a core goes offline.
    #[test]
    fn offline_cores_leave_gaps_rather_than_shifting_indices() {
        let stat = "\
cpu  100 0 0 100 0 0 0 0 0 0
cpu0 10 0 0 10 0 0 0 0 0 0
cpu3 40 0 0 40 0 0 0 0 0 0
";
        let (_, per_core) = parse_proc_stat(p(), stat).expect("parses");

        assert_eq!(per_core.len(), 4);
        assert_eq!(per_core[0].user, 10);
        // The two missing cores read as zeroed, not as core 3's numbers.
        assert_eq!(per_core[1], CpuTimes::default());
        assert_eq!(per_core[2], CpuTimes::default());
        assert_eq!(per_core[3].user, 40);
    }

    #[test]
    fn a_truncated_line_is_a_parse_error_not_a_panic() {
        // Line cut off mid-file, as a torn read of /proc would give.
        let stat = "\
cpu  716788 2411 217900 54326362 611468 0 7764 0 3632 0
cpu0 34662 182 130";

        // Three fields is below the four every kernel has always reported.
        let err = parse_proc_stat(p(), stat).expect_err("truncated line must fail");
        assert!(matches!(err, Error::Parse { line: Some(2), .. }), "got {err:?}");
    }

    #[test]
    fn a_non_numeric_field_is_a_parse_error() {
        let stat = "cpu  716788 2411 banana 54326362 611468 0 7764 0 3632 0\n";
        let err = parse_proc_stat(p(), stat).expect_err("garbage field must fail");
        assert!(matches!(err, Error::Parse { line: Some(1), .. }), "got {err:?}");

        // ...and the garbage must not travel into the message unsanitised.
        let rendered = parse_proc_stat(p(), "cpu  1 2 \u{1b}[2J3 4\n")
            .expect_err("escape must fail")
            .to_string();
        assert!(!rendered.contains('\u{1b}'), "escape survived: {rendered:?}");
    }

    /// The specific case the TODO calls out: a value too large for u64 must be
    /// a parse error, never a wrap and never a panic.
    #[test]
    fn a_value_overflowing_u64_is_a_parse_error() {
        let stat = format!("cpu  {} 2411 217900 54326362 0 0 0 0 0 0\n", u128::from(u64::MAX) + 1);
        let err = parse_proc_stat(p(), &stat).expect_err("u64 overflow must fail");
        assert!(matches!(err, Error::Parse { .. }), "got {err:?}");

        // The largest value that does fit must still parse.
        let stat = format!("cpu  {} 0 0 0 0 0 0 0 0 0\n", u64::MAX);
        let (total, _) = parse_proc_stat(p(), &stat).expect("u64::MAX parses");
        assert_eq!(total.user, u64::MAX);
    }

    #[test]
    fn missing_trailing_fields_default_to_zero() {
        // A pre-2.6 kernel reported only four columns.
        let (total, _) = parse_proc_stat(p(), "cpu  100 20 30 40\n").expect("four fields parse");
        assert_eq!(total.user, 100);
        assert_eq!(total.idle, 40);
        assert_eq!(total.steal, 0);
        assert_eq!(total.guest_nice, 0);
    }

    #[test]
    fn a_file_with_no_aggregate_line_is_an_error() {
        let err = parse_proc_stat(p(), "cpu0 1 2 3 4\nctxt 5\n").expect_err("no `cpu ` line");
        assert!(matches!(err, Error::Parse { line: None, .. }), "got {err:?}");
    }

    #[test]
    fn an_absurd_core_index_is_refused_rather_than_allocated() {
        let stat = "cpu  1 2 3 4\ncpu4000000000 1 2 3 4\n";
        let err = parse_proc_stat(p(), stat).expect_err("huge index must be refused");
        assert!(matches!(err, Error::Parse { .. }), "got {err:?}");
    }

    #[test]
    fn a_cpu_line_with_a_non_numeric_index_is_an_error() {
        let err = parse_proc_stat(p(), "cpu  1 2 3 4\ncpuX 1 2 3 4\n").expect_err("bad index");
        assert!(matches!(err, Error::Parse { line: Some(2), .. }), "got {err:?}");
    }

    // ---- load average ------------------------------------------------------

    #[test]
    fn parses_a_real_loadavg() {
        let load = parse_loadavg(Path::new("proc/loadavg"), "1.52 1.52 1.52 2/2535 47452\n")
            .expect("real fixture parses");
        assert_eq!(load, [1.52, 1.52, 1.52]);
    }

    #[test]
    fn a_short_or_garbled_loadavg_is_a_parse_error() {
        let path = Path::new("proc/loadavg");
        assert!(parse_loadavg(path, "1.52 1.52\n").is_err(), "only two values");
        assert!(parse_loadavg(path, "").is_err(), "empty file");
        assert!(parse_loadavg(path, "x y z\n").is_err(), "non-numeric");
        // "nan" and "inf" parse as f64 and must still be rejected.
        assert!(parse_loadavg(path, "nan 1.0 1.0\n").is_err(), "NaN accepted");
        assert!(parse_loadavg(path, "inf 1.0 1.0\n").is_err(), "inf accepted");
    }

    // ---- model name --------------------------------------------------------

    #[test]
    fn parses_and_sanitises_the_model_name() {
        let cpuinfo = "\
processor\t: 0
vendor_id\t: AuthenticAMD
model name\t: AMD Ryzen 9 9900X 12-Core Processor
cpu MHz\t\t: 4400.000
";
        assert_eq!(
            parse_model_name(cpuinfo).as_deref(),
            Some("AMD Ryzen 9 9900X 12-Core Processor")
        );

        // A hypervisor-supplied name is attacker-influenced.
        let hostile = "model name\t: \u{1b}[2JEvil\u{1b}[0m CPU\n";
        let got = parse_model_name(hostile).expect("still yields a name");
        assert!(!got.contains('\u{1b}'), "escape survived: {got:?}");
        assert_eq!(got, "Evil CPU");

        // ARM and friends have no `model name` line at all.
        assert_eq!(parse_model_name("processor\t: 0\n"), None);
        assert_eq!(parse_model_name("model name\t:   \n"), None);
    }
}
