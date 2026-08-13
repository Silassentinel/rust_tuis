//! Counters to rates.
//!
//! Kept out of the collectors on purpose (see [`crate::sample`]'s module doc):
//! collectors stay pure "read and parse" and testable against a static fixture
//! tree, and all the interval arithmetic — which is where the bugs are — lives
//! in one place with its own tests.
//!
//! The three cases that must be handled, and which chunk 4's tests target
//! directly, because each one silently produces nonsense otherwise:
//!
//! 1. **Counter went backwards.** A NIC being reset, a device being
//!    hot-unplugged and re-plugged, or a 32-bit counter wrapping. `curr - prev`
//!    underflows and you render an absurd spike. Correct behaviour is to drop
//!    the interval and show nothing for one refresh.
//! 2. **Non-positive elapsed time.** Two samples in the same monotonic instant,
//!    or a clock that stepped. Division by zero, or a negative rate. Correct
//!    behaviour is again to drop the interval. This is why [`Snapshot`] carries
//!    an `Instant` alongside its `SystemTime`.
//! 3. **A device that appeared or disappeared mid-session.** Matching by index
//!    would attribute one device's counters to another. Everything here matches
//!    by name.

use std::collections::HashMap;

use crate::sample::{CpuTimes, DiskDevice, DiskSample, NetInterface, NetSample, Snapshot};
use crate::units::{BytesPerSec, Percent};

/// Holds the previous snapshot so the next one can be turned into rates.
#[derive(Debug, Default)]
pub struct RateTracker {
    previous: Option<Snapshot>,
}

impl RateTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed in a new snapshot, get back the rates since the previous one.
    ///
    /// Returns `None` on the very first call (nothing to compare against) and
    /// whenever the interval is unusable per the rules above. The caller shows
    /// "—" for that refresh; it does not treat it as an error.
    pub fn update(&mut self, current: &Snapshot) -> Option<Rates> {
        let rates = self
            .previous
            .as_ref()
            .and_then(|prev| build_rates(prev, current));

        // Stored unconditionally, even when `rates` came back `None`: the
        // *next* refresh should compare against this reading, not silently
        // skip past it and compare against whatever came before it.
        self.previous = Some(current.clone());

        rates
    }

    /// Drop the stored snapshot — used when the user resets the view, so the
    /// next refresh doesn't average across the gap.
    pub fn reset(&mut self) {
        self.previous = None;
    }
}

/// Build every rate that can be computed between two snapshots.
///
/// `None` only for the whole-snapshot reasons (non-positive interval).
/// Anything narrower — one interface's counters reset, one core coming
/// online mid-session — is handled per-item: that item is missing or `None`
/// from the result, the rest of `Rates` is still populated.
fn build_rates(prev: &Snapshot, curr: &Snapshot) -> Option<Rates> {
    let interval_secs = curr.elapsed_since(prev)?;

    let mut rates = Rates {
        interval_secs,
        ..Rates::default()
    };

    if let (Some(p), Some(c)) = (&prev.cpu, &curr.cpu) {
        rates.cpu_total = cpu_busy_percent(&p.total, &c.total);

        // Matched by index, not name — a CPU core's identity *is* its index
        // in `/proc/stat`. A core that just came online has no entry at that
        // index in `prev`, so `get` yields `None` for it rather than pairing
        // it with the wrong core's counters.
        rates.cpu_per_core = c
            .per_core
            .iter()
            .enumerate()
            .map(|(i, curr_times)| {
                p.per_core
                    .get(i)
                    .and_then(|prev_times| cpu_busy_percent(prev_times, curr_times))
            })
            .collect();
    }

    if let (Some(p), Some(c)) = (&prev.disks, &curr.disks) {
        rates.disk = disk_rates(p, c, interval_secs);
    }

    if let (Some(p), Some(c)) = (&prev.net, &curr.net) {
        rates.net = net_rates(p, c, interval_secs);
    }

    Some(rates)
}

/// Match disk devices by name and compute a rate for each pair.
///
/// A device only in `prev` (unplugged since) or only in `curr` (plugged in
/// since) simply has no entry in the result — matching by name is what makes
/// that safe, rather than pairing devices up by their position in each
/// listing. If *any* of a device's counters went backwards, the whole device
/// is skipped for this refresh rather than mixing valid and invalid figures
/// into one row.
fn disk_rates(prev: &DiskSample, curr: &DiskSample, interval_secs: f64) -> HashMap<String, DiskRate> {
    let prev_by_name: HashMap<&str, &DiskDevice> =
        prev.devices.iter().map(|d| (d.name.as_str(), d)).collect();

    let mut out = HashMap::new();

    for c in &curr.devices {
        let Some(p) = prev_by_name.get(c.name.as_str()) else {
            continue;
        };

        let sectors_read = counter_delta(p.sectors_read, c.sectors_read);
        let sectors_written = counter_delta(p.sectors_written, c.sectors_written);
        let reads = counter_delta(p.reads_completed, c.reads_completed);
        let writes = counter_delta(p.writes_completed, c.writes_completed);

        let (Some(sectors_read), Some(sectors_written), Some(reads), Some(writes)) =
            (sectors_read, sectors_written, reads, writes)
        else {
            continue;
        };

        // `io_ticks_ms` is kept separate from the four counters above: some
        // drivers don't advance it, and losing utilisation must not cost the
        // caller the throughput and IOPS figures it came for. That is why
        // `DiskRate::utilisation` is an `Option` while the others are not.
        let utilisation = counter_delta(p.io_ticks_ms, c.io_ticks_ms).map(|delta_ms| {
            Percent::new(delta_ms as f64 / (interval_secs * 1000.0) * 100.0)
        });

        out.insert(
            c.name.clone(),
            DiskRate {
                // Sectors are always 512 bytes per the kernel's fixed
                // convention (`units.rs`'s reason for existing). The
                // conversion happens here as a plain multiply, not through
                // `Bytes::from_sectors_512`: that constructor is for an
                // absolute byte count and its overflow check has no
                // meaning for a per-second `f64` rate.
                read: BytesPerSec::new(sectors_read as f64 * 512.0 / interval_secs),
                write: BytesPerSec::new(sectors_written as f64 * 512.0 / interval_secs),
                read_iops: reads as f64 / interval_secs,
                write_iops: writes as f64 / interval_secs,
                utilisation,
            },
        );
    }

    out
}

/// Match network interfaces by name and compute a rate for each pair. Same
/// per-device drop rule as [`disk_rates`]; see there for why.
fn net_rates(prev: &NetSample, curr: &NetSample, interval_secs: f64) -> HashMap<String, NetRate> {
    let prev_by_name: HashMap<&str, &NetInterface> = prev
        .interfaces
        .iter()
        .map(|i| (i.name.as_str(), i))
        .collect();

    let mut out = HashMap::new();

    for c in &curr.interfaces {
        let Some(p) = prev_by_name.get(c.name.as_str()) else {
            continue;
        };

        let rx_bytes = counter_delta(p.rx_bytes, c.rx_bytes);
        let tx_bytes = counter_delta(p.tx_bytes, c.tx_bytes);
        let rx_packets = counter_delta(p.rx_packets, c.rx_packets);
        let tx_packets = counter_delta(p.tx_packets, c.tx_packets);

        // This is where a 32-bit `/proc/net/dev` wrap actually lands: on a
        // 32-bit-counter kernel, `rx_bytes` can wrap from just under 2^32
        // back to a small number. `counter_delta` sees `curr < prev` and
        // returns `None` — same as a NIC reset — and this interface is
        // dropped for one refresh rather than rendering a multi-exabyte
        // spike. There is deliberately no wrap-width correction; see the
        // module doc.
        let (Some(rx_bytes), Some(tx_bytes), Some(rx_packets), Some(tx_packets)) =
            (rx_bytes, tx_bytes, rx_packets, tx_packets)
        else {
            continue;
        };

        out.insert(
            c.name.clone(),
            NetRate {
                rx: BytesPerSec::new(rx_bytes as f64 / interval_secs),
                tx: BytesPerSec::new(tx_bytes as f64 / interval_secs),
                rx_packets_per_sec: rx_packets as f64 / interval_secs,
                tx_packets_per_sec: tx_packets as f64 / interval_secs,
            },
        );
    }

    out
}

/// Everything derived from a pair of snapshots.
#[derive(Debug, Clone, Default)]
pub struct Rates {
    /// Seconds between the two snapshots, monotonic.
    pub interval_secs: f64,
    /// Aggregate CPU busy percentage.
    pub cpu_total: Option<Percent>,
    /// Per-core busy percentage, index = core number.
    pub cpu_per_core: Vec<Option<Percent>>,
    /// Keyed by device name.
    pub disk: HashMap<String, DiskRate>,
    /// Keyed by interface name.
    pub net: HashMap<String, NetRate>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct DiskRate {
    pub read: BytesPerSec,
    pub write: BytesPerSec,
    pub read_iops: f64,
    pub write_iops: f64,
    /// From `io_ticks_ms`: fraction of the interval the device was busy.
    pub utilisation: Option<Percent>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct NetRate {
    pub rx: BytesPerSec,
    pub tx: BytesPerSec,
    pub rx_packets_per_sec: f64,
    pub tx_packets_per_sec: f64,
}

/// Subtract two counter readings.
///
/// `None` when `curr < prev` — the counter reset or wrapped. Deliberately does
/// **not** try to guess a wrap width and correct for it: guessing wrong
/// produces a plausible-looking wrong number, whereas returning `None` produces
/// a visibly absent one. Absent beats subtly wrong in a monitoring tool.
pub fn counter_delta(prev: u64, curr: u64) -> Option<u64> {
    curr.checked_sub(prev)
}

/// A per-second rate from a counter delta over an interval.
///
/// `None` if the delta is `None` or `interval_secs` is not finite and positive.
pub fn per_second(delta: Option<u64>, interval_secs: f64) -> Option<f64> {
    let delta = delta?;

    if !interval_secs.is_finite() || interval_secs <= 0.0 {
        return None;
    }

    Some(delta as f64 / interval_secs)
}

/// CPU busy percentage between two jiffy readings.
///
/// `None` if the total jiffies didn't advance — which happens on a very short
/// interval and must not become a division by zero — or if either counter
/// went backwards (a reset).
pub fn cpu_busy_percent(prev: &CpuTimes, curr: &CpuTimes) -> Option<Percent> {
    let total_delta = counter_delta(prev.total(), curr.total())?;

    // `busy()` is derived from the same fields as `total()`, which just
    // proved non-decreasing above, so `busy` should be non-decreasing too.
    // `saturating_sub` rather than `checked_sub` here: if a single field's
    // rounding made that not quite hold, floor at zero busy time instead of
    // failing the whole reading.
    let busy_delta = curr.busy().saturating_sub(prev.busy());

    // Guards `total_delta == 0` itself (a sub-tick interval) by returning
    // `None` rather than dividing by zero, and clamps the result the same
    // way every other jiffy-rounding case in this crate does.
    Percent::from_ratio(busy_delta, total_delta)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sample::NetInterface;
    use std::time::Duration;

    // ---- counter_delta ------------------------------------------------------

    #[test]
    fn counter_delta_forward_and_flat() {
        assert_eq!(counter_delta(10, 15), Some(5));
        assert_eq!(counter_delta(15, 15), Some(0));
    }

    /// The case bullet 1 of the module doc describes: a device reset. Must
    /// drop the interval, never underflow.
    #[test]
    fn counter_delta_drops_the_interval_when_the_counter_goes_backwards() {
        assert_eq!(counter_delta(15, 10), None);
        // The pathological end of it: not just "smaller", but a counter that
        // reset all the way to zero.
        assert_eq!(counter_delta(u64::MAX, 0), None);
    }

    /// The specific failure mode named in the module doc: a 32-bit counter on
    /// `/proc/net/dev` wraps from just under 2^32 back to a small number.
    /// Correct behaviour is to treat that exactly like a reset — drop the
    /// interval — and deliberately *not* try to reconstruct the true delta by
    /// guessing a wrap width.
    #[test]
    fn counter_delta_does_not_correct_a_32_bit_wrap() {
        let prev = u64::from(u32::MAX) - 5; // 4294967290
        let curr = 100u64; // wrapped back around near zero
        assert_eq!(counter_delta(prev, curr), None);
    }

    // ---- per_second ----------------------------------------------------------

    #[test]
    fn per_second_basic_rate() {
        assert_eq!(per_second(Some(1_000), 2.0), Some(500.0));
        assert_eq!(per_second(Some(0), 5.0), Some(0.0));
    }

    #[test]
    fn per_second_propagates_a_missing_delta() {
        assert_eq!(per_second(None, 2.0), None);
    }

    /// Bullet 2 of the module doc: a zero or negative interval — two samples
    /// in the same monotonic instant, or a stepped clock — must not become a
    /// division by zero or a negative rate.
    #[test]
    fn per_second_rejects_non_positive_or_non_finite_intervals() {
        assert_eq!(per_second(Some(100), 0.0), None);
        assert_eq!(per_second(Some(100), -1.0), None);
        assert_eq!(per_second(Some(100), f64::NAN), None);
        assert_eq!(per_second(Some(100), f64::INFINITY), None);
    }

    // ---- cpu_busy_percent -----------------------------------------------------

    fn times(user: u64, idle: u64) -> CpuTimes {
        CpuTimes {
            user,
            idle,
            ..CpuTimes::default()
        }
    }

    #[test]
    fn cpu_busy_percent_basic() {
        let prev = times(100, 900); // total 1000, busy 100
        let curr = times(200, 1_800); // total 2000, busy 200
        let pct = cpu_busy_percent(&prev, &curr).expect("valid delta").as_f64();
        assert!((pct - 10.0).abs() < 1e-9, "got {pct}");
    }

    #[test]
    fn cpu_busy_percent_none_on_a_sub_tick_interval() {
        let t = times(100, 900);
        assert_eq!(cpu_busy_percent(&t, &t), None);
    }

    #[test]
    fn cpu_busy_percent_none_when_the_counters_reset() {
        let prev = times(200, 1_800);
        let curr = times(100, 900); // reset backwards
        assert_eq!(cpu_busy_percent(&prev, &curr), None);
    }

    // ---- RateTracker / build_rates ---------------------------------------------

    fn disk(name: &str, sectors_read: u64, sectors_written: u64, reads: u64, writes: u64, ticks_ms: u64) -> DiskDevice {
        DiskDevice {
            name: name.to_string(),
            reads_completed: reads,
            writes_completed: writes,
            sectors_read,
            sectors_written,
            io_ticks_ms: ticks_ms,
        }
    }

    fn net_if(name: &str, rx_bytes: u64, tx_bytes: u64, rx_packets: u64, tx_packets: u64) -> NetInterface {
        NetInterface {
            name: name.to_string(),
            rx_bytes,
            tx_bytes,
            rx_packets,
            tx_packets,
            rx_errors: 0,
            tx_errors: 0,
            rx_dropped: 0,
            tx_dropped: 0,
            operstate: None,
            mtu: None,
        }
    }

    /// Build a snapshot `offset` after `base`'s monotonic instant.
    /// `Instant` has no public constructor from an arbitrary value, so tests
    /// derive one relative to a real `Instant::now()` instead — the same
    /// pattern `sample.rs`'s own tests use.
    fn at(base: &Snapshot, offset: Duration) -> Snapshot {
        let mut s = Snapshot::now();
        s.taken_at_monotonic = base.taken_at_monotonic + offset;
        s
    }

    #[test]
    fn the_first_update_has_nothing_to_compare_against() {
        let mut tracker = RateTracker::new();
        assert!(tracker.update(&Snapshot::now()).is_none());
    }

    #[test]
    fn reset_forgets_the_previous_snapshot() {
        let mut tracker = RateTracker::new();
        let first = Snapshot::now();
        let second = at(&first, Duration::from_secs(1));

        assert!(tracker.update(&first).is_none());
        assert!(tracker.update(&second).is_some(), "second call has a prior reading");

        tracker.reset();

        let third = at(&second, Duration::from_secs(1));
        assert!(
            tracker.update(&third).is_none(),
            "post-reset update must behave like a first call"
        );
    }

    /// Bullet 2 at the `RateTracker` level: two snapshots at the same
    /// monotonic instant (or an inverted pair) must yield no rate at all —
    /// not just no per-device rates.
    #[test]
    fn a_zero_or_negative_interval_yields_no_rates_at_the_whole_snapshot_level() {
        let mut tracker = RateTracker::new();
        let first = Snapshot::now();
        let same_instant = at(&first, Duration::ZERO);

        assert!(tracker.update(&first).is_none());
        assert!(
            tracker.update(&same_instant).is_none(),
            "zero elapsed time"
        );
    }

    #[test]
    fn disk_and_net_rates_match_by_name_over_a_real_interval() {
        let mut tracker = RateTracker::new();

        let mut first = Snapshot::now();
        first.disks = Some(DiskSample {
            devices: vec![disk("sda", 1_000, 2_000, 100, 50, 400)],
            mounts: vec![],
        });
        first.net = Some(NetSample {
            interfaces: vec![net_if("eth0", 10_000, 5_000, 100, 50)],
        });

        let mut second = at(&first, Duration::from_secs(2));
        second.disks = Some(DiskSample {
            devices: vec![disk("sda", 3_000, 2_400, 300, 90, 1_400)],
            mounts: vec![],
        });
        second.net = Some(NetSample {
            interfaces: vec![net_if("eth0", 30_000, 9_000, 300, 90)],
        });

        assert!(tracker.update(&first).is_none());
        let rates = tracker.update(&second).expect("valid interval");

        assert!((rates.interval_secs - 2.0).abs() < 1e-9);

        let d = &rates.disk["sda"];
        // (3000 - 1000) sectors * 512 B / 2 s = 512,000 B/s.
        assert!((d.read.as_f64() - 512_000.0).abs() < 1e-6, "{:?}", d.read);
        assert!((d.write.as_f64() - 102_400.0).abs() < 1e-6, "{:?}", d.write);
        assert!((d.read_iops - 100.0).abs() < 1e-9);
        assert!((d.write_iops - 20.0).abs() < 1e-9);
        // (1400 - 400) ms busy / 2000 ms interval = 50%.
        let util = d.utilisation.expect("ticks advanced").as_f64();
        assert!((util - 50.0).abs() < 1e-6, "got {util}");

        let n = &rates.net["eth0"];
        assert!((n.rx.as_f64() - 10_000.0).abs() < 1e-6);
        assert!((n.tx.as_f64() - 2_000.0).abs() < 1e-6);
        assert!((n.rx_packets_per_sec - 100.0).abs() < 1e-9);
        assert!((n.tx_packets_per_sec - 20.0).abs() < 1e-9);
    }

    /// Bullet 3 of the module doc: matching must be by name, never by
    /// position — a device appearing or disappearing between two snapshots
    /// must not shift every later device onto the wrong row.
    #[test]
    fn devices_are_matched_by_name_not_by_position() {
        let mut tracker = RateTracker::new();

        let mut first = Snapshot::now();
        first.disks = Some(DiskSample {
            devices: vec![disk("sda", 0, 0, 0, 0, 0), disk("sdb", 0, 0, 0, 0, 0)],
            mounts: vec![],
        });

        // A device shows up between "sda" and "sdb" in listing order, and
        // "sda" moves to the end. If matching were positional, "sdb"'s old
        // counters would get paired with the new device's, and "sda" would
        // get paired with "sdb"'s.
        let mut second = at(&first, Duration::from_secs(1));
        second.disks = Some(DiskSample {
            devices: vec![
                disk("sdc", 5_000, 0, 0, 0, 0),
                disk("sdb", 1_024, 0, 0, 0, 0),
                disk("sda", 512, 0, 0, 0, 0),
            ],
            mounts: vec![],
        });

        assert!(tracker.update(&first).is_none());
        let rates = tracker.update(&second).expect("valid interval");

        // "sdc" is new this refresh — no previous reading, so no rate.
        assert!(!rates.disk.contains_key("sdc"));

        // sda: 512 sectors * 512 B / 1 s. sdb: 1024 sectors * 512 B / 1 s.
        // Wrong matching (by position) would swap these two.
        assert!(
            (rates.disk["sda"].read.as_f64() - 262_144.0).abs() < 1e-6,
            "{:?}",
            rates.disk["sda"]
        );
        assert!(
            (rates.disk["sdb"].read.as_f64() - 524_288.0).abs() < 1e-6,
            "{:?}",
            rates.disk["sdb"]
        );
    }

    /// The `RateTracker`-level counterpart of `counter_delta`'s wrap test: a
    /// NIC that wraps its 32-bit byte counter must disappear from the map for
    /// one refresh, not report an exabyte spike, while *other* interfaces on
    /// the same snapshot are unaffected.
    #[test]
    fn a_wrapped_interface_is_dropped_for_one_refresh_others_are_not() {
        let mut tracker = RateTracker::new();

        let mut first = Snapshot::now();
        first.net = Some(NetSample {
            interfaces: vec![
                net_if("eth0", u64::from(u32::MAX) - 5, 0, 0, 0),
                net_if("lo", 1_000, 1_000, 10, 10),
            ],
        });

        let mut second = at(&first, Duration::from_secs(1));
        second.net = Some(NetSample {
            interfaces: vec![
                net_if("eth0", 100, 0, 0, 0), // wrapped
                net_if("lo", 2_000, 2_000, 20, 20),
            ],
        });

        assert!(tracker.update(&first).is_none());
        let rates = tracker.update(&second).expect("valid interval");

        assert!(
            !rates.net.contains_key("eth0"),
            "a wrapped counter must not produce a rate"
        );
        assert!(rates.net.contains_key("lo"), "an unaffected interface must still report");
    }

    /// A device present in `prev` but not `curr` (unplugged) must simply be
    /// absent from the result, not panic or leave a stale entry behind.
    #[test]
    fn a_device_removed_since_the_previous_snapshot_has_no_entry() {
        let mut tracker = RateTracker::new();

        let mut first = Snapshot::now();
        first.disks = Some(DiskSample {
            devices: vec![disk("sda", 0, 0, 0, 0, 0), disk("sdb", 0, 0, 0, 0, 0)],
            mounts: vec![],
        });

        let mut second = at(&first, Duration::from_secs(1));
        second.disks = Some(DiskSample {
            devices: vec![disk("sda", 512, 0, 0, 0, 0)], // sdb unplugged
            mounts: vec![],
        });

        assert!(tracker.update(&first).is_none());
        let rates = tracker.update(&second).expect("valid interval");

        assert!(rates.disk.contains_key("sda"));
        assert!(!rates.disk.contains_key("sdb"));
        assert_eq!(rates.disk.len(), 1);
    }

    /// A core that comes online mid-session (index present in `curr` but not
    /// `prev`) must read `None`, not be paired with the wrong core's jiffies.
    #[test]
    fn a_core_that_comes_online_mid_session_has_no_rate_yet() {
        let mut tracker = RateTracker::new();

        let mut first = Snapshot::now();
        first.cpu = Some(crate::sample::CpuSample {
            total: times(100, 900),
            per_core: vec![times(100, 900)],
            freq_khz: vec![],
            load_avg: None,
            model: None,
            ctxt: None,
            btime: None,
        });

        let mut second = at(&first, Duration::from_secs(1));
        second.cpu = Some(crate::sample::CpuSample {
            total: times(200, 1_800),
            per_core: vec![times(200, 1_800), times(50, 50)], // core 1 just appeared
            freq_khz: vec![],
            load_avg: None,
            model: None,
            ctxt: None,
            btime: None,
        });

        assert!(tracker.update(&first).is_none());
        let rates = tracker.update(&second).expect("valid interval");

        assert_eq!(rates.cpu_per_core.len(), 2);
        assert!(rates.cpu_per_core[0].is_some(), "core 0 has a prior reading");
        assert!(
            rates.cpu_per_core[1].is_none(),
            "core 1 has no prior reading and must not borrow core 0's"
        );
    }
}
