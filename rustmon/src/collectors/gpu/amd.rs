//! AMD GPUs via the amdgpu driver's sysfs interface.
//!
//! The best-supported vendor here — everything is a plain file:
//!
//! | Metric | File (under `sys/class/drm/cardN/device/`) | Unit |
//! |---|---|---|
//! | Utilisation | `gpu_busy_percent` | percent |
//! | VRAM total | `mem_info_vram_total` | bytes |
//! | VRAM used | `mem_info_vram_used` | bytes |
//! | Temperature | `hwmon/hwmonN/temp1_input` | millidegrees |
//! | Power | `hwmon/hwmonN/power1_average` | microwatts |
//! | Fan | `hwmon/hwmonN/fan1_input` | RPM |
//!
//! Note VRAM here is in **bytes**, unlike almost everything else in `/sys` —
//! don't route it through the kB or sector converters.
//!
//! The hwmon subdirectory under the device is a nested `hwmonN` whose number is
//! not predictable, so it has to be listed rather than assumed. Older amdgpu
//! versions expose `power1_average`; newer ones prefer `power1_input`. Both are
//! tried, in that order.

use std::path::{Path, PathBuf};

use crate::error::Result;
use crate::sample::{Gpu, GpuVendor};
use crate::sysfs::SysfsReader;
use crate::units::{Bytes, MicroWatts, MilliCelsius, Percent, Rpm};

/// Read every AMD metric available for one card.
///
/// Each field is independently optional: a card with no fan (passively cooled)
/// or no power reporting still produces a useful [`Gpu`]. Unlike the thermal
/// collector's chips, there is no "produced nothing, so treat it as absent"
/// case here — the caller already knows this is an AMD card (that's how it
/// got dispatched here), so it always gets a `Gpu`, even one that's mostly
/// `None`.
pub fn read(reader: &SysfsReader, card: &str) -> Result<Option<Gpu>> {
    let device_dir = super::device_dir(card);

    let busy = read_busy(reader, &device_dir)?;
    let (vram_total, vram_used) = read_vram(reader, &device_dir)?;

    let (temp, power, fan_rpm) = match find_hwmon_dir(reader, &device_dir)? {
        Some(hwmon_dir) => read_hwmon_metrics(reader, &hwmon_dir)?,
        None => (None, None, None),
    };

    Ok(Some(Gpu {
        vendor: GpuVendor::Amd,
        name: card.to_string(),
        busy,
        vram_total,
        vram_used,
        temp,
        power,
        fan_rpm,
        // AMD exposes clock via `pp_dpm_sclk`, a multi-line "level: value"
        // format quite different from the plain-scalar files everything
        // else here reads — out of scope for this chunk. See the field's
        // own doc in `sample.rs`.
        freq_khz: None,
    }))
}

/// Locate the nested `hwmon/hwmonN` directory under a card's `device` dir.
///
/// `None` when the driver exposes no hwmon node, which happens on some
/// virtualised/passthrough setups. Reused by `collectors::gpu::intel` — both
/// drivers nest their hwmon node under `device/` the same way, so this one
/// implementation covers both rather than being duplicated.
pub fn find_hwmon_dir(reader: &SysfsReader, device_dir: &Path) -> Result<Option<PathBuf>> {
    let is_hwmon_subdir = |name: &str| {
        name.strip_prefix("hwmon")
            .is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()))
    };

    match reader.list_dir(&device_dir.join("hwmon"), &is_hwmon_subdir)? {
        // `list_dir` already sorts for determinism, so `next()` is the
        // lowest-numbered hwmon node without an extra sort here.
        Ok(names) => Ok(names.into_iter().next().map(|n| device_dir.join("hwmon").join(n))),
        Err(_) => Ok(None),
    }
}

/// `gpu_busy_percent`, 0-100.
pub fn read_busy(reader: &SysfsReader, device_dir: &Path) -> Result<Option<Percent>> {
    Ok(match reader.read_u64(&device_dir.join("gpu_busy_percent")) {
        Ok(Ok(v)) => Some(Percent::new(v as f64)),
        _ => None,
    })
}

/// `mem_info_vram_total` and `mem_info_vram_used`, both already in bytes.
pub fn read_vram(reader: &SysfsReader, device_dir: &Path) -> Result<(Option<Bytes>, Option<Bytes>)> {
    let total = match reader.read_u64(&device_dir.join("mem_info_vram_total")) {
        Ok(Ok(v)) => Some(Bytes::from_bytes(v)),
        _ => None,
    };
    let used = match reader.read_u64(&device_dir.join("mem_info_vram_used")) {
        Ok(Ok(v)) => Some(Bytes::from_bytes(v)),
        _ => None,
    };
    Ok((total, used))
}

/// Temperature, power draw and fan speed from the nested hwmon node.
pub fn read_hwmon_metrics(
    reader: &SysfsReader,
    hwmon_dir: &Path,
) -> Result<(Option<MilliCelsius>, Option<MicroWatts>, Option<Rpm>)> {
    let temp = match reader.read_i64(&hwmon_dir.join("temp1_input")) {
        Ok(Ok(v)) => Some(MilliCelsius::from_millidegrees(v)),
        _ => None,
    };

    // Older amdgpu versions expose `power1_average`; newer ones prefer
    // `power1_input`. Both are tried, in that order, per the module doc.
    let power = match reader.read_u64(&hwmon_dir.join("power1_average")) {
        Ok(Ok(v)) => Some(MicroWatts::from_microwatts(v)),
        _ => match reader.read_u64(&hwmon_dir.join("power1_input")) {
            Ok(Ok(v)) => Some(MicroWatts::from_microwatts(v)),
            _ => None,
        },
    };

    let fan_rpm = match reader.read_u64(&hwmon_dir.join("fan1_input")) {
        Ok(Ok(v)) => Some(Rpm::from_rpm(v)),
        _ => None,
    };

    Ok((temp, power, fan_rpm))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sysfs::tests::TempTree;

    /// Captured from this machine's `card1` (a real AMD GPU) on 2026-08-11.
    /// Note `power1_input` is genuinely absent here — only `power1_average`
    /// exists — which is exactly the fallback order this module has to get
    /// right.
    fn real_amd_tree() -> TempTree {
        let tree = TempTree::new("gpu-amd-real");
        tree.file("sys/class/drm/card1/device/gpu_busy_percent", "2\n");
        tree.file("sys/class/drm/card1/device/mem_info_vram_total", "17095983104\n");
        tree.file("sys/class/drm/card1/device/mem_info_vram_used", "1811267584\n");
        tree.file("sys/class/drm/card1/device/hwmon/hwmon5/temp1_input", "26000\n");
        tree.file("sys/class/drm/card1/device/hwmon/hwmon5/power1_average", "9000000\n");
        tree.file("sys/class/drm/card1/device/hwmon/hwmon5/fan1_input", "0\n");
        tree
    }

    #[test]
    fn reads_a_real_amd_card_end_to_end() {
        let tree = real_amd_tree();
        let r = tree.reader();

        let gpu = read(&r, "card1").expect("reads fine").expect("AMD always yields a Gpu");

        assert_eq!(gpu.vendor, GpuVendor::Amd);
        assert_eq!(gpu.name, "card1");
        assert!((gpu.busy.expect("busy present").as_f64() - 2.0).abs() < 1e-9);
        assert_eq!(gpu.vram_total.expect("total present").as_u64(), 17_095_983_104);
        assert_eq!(gpu.vram_used.expect("used present").as_u64(), 1_811_267_584);
        assert_eq!(gpu.temp.expect("temp present").as_millidegrees(), 26_000);
        assert_eq!(gpu.power.expect("power present").as_watts(), 9.0);
        assert_eq!(gpu.fan_rpm.expect("fan present").as_u64(), 0);
        assert_eq!(gpu.freq_khz, None, "AMD clock isn't wired up yet — see the field's doc");
    }

    /// The specific fallback order the module doc calls out: newer amdgpu
    /// prefers `power1_input`, and it must be tried when `power1_average` is
    /// absent.
    #[test]
    fn power_falls_back_to_power1_input_when_average_is_absent() {
        let tree = TempTree::new("gpu-amd-power-fallback");
        tree.file("sys/class/drm/card0/device/hwmon/hwmon0/power1_input", "12000000\n");
        let r = tree.reader();

        let (_, power, _) = read_hwmon_metrics(&r, Path::new("sys/class/drm/card0/device/hwmon/hwmon0"))
            .expect("reads fine");
        assert_eq!(power.expect("fallback found it").as_watts(), 12.0);
    }

    #[test]
    fn a_card_with_no_hwmon_node_still_yields_a_gpu() {
        let tree = TempTree::new("gpu-amd-no-hwmon");
        tree.file("sys/class/drm/card0/device/gpu_busy_percent", "0\n");
        let r = tree.reader();

        let gpu = read(&r, "card0").expect("reads fine").expect("still a Gpu");
        assert_eq!(gpu.temp, None);
        assert_eq!(gpu.power, None);
        assert_eq!(gpu.fan_rpm, None);
    }

    #[test]
    fn a_completely_empty_device_dir_still_yields_a_gpu() {
        let tree = TempTree::new("gpu-amd-empty");
        tree.dir("sys/class/drm/card0/device");
        let r = tree.reader();

        let gpu = read(&r, "card0").expect("reads fine").expect("identity alone is enough");
        assert_eq!(gpu.name, "card0");
        assert_eq!(gpu.busy, None);
    }

    #[test]
    fn find_hwmon_dir_picks_the_lowest_numbered_node() {
        let tree = TempTree::new("gpu-amd-hwmon-pick");
        tree.dir("sys/class/drm/card0/device/hwmon/hwmon9");
        tree.dir("sys/class/drm/card0/device/hwmon/hwmon2");
        let r = tree.reader();

        let found = find_hwmon_dir(&r, Path::new("sys/class/drm/card0/device"))
            .expect("reads fine")
            .expect("a hwmon node exists");
        assert!(found.ends_with("hwmon2"), "{found:?}");
    }

    #[test]
    fn find_hwmon_dir_is_none_when_absent() {
        let tree = TempTree::new("gpu-amd-hwmon-absent");
        tree.dir("sys/class/drm/card0/device");
        let r = tree.reader();

        assert!(find_hwmon_dir(&r, Path::new("sys/class/drm/card0/device"))
            .expect("not an error")
            .is_none());
    }
}
