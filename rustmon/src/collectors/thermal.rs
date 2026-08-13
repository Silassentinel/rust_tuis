//! Temperatures and fans, from `sys/class/hwmon` with a `sys/class/thermal`
//! fallback.
//!
//! hwmon is the messiest interface rustmon touches, and the shape of this module
//! reflects that:
//!
//! - A chip may expose temps, fans, both, or neither.
//! - `tempN_label` is often absent; the fallback name is the chip name plus the
//!   index, never a guess at what the sensor measures.
//! - Sensor indices are **not contiguous** — `temp1`, `temp3`, `temp5` is
//!   normal. Enumerate by listing the directory, never by counting up from 1.
//! - Some sensors need root; those come back as absent with a reason, and that
//!   is the correct behaviour, not a bug to work around by escalating.
//! - Machines with no `hwmon` at all (many VMs, most containers) must yield
//!   `None`, not an error.
//!
//! `sys/class/thermal/thermal_zoneN` is a poorer interface (no fans, coarse
//! labels) used only where no hwmon chip exists.
//!
//! Security note: `name` and `tempN_label` are kernel-supplied strings that can
//! originate from device firmware. Everything read here goes through
//! [`crate::sysfs::sanitize_kernel_string`] before it reaches a `Snapshot`, not
//! at render time — sanitising at the boundary means no renderer can forget to.
//!
//! # Fail-soft granularity is per-sensor, not per-chip or per-collector
//!
//! Chunk 3's collectors treat their one source file as fatal (a garbled
//! `/proc/stat` fails the whole CPU collector) because that file *is* the
//! point of the collector. Thermal has no equivalent single point of truth —
//! a machine here has seven independent hwmon chips, each with its own
//! sensors read from its own files. A garbled `temp3_input` on one chip must
//! not cost the caller the other twenty sensors on six other chips, so every
//! per-sensor read in this module degrades to "this one sensor is absent"
//! (`Ok(None)`) rather than propagating `Err` — the same fail-soft posture
//! the module doc already applies to `EACCES`, extended to cover garbage
//! values too. `read_hwmon_chips`/`ThermalCollector::collect` can still
//! return `Err` for a genuine directory-listing failure, which is a
//! different, much rarer thing than one sensor's value not parsing.

use std::path::{Path, PathBuf};

use crate::collector::Collector;
use crate::error::Result;
use crate::sample::{FanSensor, HwmonChip, Snapshot, TempSensor, ThermalSample};
use crate::sysfs::{sanitize_kernel_string, SysfsReader};
use crate::units::{MilliCelsius, Rpm};

pub const NAME: &str = "thermal";

const SYS_HWMON: &str = "sys/class/hwmon";
const SYS_THERMAL: &str = "sys/class/thermal";

/// Cap on sensors per chip, so a malformed fixture tree can't make us
/// enumerate unboundedly.
const MAX_SENSORS_PER_CHIP: usize = 256;

/// Cap on a chip name or sensor label before it reaches a terminal.
const MAX_LABEL_LEN: usize = 64;

#[derive(Debug, Default)]
pub struct ThermalCollector {
    /// Chip directory names discovered at probe time. Re-enumerated only when
    /// a read fails, since hwmon chips can appear on module load.
    chip_dirs: Vec<String>,
}

impl ThermalCollector {
    pub fn new() -> Self {
        Self::default()
    }

    /// Read every currently-cached chip directory, refreshing the cache once
    /// if any of them has gone missing since it was cached.
    ///
    /// **What this promises and what it doesn't:** a chip that *disappears*
    /// (module unloaded) is reliably detected and drops out within one
    /// refresh, because that's checked directly against every cached name. A
    /// chip that *appears* is only picked up as a side effect of some other
    /// chip disappearing at the same time (which triggers the same
    /// refresh) — there's no proactive "did anything new show up" check,
    /// because that would mean re-listing the directory every refresh, which
    /// is the exact cost the cache exists to avoid. This is a narrower
    /// guarantee than "hwmon chips can appear on module load" alone might
    /// suggest; it's deliberate, and `a_new_chip_is_not_picked_up_by_itself`
    /// pins the boundary down.
    fn read_cached_chips(&mut self, reader: &SysfsReader) -> Result<Vec<HwmonChip>> {
        if self.chip_dirs.is_empty() {
            self.chip_dirs = list_hwmon_chip_dirs(reader)?;
        }

        let stale = self
            .chip_dirs
            .iter()
            .any(|name| !reader.exists(&hwmon_chip_dir(name)));

        if stale {
            self.chip_dirs = list_hwmon_chip_dirs(reader)?;
        }

        let mut chips = Vec::with_capacity(self.chip_dirs.len());
        for name in &self.chip_dirs {
            if let Some(chip) = read_chip(reader, &hwmon_chip_dir(name))? {
                chips.push(chip);
            }
        }
        Ok(chips)
    }
}

impl Collector for ThermalCollector {
    fn name(&self) -> &'static str {
        NAME
    }

    fn probe(&self, reader: &SysfsReader) -> bool {
        let has_hwmon_chip = matches!(
            reader.list_dir(Path::new(SYS_HWMON), &is_hwmon_chip_dir),
            Ok(Ok(names)) if !names.is_empty()
        );
        has_hwmon_chip || reader.exists(Path::new(SYS_THERMAL))
    }

    fn collect(&mut self, reader: &SysfsReader, snapshot: &mut Snapshot) -> Result<()> {
        let mut chips = self.read_cached_chips(reader)?;

        if chips.is_empty() {
            chips = read_thermal_zones(reader)?;
        }

        snapshot.thermal = build_sample(chips);
        Ok(())
    }
}

fn hwmon_chip_dir(name: &str) -> PathBuf {
    PathBuf::from(format!("{SYS_HWMON}/{name}"))
}

fn is_hwmon_chip_dir(name: &str) -> bool {
    name.strip_prefix("hwmon")
        .is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()))
}

fn is_thermal_zone_dir(name: &str) -> bool {
    name.strip_prefix("thermal_zone")
        .is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()))
}

fn list_hwmon_chip_dirs(reader: &SysfsReader) -> Result<Vec<String>> {
    match reader.list_dir(Path::new(SYS_HWMON), &is_hwmon_chip_dir)? {
        Ok(names) => Ok(names),
        // No `/sys/class/hwmon` at all — not fatal, `read_thermal_zones` is
        // the fallback for exactly this.
        Err(_) => Ok(Vec::new()),
    }
}

/// Enumerate and read every `sys/class/hwmon/hwmonN`.
pub fn read_hwmon_chips(reader: &SysfsReader) -> Result<Vec<HwmonChip>> {
    let mut chips = Vec::new();
    for name in list_hwmon_chip_dirs(reader)? {
        if let Some(chip) = read_chip(reader, &hwmon_chip_dir(&name))? {
            chips.push(chip);
        }
    }
    Ok(chips)
}

/// Read one chip directory into a [`HwmonChip`].
pub fn read_chip(reader: &SysfsReader, chip_dir: &Path) -> Result<Option<HwmonChip>> {
    // Every real hwmon chip has a `name` file — it's mandatory in the kernel's
    // own hwmon API. If it's missing, either the chip vanished between being
    // listed and being read (a race, not an error) or this directory was
    // never a real chip; either way there's nothing sensible to show.
    let name = match reader.read_first_line(&chip_dir.join("name")) {
        Ok(Ok(raw)) => sanitize_kernel_string(&raw, MAX_LABEL_LEN),
        Ok(Err(_)) | Err(_) => return Ok(None),
    };

    let mut temps = Vec::with_capacity(4);
    for index in sensor_indices(reader, chip_dir, "temp")? {
        if let Some(sensor) = read_temp_sensor(reader, chip_dir, &name, index)? {
            temps.push(sensor);
        }
    }

    let mut fans = Vec::new();
    for index in sensor_indices(reader, chip_dir, "fan")? {
        if let Some(sensor) = read_fan_sensor(reader, chip_dir, &name, index)? {
            fans.push(sensor);
        }
    }

    if temps.is_empty() && fans.is_empty() {
        // A chip that reports neither is indistinguishable, to a caller,
        // from a chip that isn't there — treat it the same way.
        return Ok(None);
    }

    Ok(Some(HwmonChip { name, temps, fans }))
}

/// Sensor indices actually present, found by listing rather than counting.
///
/// `prefix` is `"temp"` or `"fan"`; matches `<prefix><N>_input`. Indices are
/// **not contiguous** on real hardware — this machine's `k10temp` chip
/// exposes `temp1`, `temp3`, `temp4` with no `temp2` at all — so counting up
/// from 1 and stopping at the first gap would silently drop every sensor
/// after it.
pub fn sensor_indices(reader: &SysfsReader, chip_dir: &Path, prefix: &str) -> Result<Vec<u32>> {
    let suffix = "_input";

    let names = match reader.list_dir(chip_dir, &|name| {
        name.starts_with(prefix) && name.ends_with(suffix)
    })? {
        Ok(names) => names,
        // The chip directory itself vanished between being listed and being
        // read — a race, not an error.
        Err(_) => return Ok(Vec::new()),
    };

    let mut indices: Vec<u32> = names
        .iter()
        // `strip_prefix`/`strip_suffix` rather than manual slicing: it can
        // never panic regardless of how `prefix` and `suffix` might overlap
        // in a pathologically short name.
        .filter_map(|name| name.strip_prefix(prefix)?.strip_suffix(suffix))
        .filter_map(|middle| middle.parse::<u32>().ok())
        .collect();

    indices.sort_unstable();
    indices.truncate(MAX_SENSORS_PER_CHIP);
    Ok(indices)
}

/// Read `tempN_input` plus its optional `_label`, `_max`, `_crit`.
///
/// `Ok(None)` covers two different situations identically on purpose: the
/// file is genuinely absent (raced away since listing), and the file exists
/// but its content is garbage. Both mean "nothing sensible to show for this
/// one sensor" — see the module doc on fail-soft granularity.
pub fn read_temp_sensor(
    reader: &SysfsReader,
    chip_dir: &Path,
    chip_name: &str,
    index: u32,
) -> Result<Option<TempSensor>> {
    let value = match reader.read_i64(&chip_dir.join(format!("temp{index}_input"))) {
        Ok(Ok(v)) => MilliCelsius::from_millidegrees(v),
        Ok(Err(_)) | Err(_) => return Ok(None),
    };

    let label = read_optional_label(reader, &chip_dir.join(format!("temp{index}_label")))
        .unwrap_or_else(|| format!("{chip_name} temp{index}"));

    let max = read_optional_millicelsius(reader, &chip_dir.join(format!("temp{index}_max")));
    let crit = read_optional_millicelsius(reader, &chip_dir.join(format!("temp{index}_crit")));

    Ok(Some(TempSensor { label, value, max, crit }))
}

/// Read `fanN_input` plus its optional `_label`.
pub fn read_fan_sensor(
    reader: &SysfsReader,
    chip_dir: &Path,
    chip_name: &str,
    index: u32,
) -> Result<Option<FanSensor>> {
    let rpm = match reader.read_u64(&chip_dir.join(format!("fan{index}_input"))) {
        Ok(Ok(v)) => Rpm::from_rpm(v),
        Ok(Err(_)) | Err(_) => return Ok(None),
    };

    let label = read_optional_label(reader, &chip_dir.join(format!("fan{index}_label")))
        .unwrap_or_else(|| format!("{chip_name} fan{index}"));

    Ok(Some(FanSensor { label, rpm }))
}

/// `_label` is decoration on top of an already-valid `_input`: a garbled or
/// absent label must not cost the caller the reading itself, hence `Option`
/// rather than propagating through the outer `Result`.
fn read_optional_label(reader: &SysfsReader, path: &Path) -> Option<String> {
    match reader.read_first_line(path) {
        Ok(Ok(raw)) => Some(sanitize_kernel_string(&raw, MAX_LABEL_LEN)),
        _ => None,
    }
}

/// `_max`/`_crit` are decoration too — see [`read_optional_label`].
fn read_optional_millicelsius(reader: &SysfsReader, path: &Path) -> Option<MilliCelsius> {
    match reader.read_i64(path) {
        Ok(Ok(v)) => Some(MilliCelsius::from_millidegrees(v)),
        _ => None,
    }
}

/// Fallback for machines with no hwmon chips.
///
/// `sys/class/thermal/thermal_zoneN/{type,temp}`. Presented as a synthetic chip
/// named `thermal_zone` so the renderers don't need a second code path.
///
/// Trip points (`trip_point_N_temp`/`_type`) are a materially more involved
/// API than a chip's `_max`/`_crit` pair and this interface is explicitly the
/// poorer fallback — `max`/`crit` are left `None` here rather than partially
/// implementing trip-point parsing.
pub fn read_thermal_zones(reader: &SysfsReader) -> Result<Vec<HwmonChip>> {
    let names = match reader.list_dir(Path::new(SYS_THERMAL), &is_thermal_zone_dir)? {
        Ok(names) => names,
        Err(_) => return Ok(Vec::new()),
    };

    let mut temps = Vec::with_capacity(names.len().min(MAX_SENSORS_PER_CHIP));

    for name in names.into_iter().take(MAX_SENSORS_PER_CHIP) {
        let zone_dir = PathBuf::from(format!("{SYS_THERMAL}/{name}"));

        let value = match reader.read_i64(&zone_dir.join("temp")) {
            Ok(Ok(v)) => MilliCelsius::from_millidegrees(v),
            // This zone is unreadable right now — skip it, not the whole
            // fallback; same fail-soft granularity as the hwmon path.
            Ok(Err(_)) | Err(_) => continue,
        };

        // `type` (e.g. "x86_pkg_temp", "acpitz") is the closest thing this
        // interface has to a label. Falling back to the directory name itself
        // is still meaningful on its own.
        let label = read_optional_label(reader, &zone_dir.join("type")).unwrap_or(name);

        temps.push(TempSensor { label, value, max: None, crit: None });
    }

    if temps.is_empty() {
        return Ok(Vec::new());
    }

    Ok(vec![HwmonChip {
        name: "thermal_zone".to_string(),
        temps,
        fans: Vec::new(),
    }])
}

/// Assemble the whole sample. `None` when the machine exposes no sensors —
/// which is normal in a VM and must not be an error.
pub fn build_sample(chips: Vec<HwmonChip>) -> Option<ThermalSample> {
    if chips.is_empty() {
        None
    } else {
        Some(ThermalSample { chips })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sample::TempSeverity;
    use crate::sysfs::tests::TempTree;

    // ---- sensor_indices ------------------------------------------------------

    /// The trap this function exists to avoid: `k10temp` on the machine this
    /// crate was developed on reports temp1, temp3, temp4 with no temp2 at
    /// all. Counting up from 1 and stopping at the first gap would drop every
    /// sensor after it.
    #[test]
    fn sensor_indices_are_not_assumed_contiguous() {
        let tree = TempTree::new("thermal-indices");
        tree.dir("hwmon0");
        tree.file("hwmon0/temp1_input", "50000\n");
        tree.file("hwmon0/temp3_input", "39125\n");
        tree.file("hwmon0/temp4_input", "40500\n");
        // Present but not an `_input` file — must not be mistaken for index 2.
        tree.file("hwmon0/temp2_label", "ghost\n");
        let r = tree.reader();

        let indices = sensor_indices(&r, Path::new("hwmon0"), "temp").expect("lists fine");
        assert_eq!(indices, vec![1, 3, 4]);
    }

    #[test]
    fn sensor_indices_caps_at_the_per_chip_maximum() {
        let tree = TempTree::new("thermal-cap");
        tree.dir("hwmon0");
        for i in 0..MAX_SENSORS_PER_CHIP + 50 {
            tree.file(&format!("hwmon0/temp{i}_input"), "1000\n");
        }
        let r = tree.reader();

        let indices = sensor_indices(&r, Path::new("hwmon0"), "temp").expect("lists fine");
        assert_eq!(indices.len(), MAX_SENSORS_PER_CHIP);
    }

    #[test]
    fn a_missing_chip_directory_yields_no_indices_not_an_error() {
        let tree = TempTree::new("thermal-missing-dir");
        tree.dir("hwmon");
        let r = tree.reader();

        assert_eq!(
            sensor_indices(&r, Path::new("hwmon/hwmon0"), "temp").expect("not an error"),
            Vec::<u32>::new()
        );
    }

    // ---- read_chip -------------------------------------------------------------

    /// Captured from this machine's `r8169` NIC hwmon chip on 2026-08-05: it
    /// has a temperature and a `_max`, but genuinely no `temp1_label` at all
    /// — the common case, not an edge case.
    #[test]
    fn a_chip_with_no_labels_falls_back_to_a_synthetic_one() {
        let tree = TempTree::new("thermal-no-label");
        tree.file("hwmon0/name", "r8169_0_d00:00\n");
        tree.file("hwmon0/temp1_input", "39000\n");
        tree.file("hwmon0/temp1_max", "120000\n");
        let r = tree.reader();

        let chip = read_chip(&r, Path::new("hwmon0"))
            .expect("reads fine")
            .expect("chip has a sensor");

        assert_eq!(chip.name, "r8169_0_d00:00");
        assert_eq!(chip.temps.len(), 1);
        assert_eq!(chip.temps[0].label, "r8169_0_d00:00 temp1");
        assert_eq!(chip.temps[0].value.as_millidegrees(), 39_000);
        assert_eq!(chip.temps[0].max.map(MilliCelsius::as_millidegrees), Some(120_000));
        assert_eq!(chip.temps[0].crit, None);
        assert!(chip.fans.is_empty());
    }

    /// A GPU chip with a fan but temperature sensors, so the two must be
    /// populated independently rather than one implying the other.
    #[test]
    fn a_chip_with_fans_but_no_temps() {
        let tree = TempTree::new("thermal-fans-only");
        tree.file("hwmon0/name", "amdgpu\n");
        tree.file("hwmon0/fan1_input", "1850\n");
        tree.file("hwmon0/fan1_label", "intake\n");
        let r = tree.reader();

        let chip = read_chip(&r, Path::new("hwmon0"))
            .expect("reads fine")
            .expect("chip has a sensor");

        assert!(chip.temps.is_empty());
        assert_eq!(chip.fans.len(), 1);
        assert_eq!(chip.fans[0].label, "intake");
        assert_eq!(chip.fans[0].rpm.as_u64(), 1_850);
    }

    /// The other required fixture case: garbage in one sensor's `_input`
    /// must not fail the chip (or the collector) — it must just be absent
    /// from the result, exactly like a sensor that was never there.
    #[test]
    fn a_garbage_temp_input_skips_only_that_sensor() {
        let tree = TempTree::new("thermal-garbage");
        tree.file("hwmon0/name", "coretemp\n");
        tree.file("hwmon0/temp1_input", "not a number\n");
        tree.file("hwmon0/temp2_input", "45000\n");
        let r = tree.reader();

        let chip = read_chip(&r, Path::new("hwmon0"))
            .expect("a garbled sensor must not be a hard error")
            .expect("the other sensor still makes this a real chip");

        assert_eq!(chip.temps.len(), 1);
        assert_eq!(chip.temps[0].value.as_millidegrees(), 45_000);
    }

    /// A garbled `_max`/`_crit` must not cost the caller the temperature
    /// reading itself — only the decoration is lost.
    #[test]
    fn a_garbage_trip_point_does_not_lose_the_reading() {
        let tree = TempTree::new("thermal-garbage-trip");
        tree.file("hwmon0/name", "coretemp\n");
        tree.file("hwmon0/temp1_input", "45000\n");
        tree.file("hwmon0/temp1_max", "not a number\n");
        let r = tree.reader();

        let chip = read_chip(&r, Path::new("hwmon0")).expect("reads fine").expect("has a sensor");
        assert_eq!(chip.temps[0].value.as_millidegrees(), 45_000);
        assert_eq!(chip.temps[0].max, None);
    }

    /// Sensor labels are kernel-supplied and attacker-influenced on some
    /// systems (a USB device can name itself). They must be sanitised before
    /// they ever reach a `Snapshot`, not left to the renderer to remember.
    #[test]
    fn labels_are_sanitised_before_reaching_the_sample() {
        let tree = TempTree::new("thermal-hostile-label");
        tree.file("hwmon0/name", "coretemp\n");
        tree.file("hwmon0/temp1_input", "45000\n");
        tree.file("hwmon0/temp1_label", "\u{1b}[2JPackage\u{1b}[0m id 0\n");
        let r = tree.reader();

        let chip = read_chip(&r, Path::new("hwmon0")).expect("reads fine").expect("has a sensor");
        assert_eq!(chip.temps[0].label, "Package id 0");
    }

    /// A chip directory that exists but reports nothing this crate tracks
    /// (e.g. only `update_interval`, no `_input` files) must be indistinguishable
    /// from an absent chip.
    #[test]
    fn a_chip_reporting_nothing_is_treated_as_absent() {
        let tree = TempTree::new("thermal-empty-chip");
        tree.file("hwmon0/name", "ghost\n");
        tree.file("hwmon0/update_interval", "1000\n");
        let r = tree.reader();

        assert!(read_chip(&r, Path::new("hwmon0")).expect("reads fine").is_none());
    }

    #[test]
    fn a_chip_with_no_name_file_is_skipped() {
        let tree = TempTree::new("thermal-no-name");
        tree.file("hwmon0/temp1_input", "45000\n");
        let r = tree.reader();

        assert!(read_chip(&r, Path::new("hwmon0")).expect("reads fine").is_none());
    }

    // ---- read_hwmon_chips / build_sample ----------------------------------------

    /// The other required fixture case: an empty (or absent) `/sys/class/hwmon`
    /// must produce `None`, not an error — the ordinary VM/container case.
    #[test]
    fn an_empty_hwmon_directory_yields_no_chips() {
        let tree = TempTree::new("thermal-empty-hwmon");
        tree.dir("sys/class/hwmon");
        let r = tree.reader();

        assert!(read_hwmon_chips(&r).expect("not an error").is_empty());
        assert!(build_sample(Vec::new()).is_none());
    }

    #[test]
    fn a_missing_hwmon_directory_yields_no_chips() {
        let tree = TempTree::new("thermal-no-hwmon-at-all");
        tree.dir("sys");
        let r = tree.reader();

        assert!(read_hwmon_chips(&r).expect("not an error").is_empty());
    }

    /// Real values captured from this machine's `k10temp` chip on
    /// 2026-08-05: non-contiguous indices (1, 3, 4 — no temp2) mixed with a
    /// chip that has no `_max`/`_crit` at all for any sensor.
    #[test]
    fn parses_a_real_multi_sensor_chip() {
        let tree = TempTree::new("thermal-real-k10temp");
        tree.file("sys/class/hwmon/hwmon0/name", "k10temp\n");
        tree.file("sys/class/hwmon/hwmon0/temp1_label", "Tctl\n");
        tree.file("sys/class/hwmon/hwmon0/temp1_input", "50000\n");
        tree.file("sys/class/hwmon/hwmon0/temp3_label", "Tccd1\n");
        tree.file("sys/class/hwmon/hwmon0/temp3_input", "39125\n");
        tree.file("sys/class/hwmon/hwmon0/temp4_label", "Tccd2\n");
        tree.file("sys/class/hwmon/hwmon0/temp4_input", "40500\n");
        let r = tree.reader();

        let chips = read_hwmon_chips(&r).expect("reads fine");
        assert_eq!(chips.len(), 1);
        assert_eq!(chips[0].name, "k10temp");

        let labels: Vec<&str> = chips[0].temps.iter().map(|t| t.label.as_str()).collect();
        assert_eq!(labels, vec!["Tctl", "Tccd1", "Tccd2"]);

        let sample = build_sample(chips).expect("non-empty");
        assert_eq!(sample.chips[0].temps[0].severity(), TempSeverity::Unknown, "no trip points on this chip");
    }

    // ---- thermal_zone fallback ---------------------------------------------------

    #[test]
    fn falls_back_to_thermal_zones_when_hwmon_is_empty() {
        let tree = TempTree::new("thermal-zone-fallback");
        tree.dir("sys/class/hwmon"); // present, but empty
        tree.file("sys/class/thermal/thermal_zone0/type", "x86_pkg_temp\n");
        tree.file("sys/class/thermal/thermal_zone0/temp", "52000\n");
        tree.file("sys/class/thermal/thermal_zone1/type", "acpitz\n");
        tree.file("sys/class/thermal/thermal_zone1/temp", "48000\n");
        // A cooling device sits alongside thermal zones in this directory on
        // real hardware and must not be mistaken for a zone.
        tree.dir("sys/class/thermal/cooling_device0");
        let r = tree.reader();

        let mut collector = ThermalCollector::new();
        let mut snapshot = Snapshot::now();
        collector.collect(&r, &mut snapshot).expect("collects fine");

        let thermal = snapshot.thermal.expect("fallback produced a sample");
        assert_eq!(thermal.chips.len(), 1);
        assert_eq!(thermal.chips[0].name, "thermal_zone");
        assert_eq!(thermal.chips[0].temps.len(), 2);
        assert!(thermal.chips[0]
            .temps
            .iter()
            .any(|t| t.label == "x86_pkg_temp" && t.value.as_millidegrees() == 52_000));
    }

    #[test]
    fn no_sensors_anywhere_yields_no_thermal_sample() {
        let tree = TempTree::new("thermal-nothing");
        tree.dir("sys/class/hwmon");
        tree.dir("sys/class/thermal");
        let r = tree.reader();

        let mut collector = ThermalCollector::new();
        let mut snapshot = Snapshot::now();
        collector.collect(&r, &mut snapshot).expect("collects fine");

        assert!(snapshot.thermal.is_none());
        assert!(snapshot.errors.is_empty());
    }

    // ---- probe --------------------------------------------------------------

    #[test]
    fn probe_is_true_only_when_something_is_actually_there() {
        let empty = TempTree::new("thermal-probe-empty");
        empty.dir("sys/class/hwmon");
        assert!(!ThermalCollector::default().probe(&empty.reader()));

        let with_chip = TempTree::new("thermal-probe-chip");
        with_chip.dir("sys/class/hwmon/hwmon0");
        assert!(ThermalCollector::default().probe(&with_chip.reader()));

        let with_zone_only = TempTree::new("thermal-probe-zone");
        with_zone_only.dir("sys/class/hwmon"); // present but empty
        with_zone_only.dir("sys/class/thermal");
        assert!(ThermalCollector::default().probe(&with_zone_only.reader()));

        let nothing = TempTree::new("thermal-probe-nothing");
        nothing.dir("sys");
        assert!(!ThermalCollector::default().probe(&nothing.reader()));
    }

    // ---- ThermalCollector chip-directory caching --------------------------------

    /// The primary guarantee `read_cached_chips` makes: a chip that
    /// disappears between two refreshes (module unloaded, device unplugged)
    /// drops out within one `collect()` call, not eventually.
    #[test]
    fn a_disappearing_chip_is_dropped_on_the_next_collect() {
        let tree = TempTree::new("thermal-cache-disappear");
        tree.file("sys/class/hwmon/hwmon0/name", "keep\n");
        tree.file("sys/class/hwmon/hwmon0/temp1_input", "40000\n");
        tree.file("sys/class/hwmon/hwmon1/name", "gone\n");
        tree.file("sys/class/hwmon/hwmon1/temp1_input", "50000\n");
        let r = tree.reader();

        let mut collector = ThermalCollector::new();
        let mut snapshot = Snapshot::now();
        collector.collect(&r, &mut snapshot).expect("first collect");
        assert_eq!(snapshot.thermal.expect("has chips").chips.len(), 2);

        tree.remove("sys/class/hwmon/hwmon1");

        let mut snapshot = Snapshot::now();
        collector.collect(&r, &mut snapshot).expect("second collect after removal");
        let chips = snapshot.thermal.expect("still has a chip").chips;
        assert_eq!(chips.len(), 1);
        assert_eq!(chips[0].name, "keep");
    }

    /// The documented boundary of that guarantee: a chip appearing *on its
    /// own*, with nothing else changing, is not picked up until some other
    /// trigger causes a re-scan. This pins down the trade-off described in
    /// `ThermalCollector::read_cached_chips`'s doc comment so a future change
    /// to that behaviour is a deliberate decision, not a silent regression.
    #[test]
    fn a_new_chip_is_not_picked_up_by_itself() {
        let tree = TempTree::new("thermal-cache-new-chip-alone");
        tree.file("sys/class/hwmon/hwmon0/name", "original\n");
        tree.file("sys/class/hwmon/hwmon0/temp1_input", "40000\n");
        let r = tree.reader();

        let mut collector = ThermalCollector::new();
        let mut snapshot = Snapshot::now();
        collector.collect(&r, &mut snapshot).expect("first collect");
        assert_eq!(snapshot.thermal.expect("has a chip").chips.len(), 1);

        tree.file("sys/class/hwmon/hwmon1/name", "new-arrival\n");
        tree.file("sys/class/hwmon/hwmon1/temp1_input", "45000\n");

        let mut snapshot = Snapshot::now();
        collector.collect(&r, &mut snapshot).expect("second collect");
        assert_eq!(
            snapshot.thermal.expect("still has the original chip").chips.len(),
            1,
            "a chip appearing alone must not be picked up until something else triggers a re-scan"
        );
    }

    /// A chip disappearing and a different chip appearing in the same
    /// refresh: the disappearance triggers the re-scan, and the appearance
    /// is picked up as a side effect of that same re-scan.
    #[test]
    fn a_new_chip_is_picked_up_when_another_disappears_in_the_same_refresh() {
        let tree = TempTree::new("thermal-cache-swap");
        tree.file("sys/class/hwmon/hwmon0/name", "leaving\n");
        tree.file("sys/class/hwmon/hwmon0/temp1_input", "40000\n");
        let r = tree.reader();

        let mut collector = ThermalCollector::new();
        let mut snapshot = Snapshot::now();
        collector.collect(&r, &mut snapshot).expect("first collect");

        tree.remove("sys/class/hwmon/hwmon0");
        tree.file("sys/class/hwmon/hwmon1/name", "arriving\n");
        tree.file("sys/class/hwmon/hwmon1/temp1_input", "45000\n");

        let mut snapshot = Snapshot::now();
        collector.collect(&r, &mut snapshot).expect("second collect");
        let chips = snapshot.thermal.expect("has the new chip").chips;
        assert_eq!(chips.len(), 1);
        assert_eq!(chips[0].name, "arriving");
    }
}
