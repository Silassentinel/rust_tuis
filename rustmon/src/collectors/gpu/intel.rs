//! Intel GPUs (i915 / xe drivers) via sysfs.
//!
//! Considerably thinner than AMD's interface, and it's worth being honest about
//! why rather than papering over it:
//!
//! - **There is no utilisation percentage in sysfs.** Intel's GPU busy figure
//!   comes from perf/i915 PMU counters, which need `perf_event_open` — a
//!   syscall, elevated permissions on most distributions, and a whole
//!   subsystem. Out of scope for v1. [`crate::sample::Gpu::busy`] stays `None`
//!   for Intel, and the UI shows "n/a" rather than a fabricated number.
//! - **VRAM is usually meaningless** on integrated graphics, which share system
//!   memory. Reported only for discrete Arc cards, where `lmem_total_bytes`
//!   exists.
//! - Frequency (`gt_cur_freq_mhz`) is available and is the most useful signal
//!   this module can offer.
//! - Temperature and power come from a nested hwmon node on discrete cards
//!   only.
//!
//! Showing fewer, correct metrics beats showing invented ones.

use std::path::Path;

use crate::error::Result;
use crate::sample::{Gpu, GpuVendor};
use crate::sysfs::SysfsReader;
use crate::units::{Bytes, KiloHertz, MicroWatts, MilliCelsius};

/// Read every Intel metric available for one card.
///
/// As with AMD, the caller already knows this card is Intel, so this always
/// returns a `Gpu` — one that may be almost entirely `None` on a machine
/// where none of the optional files are exposed, and that's the honest
/// result rather than a reason to report nothing.
pub fn read(reader: &SysfsReader, card: &str) -> Result<Option<Gpu>> {
    // Frequency lives directly under `cardN` (i915/xe put it there); VRAM
    // and the hwmon node live under `cardN/device`, same as every other
    // vendor. Two different base directories for one card, not a typo.
    let card_dir = super::card_dir(card);
    let device_dir = super::device_dir(card);

    let freq_khz = read_frequency(reader, &card_dir)?;
    let (vram_total, vram_used) = read_vram(reader, &device_dir)?;
    let (temp, power) = read_hwmon_metrics(reader, &device_dir)?;

    Ok(Some(Gpu {
        vendor: GpuVendor::Intel,
        name: card.to_string(),
        // No utilisation percentage in sysfs — see the module doc. `None`
        // here, not a fabricated number.
        busy: None,
        vram_total,
        vram_used,
        temp,
        power,
        // Not reported for Intel per the module doc.
        fan_rpm: None,
        freq_khz,
    }))
}

/// `gt_cur_freq_mhz` (i915) or `gt/gt0/freq_cur` (xe). Reported in **MHz**, not
/// kHz — unlike CPU cpufreq. Convert deliberately.
pub fn read_frequency(reader: &SysfsReader, card_dir: &Path) -> Result<Option<KiloHertz>> {
    let mhz = match reader.read_u64(&card_dir.join("gt_cur_freq_mhz")) {
        Ok(Ok(v)) => Some(v),
        _ => match reader.read_u64(&card_dir.join("gt/gt0/freq_cur")) {
            Ok(Ok(v)) => Some(v),
            _ => None,
        },
    };

    Ok(mhz.map(|mhz| KiloHertz::from_khz(mhz.saturating_mul(1000))))
}

/// `lmem_total_bytes` / `lmem_avail_bytes`, discrete cards only.
///
/// `(None, None)` on integrated graphics, which is correct — shared system
/// memory is already reported by the memory collector and repeating it as
/// "VRAM" would be misleading.
///
/// The kernel reports *available* memory, not *used* — the opposite of AMD's
/// `mem_info_vram_used`. `used` here is derived (`total - available`) so the
/// two vendor modules hand [`super::read_card`] the same `(total, used)`
/// shape regardless of which one the kernel actually exposes.
pub fn read_vram(reader: &SysfsReader, device_dir: &Path) -> Result<(Option<Bytes>, Option<Bytes>)> {
    let total = match reader.read_u64(&device_dir.join("lmem_total_bytes")) {
        Ok(Ok(v)) => Some(v),
        _ => None,
    };
    let available = match reader.read_u64(&device_dir.join("lmem_avail_bytes")) {
        Ok(Ok(v)) => Some(v),
        _ => None,
    };

    let used = match (total, available) {
        (Some(t), Some(a)) => Some(Bytes::from_bytes(t.saturating_sub(a))),
        _ => None,
    };

    Ok((total.map(Bytes::from_bytes), used))
}

/// Temperature and power from the nested hwmon node, if the card has one.
///
/// Delegates the hwmon-node lookup to `collectors::gpu::amd::find_hwmon_dir`
/// — both drivers nest it under `device/` identically, so this borrows that
/// implementation rather than duplicating it. See that function's own doc.
pub fn read_hwmon_metrics(
    reader: &SysfsReader,
    device_dir: &Path,
) -> Result<(Option<MilliCelsius>, Option<MicroWatts>)> {
    let Some(hwmon_dir) = super::amd::find_hwmon_dir(reader, device_dir)? else {
        return Ok((None, None));
    };

    let temp = match reader.read_i64(&hwmon_dir.join("temp1_input")) {
        Ok(Ok(v)) => Some(MilliCelsius::from_millidegrees(v)),
        _ => None,
    };

    let power = match reader.read_u64(&hwmon_dir.join("power1_average")) {
        Ok(Ok(v)) => Some(MicroWatts::from_microwatts(v)),
        _ => match reader.read_u64(&hwmon_dir.join("power1_input")) {
            Ok(Ok(v)) => Some(MicroWatts::from_microwatts(v)),
            _ => None,
        },
    };

    Ok((temp, power))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sample::GpuVendor;
    use crate::sysfs::tests::TempTree;

    /// No Intel GPU exists on the machine this crate was developed on, so
    /// every case here is fixture-built against the real i915/xe sysfs
    /// layout rather than captured from `/sys` directly — the layout itself
    /// (`gt_cur_freq_mhz` under `cardN`, `lmem_*_bytes` under
    /// `cardN/device`) is documented kernel ABI, not a guess.
    #[test]
    fn reads_an_i915_style_card() {
        let tree = TempTree::new("gpu-intel-i915");
        tree.file("sys/class/drm/card0/gt_cur_freq_mhz", "1450\n");
        tree.file("sys/class/drm/card0/device/lmem_total_bytes", "8589934592\n");
        tree.file("sys/class/drm/card0/device/lmem_avail_bytes", "6442450944\n");
        let r = tree.reader();

        let gpu = read(&r, "card0").expect("reads fine").expect("Intel always yields a Gpu");

        assert_eq!(gpu.vendor, GpuVendor::Intel);
        assert_eq!(gpu.busy, None, "no utilisation percentage in sysfs for Intel");
        assert_eq!(gpu.fan_rpm, None);

        // 1450 MHz -> 1,450,000 kHz.
        assert_eq!(gpu.freq_khz.expect("frequency present").as_khz(), 1_450_000);

        // total 8 GiB, available 6 GiB -> used = 2 GiB, derived, not read.
        assert_eq!(gpu.vram_total.expect("total present").as_u64(), 8_589_934_592);
        assert_eq!(gpu.vram_used.expect("used present").as_u64(), 2_147_483_648);
    }

    /// The xe driver's frequency path must be tried when i915's isn't there.
    #[test]
    fn frequency_falls_back_to_the_xe_path() {
        let tree = TempTree::new("gpu-intel-xe-freq");
        tree.file("sys/class/drm/card0/gt/gt0/freq_cur", "800\n");
        let r = tree.reader();

        let freq = read_frequency(&r, Path::new("sys/class/drm/card0"))
            .expect("reads fine")
            .expect("xe fallback found it");
        assert_eq!(freq.as_khz(), 800_000);
    }

    /// Integrated graphics: no `lmem_*` files at all, since VRAM is shared
    /// system memory. `(None, None)` is the correct answer, not an error —
    /// reporting it would double-count what the memory collector already
    /// shows.
    #[test]
    fn integrated_graphics_reports_no_vram() {
        let tree = TempTree::new("gpu-intel-integrated");
        tree.dir("sys/class/drm/card0/device");
        let r = tree.reader();

        let (total, used) =
            read_vram(&r, Path::new("sys/class/drm/card0/device")).expect("reads fine");
        assert_eq!(total, None);
        assert_eq!(used, None);
    }

    /// Only one of the two files present: deriving `used` needs both, so it
    /// must stay `None` rather than treating a missing `available` as zero
    /// (which would report the card as 100% out of VRAM).
    #[test]
    fn vram_used_is_none_without_both_files() {
        let tree = TempTree::new("gpu-intel-partial-vram");
        tree.file("sys/class/drm/card0/device/lmem_total_bytes", "1000\n");
        let r = tree.reader();

        let (total, used) =
            read_vram(&r, Path::new("sys/class/drm/card0/device")).expect("reads fine");
        assert_eq!(total.expect("total present").as_u64(), 1000);
        assert_eq!(used, None, "available is missing, so used can't be derived");
    }

    #[test]
    fn a_card_with_nothing_readable_still_yields_a_gpu() {
        let tree = TempTree::new("gpu-intel-empty");
        tree.dir("sys/class/drm/card0/device");
        let r = tree.reader();

        let gpu = read(&r, "card0").expect("reads fine").expect("identity alone is enough");
        assert_eq!(gpu.name, "card0");
        assert_eq!(gpu.freq_khz, None);
    }
}
