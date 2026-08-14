//! The data model. Plain data, no I/O, no rates.
//!
//! Two rules hold this module together:
//!
//! 1. **Nothing here performs I/O.** Everything is constructed by a collector
//!    and consumed by a renderer, so a `Snapshot` can be built by hand in a
//!    test with no filesystem involved.
//! 2. **Nothing here is a rate.** `/proc/diskstats` and `/proc/net/dev` give
//!    monotonic counters. Turning those into MiB/s needs two snapshots and the
//!    time between them, which is [`crate::delta`]'s job. Keeping rates out of
//!    here means collectors stay pure "read and parse" and are testable against
//!    a static fixture tree with no timing involved.
//!
//! Per-area fields are `Option` and failures go in `errors` — Rust Book Ch. 6's
//! `Option`-for-normal-absence / `Result`-for-failure distinction applied at
//! the struct level. A machine with no hwmon chips, no GPU, or a container with
//! `/sys` partly masked still yields a useful snapshot.

use std::net::IpAddr;
use std::time::{Instant, SystemTime};

use crate::units::{Bytes, KiloHertz, MicroWatts, MilliCelsius, Percent, Rpm};

/// One complete reading of the machine.
#[derive(Debug, Clone)]
pub struct Snapshot {
    /// Wall-clock time, for display and for the JSON output only.
    pub taken_at: SystemTime,
    /// Monotonic time, for interval arithmetic. Never use `taken_at` for that —
    /// a clock step (NTP, suspend/resume) would produce a negative or absurd
    /// interval and a garbage rate.
    pub taken_at_monotonic: Instant,

    pub cpu: Option<CpuSample>,
    pub memory: Option<MemorySample>,
    pub thermal: Option<ThermalSample>,
    pub disks: Option<DiskSample>,
    pub net: Option<NetSample>,
    pub gpus: Option<GpuSample>,
    pub connections: Option<ConnectionSample>,

    /// Collectors that failed. Non-empty here does not make the snapshot
    /// invalid — it's the `--verbose` / diagnostics channel.
    pub errors: Vec<CollectorError>,
}

impl Snapshot {
    /// An empty snapshot stamped now. Collectors fill it in.
    pub fn now() -> Self {
        Snapshot {
            taken_at: SystemTime::now(),
            taken_at_monotonic: Instant::now(),
            cpu: None,
            memory: None,
            thermal: None,
            disks: None,
            net: None,
            gpus: None,
            connections: None,
            errors: Vec::new(),
        }
    }

    /// Monotonic seconds between two snapshots.
    ///
    /// `None` if `self` is not strictly after `earlier`; callers must drop the
    /// interval rather than substituting zero.
    pub fn elapsed_since(&self, earlier: &Snapshot) -> Option<f64> {
        let elapsed = self
            .taken_at_monotonic
            .checked_duration_since(earlier.taken_at_monotonic)?
            .as_secs_f64();

        // Zero is rejected as well as negative: two snapshots taken inside one
        // clock tick would otherwise divide a counter delta by zero.
        if elapsed > 0.0 {
            Some(elapsed)
        } else {
            None
        }
    }
}

/// A collector that failed, and which one.
#[derive(Debug, Clone)]
pub struct CollectorError {
    pub collector: &'static str,
    pub message: String,
}

// ---------------------------------------------------------------------------
// CPU
// ---------------------------------------------------------------------------

/// From `/proc/stat`, `/proc/loadavg`, and `cpufreq/scaling_cur_freq`.
#[derive(Debug, Clone)]
pub struct CpuSample {
    /// Aggregate ("cpu ") jiffy counters.
    pub total: CpuTimes,
    /// Per-logical-core counters, index = core number.
    pub per_core: Vec<CpuTimes>,
    /// Current frequency per core, where the driver reports it.
    pub freq_khz: Vec<Option<KiloHertz>>,
    /// 1 / 5 / 15 minute load averages from `/proc/loadavg`.
    pub load_avg: Option<[f64; 3]>,
    /// Model name from `/proc/cpuinfo`, sanitised.
    pub model: Option<String>,
    /// Context switches and boot time, for uptime display.
    pub ctxt: Option<u64>,
    pub btime: Option<u64>,
}

/// Raw jiffy counters for one CPU. Monotonic — utilisation is a delta.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CpuTimes {
    pub user: u64,
    pub nice: u64,
    pub system: u64,
    pub idle: u64,
    pub iowait: u64,
    pub irq: u64,
    pub softirq: u64,
    pub steal: u64,
    pub guest: u64,
    pub guest_nice: u64,
}

impl CpuTimes {
    /// Sum of every field, saturating.
    ///
    /// Note `guest`/`guest_nice` are already counted inside `user`/`nice` — the
    /// kernel double-reports them, so they must be excluded here or busy% comes
    /// out wrong on a VM host. That's the subtle bug this method exists to own.
    pub fn total(&self) -> u64 {
        // `guest` and `guest_nice` are deliberately absent from this list.
        // The kernel already folds them into `user` and `nice`, so adding them
        // again inflates the denominator and understates busy% on a VM host.
        [
            self.user,
            self.nice,
            self.system,
            self.idle,
            self.iowait,
            self.irq,
            self.softirq,
            self.steal,
        ]
        .into_iter()
        .fold(0u64, u64::saturating_add)
    }

    /// Time not spent idle (excludes `idle` and `iowait`).
    pub fn busy(&self) -> u64 {
        self.total()
            .saturating_sub(self.idle)
            .saturating_sub(self.iowait)
    }
}

// ---------------------------------------------------------------------------
// Memory
// ---------------------------------------------------------------------------

/// From `/proc/meminfo`. Remember: that file reports **kB**.
#[derive(Debug, Clone, Default)]
pub struct MemorySample {
    pub total: Bytes,
    pub free: Bytes,
    /// `MemAvailable` — the figure to show as "free", not `MemFree`.
    pub available: Bytes,
    pub buffers: Bytes,
    pub cached: Bytes,
    pub swap_total: Bytes,
    pub swap_free: Bytes,
}

impl MemorySample {
    /// `total - available`, the number a user actually means by "used".
    pub fn used(&self) -> Bytes {
        Bytes::from_bytes(self.total.as_u64().saturating_sub(self.available.as_u64()))
    }

    /// `None` when `total` is zero — a machine that reports no memory is not a
    /// machine that is 0% used.
    pub fn used_percent(&self) -> Option<Percent> {
        Percent::from_ratio(self.used().as_u64(), self.total.as_u64())
    }

    pub fn swap_used(&self) -> Bytes {
        Bytes::from_bytes(
            self.swap_total
                .as_u64()
                .saturating_sub(self.swap_free.as_u64()),
        )
    }
}

// ---------------------------------------------------------------------------
// Thermal + fans
// ---------------------------------------------------------------------------

/// Every hwmon chip found, plus thermal-zone fallbacks.
#[derive(Debug, Clone, Default)]
pub struct ThermalSample {
    pub chips: Vec<HwmonChip>,
}

/// One `/sys/class/hwmon/hwmonN`.
#[derive(Debug, Clone)]
pub struct HwmonChip {
    /// Contents of `name`, sanitised (e.g. `coretemp`, `k10temp`, `nvme`).
    pub name: String,
    pub temps: Vec<TempSensor>,
    pub fans: Vec<FanSensor>,
}

#[derive(Debug, Clone)]
pub struct TempSensor {
    /// `tempN_label` if present, else `"<chip name> tempN"` — see
    /// `collectors::thermal`'s module doc for why a bare `tempN` isn't enough
    /// (this machine's `r8169` NIC hwmon chip has no label at all).
    pub label: String,
    pub value: MilliCelsius,
    /// `tempN_max` — sustained limit.
    pub max: Option<MilliCelsius>,
    /// `tempN_crit` — hardware shutdown point.
    pub crit: Option<MilliCelsius>,
}

impl TempSensor {
    /// How close to critical, for colouring the display.
    ///
    /// `crit` is checked before `max` because a chip that reports both and is
    /// past the shutdown point should read Critical, not Warning.
    pub fn severity(&self) -> TempSeverity {
        if let Some(crit) = self.crit {
            if self.value.at_or_above(crit) {
                return TempSeverity::Critical;
            }
        }

        if let Some(max) = self.max {
            if self.value.at_or_above(max) {
                return TempSeverity::Warning;
            }
        }

        // No trip points at all means we genuinely cannot say whether 70 C is
        // fine or alarming for this sensor — which is different from saying
        // it's fine.
        if self.crit.is_none() && self.max.is_none() {
            return TempSeverity::Unknown;
        }

        TempSeverity::Normal
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TempSeverity {
    Unknown,
    Normal,
    Warning,
    Critical,
}

#[derive(Debug, Clone)]
pub struct FanSensor {
    pub label: String,
    pub rpm: Rpm,
}

// ---------------------------------------------------------------------------
// Disk
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct DiskSample {
    /// Whole devices from `/proc/diskstats`, partitions and loop/ram/zram
    /// devices filtered out.
    pub devices: Vec<DiskDevice>,
    /// Mounted filesystems from `/proc/self/mounts`.
    pub mounts: Vec<MountPoint>,
}

/// Raw counters for one block device. All monotonic — rates come from
/// [`crate::delta`].
#[derive(Debug, Clone)]
pub struct DiskDevice {
    pub name: String,
    pub reads_completed: u64,
    pub writes_completed: u64,
    /// In 512-byte sectors, per the kernel's fixed convention — NOT the
    /// device's physical sector size. Convert with [`Bytes::from_sectors_512`].
    pub sectors_read: u64,
    pub sectors_written: u64,
    /// Milliseconds spent doing I/O; the basis for utilisation%.
    pub io_ticks_ms: u64,
}

#[derive(Debug, Clone)]
pub struct MountPoint {
    pub source: String,
    pub mount_point: String,
    pub fs_type: String,
    /// `None` until the `fs-capacity` feature lands — capacity needs
    /// `statvfs`, which `std` doesn't expose. See chunk 8.
    pub total: Option<Bytes>,
    pub available: Option<Bytes>,
}

// ---------------------------------------------------------------------------
// Network
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct NetSample {
    pub interfaces: Vec<NetInterface>,
}

/// Counters from `/proc/net/dev` plus state from `/sys/class/net`.
#[derive(Debug, Clone)]
pub struct NetInterface {
    pub name: String,
    pub rx_bytes: u64,
    pub tx_bytes: u64,
    pub rx_packets: u64,
    pub tx_packets: u64,
    pub rx_errors: u64,
    pub tx_errors: u64,
    pub rx_dropped: u64,
    pub tx_dropped: u64,
    /// `operstate`: `up`, `down`, `unknown`, ...
    pub operstate: Option<String>,
    pub mtu: Option<u64>,
}

// ---------------------------------------------------------------------------
// GPU
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct GpuSample {
    pub gpus: Vec<Gpu>,
}

#[derive(Debug, Clone)]
pub struct Gpu {
    pub vendor: GpuVendor,
    /// e.g. `card0`.
    pub name: String,
    pub busy: Option<Percent>,
    pub vram_total: Option<Bytes>,
    pub vram_used: Option<Bytes>,
    pub temp: Option<MilliCelsius>,
    pub power: Option<MicroWatts>,
    pub fan_rpm: Option<Rpm>,
    /// Core clock. Chunk 0's original struct had nowhere to put this, but
    /// `collectors::gpu::intel`'s own module doc calls `gt_cur_freq_mhz`
    /// "the most useful signal this module can offer" — Intel has no
    /// utilisation percentage in sysfs at all, so without this field that
    /// collector's single best reading would have nowhere to go. Added
    /// during chunk 10 rather than left as a silently-dropped capability.
    /// `None` for AMD and NVIDIA today; nothing stops a later chunk wiring
    /// AMD's own clock file into it.
    pub freq_khz: Option<KiloHertz>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GpuVendor {
    #[default]
    Unknown,
    Amd,
    Intel,
    /// Only ever populated with the non-default `gpu-nvidia` feature.
    Nvidia,
}

// ---------------------------------------------------------------------------
// Connections
// ---------------------------------------------------------------------------

/// Every internet-facing TCP/UDP connection found on the machine, from
/// `/proc/net/{tcp,tcp6,udp,udp6}`. Loopback, link-local, private, and
/// multicast remote addresses — and bare listening sockets with no peer —
/// are filtered out at the collector boundary; see
/// `collectors::connections`'s module doc for why and exactly which ranges.
#[derive(Debug, Clone, Default)]
pub struct ConnectionSample {
    pub connections: Vec<Connection>,
}

#[derive(Debug, Clone)]
pub struct Connection {
    pub protocol: ConnProtocol,
    pub local_addr: IpAddr,
    pub local_port: u16,
    pub remote_addr: IpAddr,
    pub remote_port: u16,
    /// `None` for UDP, which has no connection state — the kernel's own
    /// `st` column is not a meaningful concept for a datagram socket the
    /// way it is for TCP.
    pub state: Option<TcpState>,
    /// The socket's owning uid, read directly from `/proc/net/{tcp,udp}*`
    /// itself — always available regardless of whether `pid`/`program`
    /// could be resolved, so a connection is never fully anonymous.
    pub uid: u32,
    /// `None` if the owning process couldn't be attributed — typically
    /// because it belongs to a different user (`EACCES` walking their
    /// `/proc/<pid>/fd`), the same fail-soft story as a root-only hwmon
    /// sensor elsewhere in this crate.
    pub pid: Option<u32>,
    /// `/proc/<pid>/comm`, sanitised. `None` exactly when `pid` is `None`.
    pub program: Option<String>,
    /// The owning process's parent pid, from `/proc/<pid>/status`'s `PPid`
    /// field — used by the connections panel to group subprocesses under
    /// their parent. `None` exactly when `pid` is `None` (nothing to read
    /// a parent for), or if that read itself failed (same fail-soft story
    /// as `program`).
    pub ppid: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConnProtocol {
    Tcp,
    Udp,
}

/// `/proc/net/tcp{,6}`'s `st` column — the kernel's own `TCP_STATES` enum
/// (`include/net/tcp_states.h`), decoded from its hex value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TcpState {
    Established,
    SynSent,
    SynRecv,
    FinWait1,
    FinWait2,
    TimeWait,
    Close,
    CloseWait,
    LastAck,
    Listen,
    Closing,
    /// A state value newer than this list, or a corrupted read — reported
    /// as-is rather than treated as a parse error, since `st` isn't this
    /// crate's identity for the row the way a device name is elsewhere.
    Unknown(u8),
}

impl TcpState {
    /// Decode the kernel's hex `st` value. Linux's `TCP_ESTABLISHED` is `1`,
    /// not `0`, so every variant here is offset by one from its discriminant.
    pub fn from_byte(b: u8) -> TcpState {
        match b {
            0x01 => TcpState::Established,
            0x02 => TcpState::SynSent,
            0x03 => TcpState::SynRecv,
            0x04 => TcpState::FinWait1,
            0x05 => TcpState::FinWait2,
            0x06 => TcpState::TimeWait,
            0x07 => TcpState::Close,
            0x08 => TcpState::CloseWait,
            0x09 => TcpState::LastAck,
            0x0a => TcpState::Listen,
            0x0b => TcpState::Closing,
            other => TcpState::Unknown(other),
        }
    }

    /// Short, stable, lowercase — used by both renderers.
    pub fn as_str(self) -> &'static str {
        match self {
            TcpState::Established => "established",
            TcpState::SynSent => "syn_sent",
            TcpState::SynRecv => "syn_recv",
            TcpState::FinWait1 => "fin_wait1",
            TcpState::FinWait2 => "fin_wait2",
            TcpState::TimeWait => "time_wait",
            TcpState::Close => "close",
            TcpState::CloseWait => "close_wait",
            TcpState::LastAck => "last_ack",
            TcpState::Listen => "listen",
            TcpState::Closing => "closing",
            TcpState::Unknown(_) => "unknown",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    // ---- CpuTimes ----------------------------------------------------------

    fn sample_times() -> CpuTimes {
        CpuTimes {
            user: 100,
            nice: 10,
            system: 20,
            idle: 1_000,
            iowait: 5,
            irq: 1,
            softirq: 2,
            steal: 3,
            guest: 50,
            guest_nice: 7,
        }
    }

    /// The trap this method exists to own: the kernel folds `guest` into
    /// `user` and `guest_nice` into `nice`. Summing all ten fields inflates
    /// the denominator and understates busy% on a VM host.
    #[test]
    fn total_excludes_the_double_counted_guest_fields() {
        let t = sample_times();
        let first_eight = 100 + 10 + 20 + 1_000 + 5 + 1 + 2 + 3;

        assert_eq!(t.total(), first_eight);
        assert_ne!(
            t.total(),
            first_eight + 50 + 7,
            "guest/guest_nice must not be added again"
        );
    }

    #[test]
    fn busy_is_everything_that_is_not_idle_or_iowait() {
        let t = sample_times();
        assert_eq!(t.busy(), t.total() - 1_000 - 5);
        assert_eq!(CpuTimes::default().busy(), 0);
    }

    #[test]
    fn counter_arithmetic_saturates_rather_than_overflowing() {
        let maxed = CpuTimes {
            user: u64::MAX,
            nice: u64::MAX,
            system: u64::MAX,
            idle: u64::MAX,
            iowait: u64::MAX,
            irq: u64::MAX,
            softirq: u64::MAX,
            steal: u64::MAX,
            guest: u64::MAX,
            guest_nice: u64::MAX,
        };

        assert_eq!(maxed.total(), u64::MAX);
        // total() saturates at MAX, so subtracting two MAXes must floor at 0
        // rather than wrapping to something enormous.
        assert_eq!(maxed.busy(), 0);
    }

    // ---- MemorySample ------------------------------------------------------

    fn mem(total: u64, available: u64) -> MemorySample {
        MemorySample {
            total: Bytes::from_bytes(total),
            available: Bytes::from_bytes(available),
            ..MemorySample::default()
        }
    }

    #[test]
    fn used_is_total_minus_available() {
        let m = mem(1_000, 400);
        assert_eq!(m.used().as_u64(), 600);
        assert!((m.used_percent().expect("non-zero total").as_f64() - 60.0).abs() < 1e-9);
    }

    #[test]
    fn a_machine_reporting_no_memory_has_no_percentage() {
        // Not 0% used — we simply have nothing to divide by.
        assert_eq!(mem(0, 0).used_percent(), None);
    }

    #[test]
    fn available_exceeding_total_floors_at_zero_used() {
        // Shouldn't happen, but a saturating floor beats a wrapped u64 the
        // size of the address space showing up in the UI.
        assert_eq!(mem(1_000, 2_000).used().as_u64(), 0);
    }

    #[test]
    fn swap_used_saturates_too() {
        let m = MemorySample {
            swap_total: Bytes::from_bytes(100),
            swap_free: Bytes::from_bytes(400),
            ..MemorySample::default()
        };
        assert_eq!(m.swap_used().as_u64(), 0);
    }

    // ---- TempSensor::severity ---------------------------------------------

    fn sensor(value: i64, max: Option<i64>, crit: Option<i64>) -> TempSensor {
        TempSensor {
            label: "Package id 0".to_string(),
            value: MilliCelsius::from_millidegrees(value),
            max: max.map(MilliCelsius::from_millidegrees),
            crit: crit.map(MilliCelsius::from_millidegrees),
        }
    }

    #[test]
    fn severity_ladder() {
        assert_eq!(
            sensor(50_000, Some(85_000), Some(95_000)).severity(),
            TempSeverity::Normal
        );
        // Trip points are inclusive.
        assert_eq!(
            sensor(85_000, Some(85_000), Some(95_000)).severity(),
            TempSeverity::Warning
        );
        assert_eq!(
            sensor(95_000, Some(85_000), Some(95_000)).severity(),
            TempSeverity::Critical
        );
    }

    /// A sensor past both trip points must read Critical, not Warning — so
    /// `crit` has to be tested first.
    #[test]
    fn crit_outranks_max() {
        assert_eq!(
            sensor(99_000, Some(85_000), Some(95_000)).severity(),
            TempSeverity::Critical
        );
    }

    /// "We don't know whether this is hot" is a different claim from "this is
    /// fine", and plenty of chips report no trip points at all.
    #[test]
    fn no_trip_points_means_unknown_not_normal() {
        assert_eq!(sensor(70_000, None, None).severity(), TempSeverity::Unknown);
        assert_eq!(
            sensor(70_000, Some(85_000), None).severity(),
            TempSeverity::Normal
        );
    }

    // ---- Snapshot ----------------------------------------------------------

    #[test]
    fn a_fresh_snapshot_is_empty() {
        let s = Snapshot::now();
        assert!(s.cpu.is_none() && s.memory.is_none() && s.thermal.is_none());
        assert!(s.disks.is_none() && s.net.is_none() && s.gpus.is_none());
        assert!(s.connections.is_none());
        assert!(s.errors.is_empty());
    }

    // ---- TcpState ------------------------------------------------------------

    #[test]
    fn tcp_state_decodes_the_kernel_enum() {
        assert_eq!(TcpState::from_byte(0x01), TcpState::Established);
        assert_eq!(TcpState::from_byte(0x0a), TcpState::Listen);
        assert_eq!(TcpState::from_byte(0x0b), TcpState::Closing);
    }

    #[test]
    fn tcp_state_unknown_values_are_preserved_not_dropped() {
        assert_eq!(TcpState::from_byte(0xff), TcpState::Unknown(0xff));
        assert_eq!(TcpState::Unknown(0xff).as_str(), "unknown");
    }

    /// Intervals are built by hand rather than by taking two real readings:
    /// two `Instant::now()` calls can land in the same tick, which would make
    /// this test fail intermittently for a reason unrelated to the logic.
    #[test]
    fn elapsed_since_measures_forward_intervals_only() {
        let earlier = Snapshot::now();
        let mut later = Snapshot::now();
        later.taken_at_monotonic = earlier.taken_at_monotonic + Duration::from_millis(500);

        assert_eq!(later.elapsed_since(&earlier), Some(0.5));

        // Backwards: the caller must drop the interval, not divide by a
        // negative.
        assert_eq!(earlier.elapsed_since(&later), None);
    }

    #[test]
    fn a_zero_length_interval_yields_no_elapsed_time() {
        let a = Snapshot::now();
        let mut b = Snapshot::now();
        b.taken_at_monotonic = a.taken_at_monotonic;

        // Zero is rejected alongside negative — it would be a division by
        // zero in every rate calculation downstream.
        assert_eq!(b.elapsed_since(&a), None);
    }
}
