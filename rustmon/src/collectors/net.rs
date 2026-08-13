//! Network interfaces.
//!
//! Sources:
//! - `proc/net/dev` — RX/TX byte, packet, error and drop counters per
//!   interface.
//! - `sys/class/net/<iface>/operstate` and `/mtu` — link state and MTU.
//!
//! Counter width is the trap here. On 32-bit kernels (and for some drivers even
//! on 64-bit) the byte counters are 32-bit and wrap roughly every 4 GiB — about
//! 34 seconds on a saturated 1 Gb/s link. [`crate::delta::counter_delta`]
//! returns `None` on a backwards step rather than guessing a wrap width;
//! guessing wrong yields a plausible wrong number, and in a monitoring tool a
//! visibly absent value beats a subtly wrong one.
//!
//! Interfaces are matched by name across snapshots, never by index — a
//! hot-plugged USB NIC or a container veth appearing mid-session would
//! otherwise attribute one device's counters to another.
//!
//! Not collected, deliberately: MAC addresses, IP addresses, and anything else
//! that identifies the machine or its network. Security model item 9 — the JSON
//! output is a fixed documented schema and nothing identifying belongs in it.

use std::path::{Path, PathBuf};

use crate::collector::Collector;
use crate::error::{Error, Result};
use crate::sample::{NetInterface, NetSample, Snapshot};
use crate::sysfs::{is_safe_component, SysfsReader, DEFAULT_MAX_LINES};

pub const NAME: &str = "net";

const PROC_NET_DEV: &str = "proc/net/dev";
const SYS_CLASS_NET: &str = "sys/class/net";

/// Fields read per data line, in the order `/proc/net/dev` reports them:
/// 8 receive counters (bytes, packets, errs, drop, fifo, frame, compressed,
/// multicast) then 8 transmit counters (bytes, packets, errs, drop, fifo,
/// colls, carrier, compressed).
const NET_DEV_FIELDS: usize = 16;

/// Interface-name prefixes hidden by default. `lo` in particular reports
/// loopback traffic that no user means when they ask about network throughput.
pub const HIDDEN_PREFIXES: &[&str] = &["lo"];

#[derive(Debug, Default)]
pub struct NetCollector {
    /// Whether to include interfaces matching [`HIDDEN_PREFIXES`].
    show_all: bool,
}

impl NetCollector {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn show_all(mut self, show_all: bool) -> Self {
        self.show_all = show_all;
        self
    }
}

impl Collector for NetCollector {
    fn name(&self) -> &'static str {
        NAME
    }

    fn probe(&self, reader: &SysfsReader) -> bool {
        reader.exists(Path::new(PROC_NET_DEV))
    }

    fn collect(&mut self, reader: &SysfsReader, snapshot: &mut Snapshot) -> Result<()> {
        let interfaces = match reader.read_to_string(Path::new(PROC_NET_DEV))? {
            Ok(contents) => parse_net_dev(Path::new(PROC_NET_DEV), &contents)?,
            Err(_) => Vec::new(),
        };

        let mut interfaces: Vec<NetInterface> = interfaces
            .into_iter()
            .filter(|iface| is_visible(&iface.name, self.show_all))
            .collect();

        for iface in &mut interfaces {
            let (operstate, mtu) = read_link_state(reader, &iface.name)?;
            iface.operstate = operstate;
            iface.mtu = mtu;
        }

        snapshot.net = build_sample(interfaces);
        Ok(())
    }
}

/// Parse `/proc/net/dev`.
///
/// The first two lines are headers and must be skipped. Data lines are
/// `iface: <8 rx fields> <8 tx fields>`, and the name is separated by a colon
/// that may have **no space before it** when the name is long enough to fill
/// the column — splitting on whitespace alone silently merges the name with the
/// first counter. Split on the first `:` instead.
pub fn parse_net_dev(path: &Path, contents: &str) -> Result<Vec<NetInterface>> {
    let mut interfaces = Vec::new();

    // `.enumerate()` before `.skip(2)` so `line_no` in error messages is the
    // true 1-based line number, including the two skipped header lines.
    for (idx, line) in contents.lines().enumerate().skip(2).take(DEFAULT_MAX_LINES) {
        if let Some(iface) = parse_net_dev_line(path, idx + 1, line)? {
            interfaces.push(iface);
        }
    }

    Ok(interfaces)
}

/// One `/proc/net/dev` data line.
pub fn parse_net_dev_line(path: &Path, line_no: usize, line: &str) -> Result<Option<NetInterface>> {
    if line.trim().is_empty() {
        return Ok(None);
    }

    // Not whitespace: a sufficiently long interface name runs straight into
    // the column with no space before the colon (`enp13s0:11667885 ...` is
    // possible in principle even if this machine's names happen to have one).
    // Splitting on whitespace first would merge the name with the first
    // counter on a line like that.
    let (name, rest) = line.split_once(':').ok_or_else(|| {
        Error::parse(path, Some(line_no), "line has no ':' separating name from counters")
    })?;
    let name = name.trim();

    if !is_safe_component(name) {
        return Err(Error::parse(
            path,
            Some(line_no),
            format!(
                "unsafe interface name {:?}",
                crate::sysfs::sanitize_kernel_string(name, 32)
            ),
        ));
    }

    let mut counters = [0u64; NET_DEV_FIELDS];
    let mut seen = 0usize;

    for (idx, (slot, field)) in counters.iter_mut().zip(rest.split_whitespace()).enumerate() {
        *slot = field.parse::<u64>().map_err(|_| {
            Error::parse(
                path,
                Some(line_no),
                format!("counter field {} is not an unsigned integer: {:?}", idx + 1, field),
            )
        })?;
        seen = idx + 1;
    }

    if seen < NET_DEV_FIELDS {
        return Err(Error::parse(
            path,
            Some(line_no),
            format!("expected {NET_DEV_FIELDS} counter fields, got {seen}"),
        ));
    }

    Ok(Some(NetInterface {
        name: name.to_string(),
        rx_bytes: counters[0],
        rx_packets: counters[1],
        rx_errors: counters[2],
        rx_dropped: counters[3],
        // counters[4..8]: rx fifo, frame, compressed, multicast — not tracked.
        tx_bytes: counters[8],
        tx_packets: counters[9],
        tx_errors: counters[10],
        tx_dropped: counters[11],
        // counters[12..16]: tx fifo, colls, carrier, compressed — not tracked.
        operstate: None,
        mtu: None,
    }))
}

/// Read `operstate` and `mtu` for one interface.
///
/// Both are optional, and both degrade to `None` on *any* failure — absence
/// (`/sys` masked, interface gone since it was listed) and a garbled value
/// are treated identically. Link state and MTU are decoration on top of the
/// counters this collector actually exists for; losing them for one
/// interface must not cost the caller anything else. `iface` is safe to
/// interpolate into a path unescaped because every caller has already run it
/// through [`is_safe_component`] in [`parse_net_dev_line`].
pub fn read_link_state(reader: &SysfsReader, iface: &str) -> Result<(Option<String>, Option<u64>)> {
    let dir = PathBuf::from(format!("{SYS_CLASS_NET}/{iface}"));

    // Not sanitised: unlike a hwmon chip's `name`/`label` (vendor-supplied
    // free text), `operstate` only ever holds one of a handful of fixed
    // kernel-defined words ("up", "down", "unknown", "dormant", ...) — there
    // is no free-text path for a device to inject anything into this file.
    let operstate = match reader.read_first_line(&dir.join("operstate")) {
        Ok(Ok(state)) => Some(state),
        _ => None,
    };

    let mtu = match reader.read_u64(&dir.join("mtu")) {
        Ok(Ok(value)) => Some(value),
        _ => None,
    };

    Ok((operstate, mtu))
}

/// Should this interface be shown, given `show_all`?
pub fn is_visible(name: &str, show_all: bool) -> bool {
    show_all || !HIDDEN_PREFIXES.iter().any(|prefix| name.starts_with(prefix))
}

/// Assemble the sample; `None` if no interfaces are visible.
pub fn build_sample(interfaces: Vec<NetInterface>) -> Option<NetSample> {
    if interfaces.is_empty() {
        None
    } else {
        Some(NetSample { interfaces })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p() -> &'static Path {
        Path::new("proc/net/dev")
    }

    /// Captured from this machine on 2026-08-07: the standard two header
    /// lines, a loopback interface, an ethernet interface, an interface with
    /// zero traffic, and a hyphenated name (`wg0-mullvad`, a WireGuard
    /// interface) — hyphens are valid in real interface names and must not
    /// be rejected by `is_safe_component`.
    const REAL_NET_DEV: &str = "\
Inter-|   Receive                                                |  Transmit
 face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed
    lo:   64142     823    0    0    0     0          0         0    64142     823    0    0    0     0       0          0
enp13s0: 11667885   17405    0    0    0     0          0        12 17816421   17864    0    0    0     0       0          0
wlp14s0:       0       0    0    0    0     0          0         0        0       0    0    0    0     0       0          0
wg0-mullvad: 10655240   17153    0    0    0     0          0         0 17062264   17780    0    0    0     0       0          0
";

    #[test]
    fn parses_real_net_dev_including_a_hyphenated_name() {
        let interfaces = parse_net_dev(p(), REAL_NET_DEV).expect("real fixture parses");
        let names: Vec<&str> = interfaces.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, vec!["lo", "enp13s0", "wlp14s0", "wg0-mullvad"]);

        let wg = interfaces.iter().find(|i| i.name == "wg0-mullvad").expect("present");
        assert_eq!(wg.rx_bytes, 10_655_240);
        assert_eq!(wg.tx_bytes, 17_062_264);
        assert_eq!(wg.rx_packets, 17_153);
        assert_eq!(wg.tx_packets, 17_780);
    }

    /// The trap the module doc calls out: a long name runs straight into the
    /// colon with no space, which would merge into the first counter if this
    /// split on whitespace instead of on `:`.
    #[test]
    fn a_name_with_no_space_before_the_colon_is_not_merged_with_its_counters() {
        let line = "\
Inter-|   Receive                                                |  Transmit
 face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed
averyveryverylonginterfacename0:123 456 0 0 0 0 0 0 789 12 0 0 0 0 0 0
";
        let interfaces = parse_net_dev(p(), line).expect("parses");
        assert_eq!(interfaces.len(), 1);
        assert_eq!(interfaces[0].name, "averyveryverylonginterfacename0");
        assert_eq!(interfaces[0].rx_bytes, 123);
    }

    #[test]
    fn a_truncated_line_is_a_parse_error_not_a_panic() {
        let contents = "h1\nh2\neth0: 123 456\n";
        let err = parse_net_dev(p(), contents).expect_err("too few counters must fail");
        assert!(matches!(err, Error::Parse { line: Some(3), .. }), "got {err:?}");
    }

    #[test]
    fn a_non_numeric_counter_is_a_parse_error() {
        let contents = "h1\nh2\neth0: banana 456 0 0 0 0 0 0 789 12 0 0 0 0 0 0\n";
        let err = parse_net_dev(p(), contents).expect_err("garbage counter must fail");
        assert!(matches!(err, Error::Parse { line: Some(3), .. }), "got {err:?}");
    }

    #[test]
    fn a_line_with_no_colon_is_a_parse_error() {
        let contents = "h1\nh2\nno colon here at all\n";
        let err = parse_net_dev(p(), contents).expect_err("no colon must fail");
        assert!(matches!(err, Error::Parse { .. }), "got {err:?}");
    }

    #[test]
    fn an_unsafe_interface_name_is_a_parse_error() {
        let contents = "h1\nh2\n../../etc: 123 456 0 0 0 0 0 0 789 12 0 0 0 0 0 0\n";
        let err = parse_net_dev(p(), contents).expect_err("unsafe name must fail");
        assert!(matches!(err, Error::Parse { .. }), "got {err:?}");
    }

    #[test]
    fn blank_lines_after_the_headers_are_skipped() {
        let contents = "h1\nh2\n\neth0: 123 456 0 0 0 0 0 0 789 12 0 0 0 0 0 0\n\n";
        let interfaces = parse_net_dev(p(), contents).expect("blank lines are fine");
        assert_eq!(interfaces.len(), 1);
    }

    // ---- visibility ---------------------------------------------------------

    #[test]
    fn loopback_is_hidden_by_default_but_not_with_show_all() {
        assert!(!is_visible("lo", false));
        assert!(is_visible("lo", true));
        assert!(is_visible("eth0", false));
    }

    #[test]
    fn build_sample_is_none_when_nothing_is_visible() {
        assert!(build_sample(Vec::new()).is_none());
    }

    // ---- collector-to-delta integration --------------------------------------

    /// The scenario the module doc calls the reason interfaces are matched by
    /// name: a hot-plugged interface (a USB NIC, a container veth) appearing
    /// mid-session must not corrupt rate tracking for the interfaces that
    /// were already there. This proves `parse_net_dev`'s *output* — not just
    /// `delta`'s matching logic in isolation, which chunk 4 already covers —
    /// is what actually flows correctly through `RateTracker`.
    #[test]
    fn a_hotplugged_interface_does_not_corrupt_existing_rates() {
        use crate::delta::RateTracker;
        use crate::sample::Snapshot;
        use std::time::Duration;

        let before = "\
h1\nh2\neth0: 1000 10 0 0 0 0 0 0 2000 20 0 0 0 0 0 0\n";
        let after = "\
h1\nh2\neth0: 3000 30 0 0 0 0 0 0 6000 60 0 0 0 0 0 0\nusb0: 500 5 0 0 0 0 0 0 700 7 0 0 0 0 0 0\n";

        let mut first = Snapshot::now();
        first.net = build_sample(parse_net_dev(p(), before).expect("parses"));

        let mut second = Snapshot::now();
        second.taken_at_monotonic = first.taken_at_monotonic + Duration::from_secs(2);
        second.net = build_sample(parse_net_dev(p(), after).expect("parses"));

        let mut tracker = RateTracker::new();
        assert!(tracker.update(&first).is_none());
        let rates = tracker.update(&second).expect("valid interval");

        // eth0 existed in both snapshots: its rate reflects its own delta,
        // not usb0's counters or a corrupted mix of the two.
        let eth0 = &rates.net["eth0"];
        assert!((eth0.rx.as_f64() - 1_000.0).abs() < 1e-6, "{:?}", eth0.rx);
        assert!((eth0.tx.as_f64() - 2_000.0).abs() < 1e-6, "{:?}", eth0.tx);

        // usb0 is new this refresh — no prior reading, so no rate yet, and
        // critically it must not have been paired with eth0's old counters.
        assert!(!rates.net.contains_key("usb0"));
    }
}
