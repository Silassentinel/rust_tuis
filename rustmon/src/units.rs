//! Typed units.
//!
//! Rust Book Ch. 19 (newtype pattern). This module exists because of one
//! specific, near-certain bug: `/proc/meminfo` reports **kB**, `/proc/diskstats`
//! reports **512-byte sectors** (always 512, regardless of the device's real
//! sector size), `scaling_cur_freq` reports **kHz**, and hwmon reports
//! **millidegrees**. Passing raw `u64`s around guarantees someone eventually
//! renders memory 1024x too small or disk throughput 8x too large.
//!
//! Every constructor is named for the unit it takes. There is deliberately no
//! `From<u64>` for any of these.

use std::fmt;

use crate::error::Error;

/// Binary prefixes, smallest first. `Bytes` is a `u64`, so `EiB` is the
/// largest that can ever be reached (`u64::MAX` is just under 16 EiB) — the
/// list stops there rather than carrying unreachable entries.
const BINARY_PREFIXES: [&str; 7] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB", "EiB"];

/// Step `value` down by 1024s until it fits a prefix, returning the scaled
/// value and its index into [`BINARY_PREFIXES`].
///
/// Rust Book Ch. 13: this is shared by [`Bytes::human`] and
/// [`BytesPerSec::human`] so the two can never drift apart on where a
/// boundary sits.
fn scale_binary(value: f64) -> (f64, usize) {
    let mut value = value;
    let mut idx = 0;

    while value >= 1024.0 && idx + 1 < BINARY_PREFIXES.len() {
        value /= 1024.0;
        idx += 1;
    }

    // Rounding to one decimal can push a value back up onto the next
    // threshold: 1 MiB - 1 byte is 1023.9990 KiB, which would otherwise print
    // as "1024.0 KiB". Promoting once more prints "1.0 MiB", which is what a
    // reader expects and is not off by enough to mislead.
    if idx + 1 < BINARY_PREFIXES.len() && (value * 10.0).round() >= 10_240.0 {
        value /= 1024.0;
        idx += 1;
    }

    (value, idx)
}

/// A byte count.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Bytes(u64);

impl Bytes {
    pub const fn from_bytes(n: u64) -> Self {
        Bytes(n)
    }

    /// `/proc/meminfo` values, which are in kB (kibibytes, despite the label).
    pub fn from_kib(n: u64) -> Result<Self, crate::error::Error> {
        n.checked_mul(1024).map(Bytes).ok_or(Error::Arithmetic {
            what: "kiB value overflows u64 when converted to bytes",
        })
    }

    /// `/proc/diskstats` values, which are in fixed 512-byte sectors.
    pub fn from_sectors_512(n: u64) -> Result<Self, crate::error::Error> {
        n.checked_mul(512).map(Bytes).ok_or(Error::Arithmetic {
            what: "sector count overflows u64 when converted to bytes",
        })
    }

    pub const fn as_u64(self) -> u64 {
        self.0
    }

    /// Difference, or `None` if `earlier` is larger (counter reset / wrap).
    /// Callers must treat `None` as "drop this interval", never as zero.
    pub fn delta_since(self, earlier: Bytes) -> Option<Bytes> {
        self.0.checked_sub(earlier.0).map(Bytes)
    }

    /// Binary-prefix rendering: `1.5 GiB`. Boundary cases (1023 B, 1024 B,
    /// 1 MiB - 1) are what the unit tests target.
    pub fn human(self) -> String {
        let (value, idx) = scale_binary(self.0 as f64);

        if idx == 0 {
            // Print the exact integer rather than round-tripping through f64:
            // a byte count below 1024 has no fractional part to show, and
            // "1023 B" reads better than "1023.0 B".
            format!("{} B", self.0)
        } else {
            format!("{value:.1} {}", BINARY_PREFIXES[idx])
        }
    }
}

impl fmt::Display for Bytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.human())
    }
}

/// A throughput, derived — never read directly from the kernel.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Default)]
pub struct BytesPerSec(f64);

impl BytesPerSec {
    pub const fn new(v: f64) -> Self {
        BytesPerSec(v)
    }

    pub const fn as_f64(self) -> f64 {
        self.0
    }

    /// e.g. `"12.4 MiB/s"`.
    ///
    /// A non-finite rate renders as `"n/a"` rather than `"NaN/s"` or
    /// `"inf/s"`. [`crate::delta`] is supposed to drop those intervals before
    /// they get here, but a renderer is the wrong place to discover that it
    /// didn't — and `"n/a"` is honest either way.
    pub fn human(self) -> String {
        if !self.0.is_finite() || self.0 < 0.0 {
            return "n/a".to_string();
        }

        let (value, idx) = scale_binary(self.0);

        if idx == 0 {
            format!("{value:.0} B/s")
        } else {
            format!("{value:.1} {}/s", BINARY_PREFIXES[idx])
        }
    }
}

/// A clock frequency in kHz, as `scaling_cur_freq` reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct KiloHertz(u64);

impl KiloHertz {
    pub const fn from_khz(n: u64) -> Self {
        KiloHertz(n)
    }

    pub const fn as_khz(self) -> u64 {
        self.0
    }

    pub fn as_mhz(self) -> f64 {
        self.0 as f64 / 1_000.0
    }

    pub fn as_ghz(self) -> f64 {
        self.0 as f64 / 1_000_000.0
    }
}

/// A temperature in millidegrees Celsius, as hwmon reports it.
/// Signed: some sensors legitimately report below zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct MilliCelsius(i64);

impl MilliCelsius {
    pub const fn from_millidegrees(n: i64) -> Self {
        MilliCelsius(n)
    }

    pub const fn as_millidegrees(self) -> i64 {
        self.0
    }

    pub fn as_celsius(self) -> f64 {
        self.0 as f64 / 1_000.0
    }

    /// True if at or above `limit`. Used for the crit/max trip-point display.
    ///
    /// Compares in millidegrees, not through `as_celsius`: the integer
    /// comparison is exact, and a float one would make "exactly at the trip
    /// point" depend on rounding.
    pub fn at_or_above(self, limit: MilliCelsius) -> bool {
        self.0 >= limit.0
    }
}

/// A percentage, clamped to 0.0..=100.0 on construction.
///
/// Clamping is deliberate: a derived CPU-busy figure can come out fractionally
/// above 100 from jiffy-counter rounding, and rendering "100.3%" looks like a
/// bug even though it isn't.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Default)]
pub struct Percent(f64);

impl Percent {
    pub fn new(v: f64) -> Self {
        // NaN is checked before `clamp` on purpose: `f64::clamp` propagates a
        // NaN input rather than clamping it, so relying on clamp alone would
        // let a NaN straight through into the renderer.
        if v.is_nan() {
            return Percent(0.0);
        }
        Percent(v.clamp(0.0, 100.0))
    }

    /// From a part/whole pair. `None` when `whole` is zero — a zero-length
    /// sampling interval must not become a division by zero.
    pub fn from_ratio(part: u64, whole: u64) -> Option<Self> {
        if whole == 0 {
            return None;
        }
        Some(Percent::new(part as f64 * 100.0 / whole as f64))
    }

    pub const fn as_f64(self) -> f64 {
        self.0
    }
}

/// Fan speed in revolutions per minute.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Rpm(u64);

impl Rpm {
    pub const fn from_rpm(n: u64) -> Self {
        Rpm(n)
    }

    pub const fn as_u64(self) -> u64 {
        self.0
    }
}

/// Power draw in microwatts, as hwmon `power*_input` reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct MicroWatts(u64);

impl MicroWatts {
    pub const fn from_microwatts(n: u64) -> Self {
        MicroWatts(n)
    }

    pub fn as_watts(self) -> f64 {
        self.0 as f64 / 1_000_000.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Float comparison helper — 45.678 C has no exact `f64` representation,
    /// so `assert_eq!` on the nose would be testing the FPU, not the code.
    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn constructors_round_trip() {
        assert_eq!(Bytes::from_bytes(4096).as_u64(), 4096);
        assert_eq!(KiloHertz::from_khz(3_600_000).as_khz(), 3_600_000);
        assert_eq!(MilliCelsius::from_millidegrees(-4_250).as_millidegrees(), -4_250);
        assert_eq!(Rpm::from_rpm(1_820).as_u64(), 1_820);
        assert!(close(BytesPerSec::new(12.5).as_f64(), 12.5));
    }

    /// The bug this whole module exists to prevent: `/proc/meminfo` is kB and
    /// `/proc/diskstats` is 512-byte sectors, and mixing them up is wrong by
    /// exactly 2x.
    #[test]
    fn kib_and_sector_conversions_use_the_right_multiplier() {
        assert_eq!(
            Bytes::from_kib(4).expect("no overflow").as_u64(),
            4_096
        );
        assert_eq!(
            Bytes::from_sectors_512(4).expect("no overflow").as_u64(),
            2_048
        );
        // 16 GiB of RAM as /proc/meminfo would report it.
        assert_eq!(
            Bytes::from_kib(16_777_216).expect("no overflow").as_u64(),
            17_179_869_184
        );
    }

    #[test]
    fn conversions_that_would_overflow_are_errors_not_wraps() {
        assert!(matches!(
            Bytes::from_kib(u64::MAX),
            Err(Error::Arithmetic { .. })
        ));
        assert!(matches!(
            Bytes::from_sectors_512(u64::MAX),
            Err(Error::Arithmetic { .. })
        ));
        // The largest value that still fits must not be rejected.
        assert!(Bytes::from_kib(u64::MAX / 1024).is_ok());
        assert!(Bytes::from_sectors_512(u64::MAX / 512).is_ok());
    }

    /// A counter that went backwards (device reset, 32-bit wrap) must yield
    /// `None` so the caller drops the interval. Returning zero would render a
    /// silent, plausible-looking lie.
    #[test]
    fn delta_since_refuses_to_go_backwards() {
        assert_eq!(
            Bytes::from_bytes(100).delta_since(Bytes::from_bytes(40)),
            Some(Bytes::from_bytes(60))
        );
        assert_eq!(
            Bytes::from_bytes(40).delta_since(Bytes::from_bytes(40)),
            Some(Bytes::from_bytes(0))
        );
        assert_eq!(Bytes::from_bytes(40).delta_since(Bytes::from_bytes(100)), None);
    }

    #[test]
    fn human_bytes_boundaries() {
        assert_eq!(Bytes::from_bytes(0).human(), "0 B");
        assert_eq!(Bytes::from_bytes(1_023).human(), "1023 B");
        assert_eq!(Bytes::from_bytes(1_024).human(), "1.0 KiB");
        assert_eq!(Bytes::from_bytes(1_536).human(), "1.5 KiB");

        // 1 MiB - 1 is 1023.999 KiB. It must promote rather than print
        // "1024.0 KiB", which reads as a bug.
        assert_eq!(Bytes::from_bytes(1_048_575).human(), "1.0 MiB");
        assert_eq!(Bytes::from_bytes(1_048_576).human(), "1.0 MiB");

        assert_eq!(Bytes::from_bytes(1_610_612_736).human(), "1.5 GiB");
        // Nothing can exceed the largest prefix, so this must not panic or
        // index past the end of BINARY_PREFIXES.
        assert_eq!(Bytes::from_bytes(u64::MAX).human(), "16.0 EiB");
    }

    #[test]
    fn display_matches_human() {
        let b = Bytes::from_bytes(1_610_612_736);
        assert_eq!(b.to_string(), b.human());
    }

    #[test]
    fn human_rate_carries_the_per_second_suffix() {
        assert_eq!(BytesPerSec::new(0.0).human(), "0 B/s");
        assert_eq!(BytesPerSec::new(512.4).human(), "512 B/s");
        assert_eq!(BytesPerSec::new(13_002_342.4).human(), "12.4 MiB/s");
    }

    /// `delta` should never hand us these, but a renderer that prints
    /// "NaN/s" or "-inf/s" is worse than one that admits it doesn't know.
    #[test]
    fn non_finite_and_negative_rates_render_as_na() {
        assert_eq!(BytesPerSec::new(f64::NAN).human(), "n/a");
        assert_eq!(BytesPerSec::new(f64::INFINITY).human(), "n/a");
        assert_eq!(BytesPerSec::new(f64::NEG_INFINITY).human(), "n/a");
        assert_eq!(BytesPerSec::new(-1.0).human(), "n/a");
    }

    #[test]
    fn frequency_conversions() {
        let f = KiloHertz::from_khz(3_600_000);
        assert!(close(f.as_mhz(), 3_600.0));
        assert!(close(f.as_ghz(), 3.6));
        assert!(close(KiloHertz::default().as_ghz(), 0.0));
    }

    #[test]
    fn millicelsius_to_celsius_keeps_the_fraction() {
        assert!(close(MilliCelsius::from_millidegrees(45_678).as_celsius(), 45.678));
        assert!(close(MilliCelsius::from_millidegrees(0).as_celsius(), 0.0));
        // Sensors below freezing are legitimate, hence the signed newtype.
        assert!(close(MilliCelsius::from_millidegrees(-500).as_celsius(), -0.5));
    }

    #[test]
    fn trip_point_comparison_is_inclusive_and_exact() {
        let crit = MilliCelsius::from_millidegrees(95_000);
        assert!(MilliCelsius::from_millidegrees(95_000).at_or_above(crit));
        assert!(MilliCelsius::from_millidegrees(95_001).at_or_above(crit));
        assert!(!MilliCelsius::from_millidegrees(94_999).at_or_above(crit));
    }

    #[test]
    fn percent_clamps_both_ends() {
        assert!(close(Percent::new(50.0).as_f64(), 50.0));
        assert!(close(Percent::new(0.0).as_f64(), 0.0));
        assert!(close(Percent::new(100.0).as_f64(), 100.0));

        // Jiffy-counter rounding genuinely produces figures a hair over 100.
        assert!(close(Percent::new(100.3).as_f64(), 100.0));
        assert!(close(Percent::new(-0.5).as_f64(), 0.0));
        assert!(close(Percent::new(f64::INFINITY).as_f64(), 100.0));
        assert!(close(Percent::new(f64::NEG_INFINITY).as_f64(), 0.0));
    }

    /// `f64::clamp` propagates NaN instead of clamping it, so this needs its
    /// own guard and its own test.
    #[test]
    fn percent_maps_nan_to_zero() {
        let p = Percent::new(f64::NAN);
        assert!(!p.as_f64().is_nan());
        assert!(close(p.as_f64(), 0.0));
    }

    #[test]
    fn percent_from_ratio_refuses_a_zero_whole() {
        assert!(close(
            Percent::from_ratio(1, 4).expect("non-zero whole").as_f64(),
            25.0
        ));
        assert!(close(
            Percent::from_ratio(0, 100).expect("non-zero whole").as_f64(),
            0.0
        ));
        // A zero-length sampling interval must not become a division by zero.
        assert_eq!(Percent::from_ratio(5, 0), None);
        assert_eq!(Percent::from_ratio(0, 0), None);
    }

    #[test]
    fn microwatts_to_watts() {
        assert!(close(
            MicroWatts::from_microwatts(65_000_000).as_watts(),
            65.0
        ));
        assert!(close(MicroWatts::from_microwatts(0).as_watts(), 0.0));
    }
}
