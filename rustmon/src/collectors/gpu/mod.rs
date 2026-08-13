//! GPU metrics, dispatched per vendor.
//!
//! The three vendors are genuinely different problems, which is why they're
//! three submodules rather than one function with branches:
//!
//! - **AMD** ([`amd`]) — rich sysfs. Utilisation, VRAM, temp, power, fan all
//!   available as plain files. Best-supported vendor here.
//! - **Intel** ([`intel`]) — partial sysfs. Frequency and some power; no
//!   utilisation percentage without perf counters, which are out of scope.
//! - **NVIDIA** ([`nvidia`]) — essentially nothing in sysfs. Requires NVML, a
//!   proprietary library, and is gated behind the non-default `gpu-nvidia`
//!   feature. See the security note below.
//!
//! Vendor is determined from `sys/class/drm/cardN/device/vendor`, a PCI vendor
//! ID, not from a driver name string.
//!
//! # Security note on NVIDIA
//!
//! Every other collector in this crate reads text files. NVML means `dlopen`-ing
//! a closed-source vendor library into rustmon's own address space — a
//! materially different trust decision, and the reason it is a separate,
//! non-default feature with its own crate-checklist proposal rather than part
//! of this collector. A default build never loads it.
//!
//! Shelling out to `nvidia-smi` is **not** an acceptable alternative: the design
//! model forbids subprocesses entirely, which removes PATH-hijack and command
//! injection as a category rather than mitigating them.

pub mod amd;
pub mod intel;
pub mod nvidia;

use std::path::{Path, PathBuf};

use crate::collector::Collector;
use crate::error::Result;
use crate::sample::{CollectorError, Gpu, GpuSample, GpuVendor, Snapshot};
use crate::sysfs::SysfsReader;

pub const NAME: &str = "gpu";

const SYS_CLASS_DRM: &str = "sys/class/drm";

/// PCI vendor IDs, as they appear in `device/vendor`.
pub const PCI_VENDOR_AMD: &str = "0x1002";
pub const PCI_VENDOR_INTEL: &str = "0x8086";
pub const PCI_VENDOR_NVIDIA: &str = "0x10de";

#[derive(Debug, Default)]
pub struct GpuCollector {
    /// `cardN` directories found at probe time, with their resolved vendor.
    cards: Vec<(String, GpuVendor)>,
}

impl GpuCollector {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Collector for GpuCollector {
    fn name(&self) -> &'static str {
        NAME
    }

    fn probe(&self, reader: &SysfsReader) -> bool {
        let Ok(cards) = enumerate_cards(reader) else {
            return false;
        };
        cards
            .iter()
            .any(|card| reader.exists(&device_dir(card).join("vendor")))
    }

    fn collect(&mut self, reader: &SysfsReader, snapshot: &mut Snapshot) -> Result<()> {
        // Cached once and reused across refreshes, like `chip_dirs` in
        // `ThermalCollector` — but deliberately *without* that struct's
        // vanish-triggers-a-rescan staleness check. Hot-plug GPUs are rare
        // enough (unlike hwmon chips, which routinely appear on module load)
        // that "enumerate once, done" is the honest trade-off here rather
        // than copying machinery this collector doesn't need.
        if self.cards.is_empty() {
            for name in enumerate_cards(reader)? {
                // `read_vendor` never actually returns `Err` — see its own
                // doc — so this can't silently drop a card on a transient
                // read failure.
                let vendor = read_vendor(reader, &name)?;
                self.cards.push((name, vendor));
            }
        }

        let mut gpus = Vec::new();

        for (name, vendor) in &self.cards {
            match read_card(reader, name, *vendor) {
                Ok(Some(gpu)) => gpus.push(gpu),
                Ok(None) => {}
                // One card failing (a genuinely malformed sysfs value) must
                // not blank every other GPU on the same machine.
                Err(e) => snapshot.errors.push(CollectorError {
                    collector: NAME,
                    message: e.to_string(),
                }),
            }
        }

        snapshot.gpus = build_sample(gpus);
        Ok(())
    }
}

fn is_card_dir(name: &str) -> bool {
    name.strip_prefix("card")
        .is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()))
}

/// Enumerate `cardN` entries, excluding the `cardN-<connector>` output nodes
/// (`card0-DP-1`, `card0-HDMI-A-1`) which are display connectors, not GPUs.
pub fn enumerate_cards(reader: &SysfsReader) -> Result<Vec<String>> {
    match reader.list_dir(Path::new(SYS_CLASS_DRM), &is_card_dir)? {
        Ok(names) => Ok(names),
        // `/sys/class/drm` missing entirely (a container with `/sys` masked,
        // or genuinely no DRM subsystem) — not fatal, just no GPUs.
        Err(_) => Ok(Vec::new()),
    }
}

/// Read `cardN/device/vendor` and map the PCI ID to a [`GpuVendor`].
///
/// Always returns `Ok(_)` — a missing file, a permission error, or an
/// unrecognised PCI ID all map to [`GpuVendor::Unknown`] rather than an
/// error. An unsupported GPU is exactly as absent as no GPU at all.
pub fn read_vendor(reader: &SysfsReader, card: &str) -> Result<GpuVendor> {
    let vendor = match reader.read_first_line(&device_dir(card).join("vendor")) {
        Ok(Ok(id)) => match id.as_str() {
            PCI_VENDOR_AMD => GpuVendor::Amd,
            PCI_VENDOR_INTEL => GpuVendor::Intel,
            PCI_VENDOR_NVIDIA => GpuVendor::Nvidia,
            _ => GpuVendor::Unknown,
        },
        _ => GpuVendor::Unknown,
    };
    Ok(vendor)
}

/// Dispatch one card to its vendor module.
///
/// An unknown vendor yields `Ok(None)` — a virtual or unsupported GPU is not an
/// error, it's just absent.
pub fn read_card(reader: &SysfsReader, card: &str, vendor: GpuVendor) -> Result<Option<Gpu>> {
    match vendor {
        GpuVendor::Amd => amd::read(reader, card),
        GpuVendor::Intel => intel::read(reader, card),
        GpuVendor::Nvidia => read_nvidia(card),
        GpuVendor::Unknown => Ok(None),
    }
}

/// `nvidia::read`'s signature differs by feature (see that module): the
/// no-feature path takes just a card name, the `gpu-nvidia` path needs an
/// `NvmlSession` this collector doesn't hold (chunk 10's scope is AMD/Intel
/// only — see `docs/TODO-rustmon.md`). Isolating the `#[cfg]` to this one
/// small function keeps `read_card` itself readable.
#[cfg(not(feature = "gpu-nvidia"))]
fn read_nvidia(card: &str) -> Result<Option<Gpu>> {
    nvidia::read(card)
}

#[cfg(feature = "gpu-nvidia")]
fn read_nvidia(_card: &str) -> Result<Option<Gpu>> {
    Ok(None)
}

/// Assemble the sample; `None` when no GPU yielded anything.
pub fn build_sample(gpus: Vec<Gpu>) -> Option<GpuSample> {
    if gpus.is_empty() {
        None
    } else {
        Some(GpuSample { gpus })
    }
}

/// Path to a card's own directory (`sys/class/drm/cardN`) — where i915/xe
/// expose frequency files directly, as opposed to [`device_dir`]'s
/// `cardN/device`, where vendor, VRAM, and the nested hwmon node live.
pub fn card_dir(card: &str) -> PathBuf {
    Path::new(SYS_CLASS_DRM).join(card)
}

/// Path to a card's `device` directory, used by every vendor module.
pub fn device_dir(card: &str) -> PathBuf {
    card_dir(card).join("device")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sysfs::tests::TempTree;

    #[test]
    fn enumerate_cards_excludes_connector_output_nodes() {
        let tree = TempTree::new("gpu-enumerate");
        tree.dir("sys/class/drm/card0");
        tree.dir("sys/class/drm/card1");
        // Display connectors, not GPUs — must not be mistaken for cardN.
        tree.dir("sys/class/drm/card0-DP-1");
        tree.dir("sys/class/drm/card0-HDMI-A-1");
        let r = tree.reader();

        assert_eq!(enumerate_cards(&r).expect("lists fine"), vec!["card0", "card1"]);
    }

    #[test]
    fn enumerate_cards_is_empty_not_an_error_when_drm_is_absent() {
        let tree = TempTree::new("gpu-no-drm");
        tree.dir("sys");
        let r = tree.reader();
        assert!(enumerate_cards(&r).expect("not an error").is_empty());
    }

    /// Real PCI vendor IDs captured from this machine's two AMD GPUs on
    /// 2026-08-11: `0x1002` (AMD). Also checks Intel's and NVIDIA's IDs and
    /// the unknown/garbage cases.
    #[test]
    fn read_vendor_maps_real_pci_ids() {
        let tree = TempTree::new("gpu-vendor");
        tree.file("sys/class/drm/card1/device/vendor", "0x1002\n");
        tree.file("sys/class/drm/card2/device/vendor", "0x8086\n");
        tree.file("sys/class/drm/card3/device/vendor", "0x10de\n");
        tree.file("sys/class/drm/card4/device/vendor", "0xbeef\n");
        let r = tree.reader();

        assert_eq!(read_vendor(&r, "card1").expect("no error"), GpuVendor::Amd);
        assert_eq!(read_vendor(&r, "card2").expect("no error"), GpuVendor::Intel);
        assert_eq!(read_vendor(&r, "card3").expect("no error"), GpuVendor::Nvidia);
        assert_eq!(read_vendor(&r, "card4").expect("no error"), GpuVendor::Unknown);
        // No vendor file at all — same as unrecognised, not an error.
        assert_eq!(read_vendor(&r, "card5").expect("no error"), GpuVendor::Unknown);
    }

    #[test]
    fn read_card_dispatches_by_vendor_and_unknown_yields_none() {
        let tree = TempTree::new("gpu-read-card-unknown");
        tree.dir("sys/class/drm/card0/device");
        let r = tree.reader();

        assert!(read_card(&r, "card0", GpuVendor::Unknown)
            .expect("not an error")
            .is_none());
    }

    #[test]
    fn build_sample_is_none_for_an_empty_list() {
        assert!(build_sample(Vec::new()).is_none());
    }

    /// This exact machine has two AMD cards (`card1`, `card2`), both
    /// discovered and read through the full `GpuCollector::collect` path,
    /// not just the lower-level helpers the other tests target directly.
    #[test]
    fn collector_reads_two_real_amd_cards() {
        let tree = TempTree::new("gpu-collector-two-cards");
        tree.file("sys/class/drm/card1/device/vendor", "0x1002\n");
        tree.file("sys/class/drm/card1/device/gpu_busy_percent", "2\n");
        tree.file("sys/class/drm/card2/device/vendor", "0x1002\n");
        tree.file("sys/class/drm/card2/device/gpu_busy_percent", "0\n");
        let r = tree.reader();

        let mut collector = GpuCollector::new();
        let mut snapshot = Snapshot::now();
        collector.collect(&r, &mut snapshot).expect("collects fine");

        let sample = snapshot.gpus.expect("two real GPUs present");
        assert_eq!(sample.gpus.len(), 2);
        assert!(sample.gpus.iter().all(|g| g.vendor == GpuVendor::Amd));
        assert!(snapshot.errors.is_empty());
    }

    #[test]
    fn collector_reports_none_when_no_gpu_is_present() {
        let tree = TempTree::new("gpu-collector-none");
        tree.dir("sys/class/drm");
        let r = tree.reader();

        let mut collector = GpuCollector::new();
        let mut snapshot = Snapshot::now();
        collector.collect(&r, &mut snapshot).expect("collects fine");
        assert!(snapshot.gpus.is_none());
    }

    #[test]
    fn probe_is_true_only_with_a_readable_vendor_file() {
        let with_gpu = TempTree::new("gpu-probe-yes");
        with_gpu.file("sys/class/drm/card0/device/vendor", "0x1002\n");
        assert!(GpuCollector::new().probe(&with_gpu.reader()));

        let card_no_vendor = TempTree::new("gpu-probe-card-no-vendor");
        card_no_vendor.dir("sys/class/drm/card0/device");
        assert!(!GpuCollector::new().probe(&card_no_vendor.reader()));

        let nothing = TempTree::new("gpu-probe-nothing");
        nothing.dir("sys");
        assert!(!GpuCollector::new().probe(&nothing.reader()));
    }
}
