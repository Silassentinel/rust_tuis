//! Memory and swap, from `proc/meminfo`.
//!
//! Two things to get right:
//!
//! 1. **The file reports kB, not bytes.** Every value goes through
//!    [`crate::units::Bytes::from_kib`]; nothing constructs `Bytes` directly
//!    from a meminfo number.
//! 2. **"Used" means `MemTotal - MemAvailable`, not `MemTotal - MemFree`.**
//!    `MemFree` excludes reclaimable page cache, so using it reports a Linux box
//!    as ~95% full at idle. `MemAvailable` is the kernel's own estimate of what
//!    a new allocation could actually get, and it's what every correct tool
//!    shows.

use std::path::Path;

use crate::collector::Collector;
use crate::error::{Error, Result};
use crate::sample::{MemorySample, Snapshot};
use crate::sysfs::{sanitize_kernel_string, SysfsReader, DEFAULT_MAX_LINES};
use crate::units::Bytes;

pub const NAME: &str = "memory";

const PROC_MEMINFO: &str = "proc/meminfo";

#[derive(Debug, Default)]
pub struct MemoryCollector;

impl MemoryCollector {
    pub fn new() -> Self {
        // Unit struct — there is nothing to cache between refreshes, unlike
        // `CpuCollector`, which holds the core count and model string.
        MemoryCollector
    }
}

impl Collector for MemoryCollector {
    fn name(&self) -> &'static str {
        NAME
    }

    fn probe(&self, reader: &SysfsReader) -> bool {
        reader.exists(Path::new(PROC_MEMINFO))
    }

    fn collect(&mut self, reader: &SysfsReader, snapshot: &mut Snapshot) -> Result<()> {
        let path = Path::new(PROC_MEMINFO);

        let contents = match reader.read_to_string(path)? {
            Ok(contents) => contents,
            // No `/proc/meminfo`: absence, not failure.
            Err(_) => return Ok(()),
        };

        snapshot.memory = Some(parse_meminfo(path, &contents)?);
        Ok(())
    }
}

/// Parse `/proc/meminfo` into a [`MemorySample`].
///
/// Lines look like `MemTotal:       16269844 kB`. Unrecognised keys are ignored
/// (the file grows between kernel versions). `MemAvailable` is absent on kernels
/// before 3.14 — fall back to `MemFree + Buffers + Cached` and note it, rather
/// than failing.
pub fn parse_meminfo(path: &Path, contents: &str) -> Result<MemorySample> {
    let mut sample = MemorySample::default();
    let mut available: Option<Bytes> = None;
    let mut seen_total = false;

    for (idx, line) in contents.lines().take(DEFAULT_MAX_LINES).enumerate() {
        if line.trim().is_empty() {
            continue;
        }

        // `None` means the line was well-formed but carried no `kB` suffix,
        // i.e. a count rather than a size. Skipping is the correct handling.
        let Some((key, bytes)) = parse_meminfo_line(path, idx + 1, line)? else {
            continue;
        };

        match key.as_str() {
            "MemTotal" => {
                sample.total = bytes;
                seen_total = true;
            }
            "MemFree" => sample.free = bytes,
            "MemAvailable" => available = Some(bytes),
            "Buffers" => sample.buffers = bytes,
            "Cached" => sample.cached = bytes,
            "SwapTotal" => sample.swap_total = bytes,
            "SwapFree" => sample.swap_free = bytes,
            // The file grows between kernel versions; unknown keys are normal.
            _ => {}
        }
    }

    if !seen_total {
        return Err(Error::parse(path, None, "no MemTotal line"));
    }

    // `MemAvailable` arrived in Linux 3.14. Older kernels get the classic
    // approximation rather than a failure — and specifically not `MemFree`,
    // which excludes reclaimable page cache and would report an idle Linux
    // box as ~95% full.
    sample.available = available.unwrap_or_else(|| {
        Bytes::from_bytes(
            sample
                .free
                .as_u64()
                .saturating_add(sample.buffers.as_u64())
                .saturating_add(sample.cached.as_u64()),
        )
    });

    Ok(sample)
}

/// Parse one `Key:  value kB` line into `(key, Bytes)`.
///
/// A handful of meminfo keys (`HugePages_Total` and friends) have **no unit
/// suffix** and are counts, not sizes. Returning `Ok(None)` for those is
/// correct; treating a page count as kB would be a 4096x error.
pub fn parse_meminfo_line(path: &Path, line_no: usize, line: &str) -> Result<Option<(String, Bytes)>> {
    let Some((key, rest)) = line.split_once(':') else {
        return Err(Error::parse(
            path,
            Some(line_no),
            format!(
                "expected `Key: value kB`, got {:?}",
                sanitize_kernel_string(line, 64)
            ),
        ));
    };

    let key = key.trim();
    if key.is_empty() {
        return Err(Error::parse(path, Some(line_no), "empty key"));
    }

    let mut fields = rest.split_whitespace();

    let Some(value_text) = fields.next() else {
        return Err(Error::parse(
            path,
            Some(line_no),
            format!(
                "key {:?} has no value",
                sanitize_kernel_string(key, 32)
            ),
        ));
    };

    let value = value_text.parse::<u64>().map_err(|_| {
        Error::parse(
            path,
            Some(line_no),
            format!(
                "value {:?} is not an unsigned integer",
                sanitize_kernel_string(value_text, 32)
            ),
        )
    })?;

    match fields.next() {
        // The `kB` suffix is the only thing that makes this line a size.
        Some(unit) if unit.eq_ignore_ascii_case("kB") => {
            Ok(Some((key.to_string(), Bytes::from_kib(value)?)))
        }
        // No suffix means a count, not a size — `HugePages_Total` and
        // friends. Multiplying one of those by 1024 would be silently,
        // plausibly wrong, which is the worst kind of wrong.
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Captured from a real 64 GiB machine on 2026-08-04. `HugePages_Total`
    /// is kept because it is the unit-less line that must not be read as kB.
    const REAL_MEMINFO: &str = "\
MemTotal:       63310204 kB
MemFree:         1842488 kB
MemAvailable:   52048372 kB
Buffers:         1710008 kB
Cached:         47800420 kB
SwapCached:            0 kB
SwapTotal:       8388604 kB
SwapFree:        8387912 kB
HugePages_Total:       0
Hugepagesize:       2048 kB
";

    fn p() -> &'static Path {
        Path::new("proc/meminfo")
    }

    #[test]
    fn parses_a_real_meminfo_in_kib_not_bytes() {
        let m = parse_meminfo(p(), REAL_MEMINFO).expect("real fixture parses");

        // 63310204 kB is 64,829,648,896 bytes. Reading the number as bytes
        // would be the 1024x bug this whole units layer exists to prevent.
        assert_eq!(m.total.as_u64(), 63_310_204 * 1024);
        assert_eq!(m.free.as_u64(), 1_842_488 * 1024);
        assert_eq!(m.available.as_u64(), 52_048_372 * 1024);
        assert_eq!(m.buffers.as_u64(), 1_710_008 * 1024);
        assert_eq!(m.cached.as_u64(), 47_800_420 * 1024);
        assert_eq!(m.swap_total.as_u64(), 8_388_604 * 1024);
        assert_eq!(m.swap_free.as_u64(), 8_387_912 * 1024);
    }

    /// `SwapCached` must not be mistaken for `Cached`, and the unit-less
    /// `HugePages_Total` must not land anywhere at all.
    #[test]
    fn similar_and_unitless_keys_do_not_bleed_into_the_sample() {
        let m = parse_meminfo(p(), REAL_MEMINFO).expect("parses");
        assert_eq!(m.cached.as_u64(), 47_800_420 * 1024);

        assert_eq!(
            parse_meminfo_line(p(), 1, "HugePages_Total:       0").expect("well-formed"),
            None,
            "a unit-less count must be skipped, not read as kB"
        );
        assert!(parse_meminfo_line(p(), 1, "Hugepagesize:       2048 kB")
            .expect("well-formed")
            .is_some());
    }

    /// `used` is `total - available`, not `total - free`. Using MemFree here
    /// would report this idle machine as 97% full instead of 18%.
    #[test]
    fn used_is_derived_from_available_not_free() {
        let m = parse_meminfo(p(), REAL_MEMINFO).expect("parses");

        assert_eq!(m.used().as_u64(), (63_310_204 - 52_048_372) * 1024);

        let pct = m.used_percent().expect("non-zero total").as_f64();
        assert!((17.0..19.0).contains(&pct), "expected ~18%, got {pct}");

        assert_eq!(m.swap_used().as_u64(), (8_388_604 - 8_387_912) * 1024);
    }

    /// Pre-3.14 kernels have no `MemAvailable`. Falling back to `MemFree`
    /// alone would be the same 97%-full lie.
    #[test]
    fn a_kernel_without_memavailable_falls_back_rather_than_failing() {
        let old = "\
MemTotal:       1000000 kB
MemFree:          10000 kB
Buffers:          20000 kB
Cached:          300000 kB
";
        let m = parse_meminfo(p(), old).expect("old kernel parses");
        assert_eq!(m.available.as_u64(), (10_000 + 20_000 + 300_000) * 1024);
    }

    #[test]
    fn a_truncated_line_is_a_parse_error_not_a_panic() {
        // A torn read leaves the last line without its value.
        let torn = "MemTotal:       63310204 kB\nMemFree:";
        let err = parse_meminfo(p(), torn).expect_err("truncated line must fail");
        assert!(matches!(err, Error::Parse { line: Some(2), .. }), "got {err:?}");

        // A line with no colon at all is not a meminfo line.
        let err = parse_meminfo(p(), "MemTotal 63310204 kB\n").expect_err("no colon");
        assert!(matches!(err, Error::Parse { .. }), "got {err:?}");
    }

    #[test]
    fn a_non_numeric_value_is_a_parse_error() {
        let err = parse_meminfo(p(), "MemTotal:  banana kB\n").expect_err("garbage value");
        assert!(matches!(err, Error::Parse { line: Some(1), .. }), "got {err:?}");

        // And the garbage must not carry terminal escapes into the message.
        let rendered = parse_meminfo(p(), "MemTotal:  \u{1b}[2Jx kB\n")
            .expect_err("escape must fail")
            .to_string();
        assert!(!rendered.contains('\u{1b}'), "escape survived: {rendered:?}");
    }

    /// The TODO's third malformed case: a value too large for u64. It must be
    /// a parse error, and a value that fits u64 but overflows when multiplied
    /// by 1024 must be an arithmetic error — not a silent wrap either way.
    #[test]
    fn values_that_overflow_are_errors_not_wraps() {
        let too_big = format!("MemTotal:  {} kB\n", u128::from(u64::MAX) + 1);
        let err = parse_meminfo(p(), &too_big).expect_err("u64 overflow must fail");
        assert!(matches!(err, Error::Parse { .. }), "got {err:?}");

        // Fits in u64, but not once multiplied by 1024 to reach bytes.
        let overflows_kib = format!("MemTotal:  {} kB\n", u64::MAX);
        let err = parse_meminfo(p(), &overflows_kib).expect_err("kiB overflow must fail");
        assert!(matches!(err, Error::Arithmetic { .. }), "got {err:?}");
    }

    #[test]
    fn a_file_without_memtotal_is_an_error() {
        let err = parse_meminfo(p(), "MemFree:  100 kB\n").expect_err("no MemTotal");
        assert!(matches!(err, Error::Parse { line: None, .. }), "got {err:?}");
    }

    #[test]
    fn unknown_keys_and_blank_lines_are_ignored() {
        let with_extras = "\
MemTotal:       1000 kB
\n
SomeFutureKey:  4242 kB
MemFree:         500 kB
";
        let m = parse_meminfo(p(), with_extras).expect("unknown keys are fine");
        assert_eq!(m.total.as_u64(), 1000 * 1024);
        assert_eq!(m.free.as_u64(), 500 * 1024);
    }
}
