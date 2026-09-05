//! NVIDIA GPUs via NVML.
//!
//! # Blocked
//!
//! Needs the `nvml-wrapper` crate — proposal "rustmon C" in
//! `docs/crate-checklist.md`, **not approved**. Nothing in this module is
//! implemented, and the whole thing is behind the non-default `gpu-nvidia`
//! feature.
//!
//! # Why this is a separate feature rather than part of the GPU collector
//!
//! The NVIDIA proprietary driver exposes essentially nothing useful through
//! sysfs — no utilisation, no VRAM, no power. The only interface is NVML, which
//! means `dlopen`-ing `libnvidia-ml.so`, a closed-source vendor library, into
//! rustmon's own address space.
//!
//! That is a different kind of decision from reading a text file, and the
//! honest thing is to make it an explicit opt-in rather than bury it in a
//! default build. Consequences of the gating:
//!
//! - A default `cargo build` never links or loads NVML.
//! - With the feature on, NVML failing to initialise (no driver, no permission,
//!   version mismatch) degrades to "no NVIDIA GPU present". It never aborts and
//!   never retries in a loop.
//! - `nvidia-smi` is **not** a fallback. The design model forbids subprocesses
//!   outright — that removes PATH-hijack and command injection as a category
//!   rather than mitigating them, and re-introducing one process spawn to work
//!   around an unapproved crate would be exactly the wrong trade.
//!
//! If the target machine has an AMD or Intel GPU, the right move is to never
//! enable this feature at all.

use crate::error::Result;
use crate::sample::Gpu;

/// Handle to an initialised NVML session.
///
/// Held across refreshes: NVML initialisation is expensive and re-initialising
/// every second would cost more than the metrics are worth.
#[cfg(feature = "gpu-nvidia")]
#[derive(Debug)]
pub struct NvmlSession {
    // Will hold `nvml_wrapper::Nvml` once the crate proposal is approved.
    _private: (),
}

#[cfg(feature = "gpu-nvidia")]
impl NvmlSession {
    /// Initialise NVML.
    ///
    /// `Ok(None)` — not an error — when the library or driver isn't present.
    /// That is the common case on most machines and must stay quiet.
    pub fn init() -> Result<Option<Self>> {
        todo!("BLOCKED on crate proposal 'rustmon C' in docs/crate-checklist.md")
    }

    /// Number of NVIDIA GPUs visible to NVML.
    pub fn device_count(&self) -> Result<u32> {
        todo!("BLOCKED on crate proposal 'rustmon C'")
    }

    /// Read one device's metrics: utilisation, VRAM, temperature, power, fan.
    pub fn read_device(&self, _index: u32) -> Result<Option<Gpu>> {
        todo!("BLOCKED on crate proposal 'rustmon C'")
    }
}

/// Entry point used by [`super::read_card`] when a card's PCI vendor is NVIDIA.
///
/// Without the `gpu-nvidia` feature this always returns `Ok(None)`, so an
/// NVIDIA card simply doesn't appear. That's the intended default behaviour,
/// not a silent failure — the `--verbose` output notes that the card was seen
/// but the feature is off.
#[cfg(not(feature = "gpu-nvidia"))]
pub fn read(_card: &str) -> Result<Option<Gpu>> {
    Ok(None)
}

/// Feature-enabled variant. Requires an initialised [`NvmlSession`].
#[cfg(feature = "gpu-nvidia")]
pub fn read(_session: Option<&NvmlSession>, _index: u32) -> Result<Option<Gpu>> {
    todo!("BLOCKED on crate proposal 'rustmon C'")
}
