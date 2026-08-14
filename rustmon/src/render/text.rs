//! Plain-text one-shot renderer.
//!
//! Aligned columns, no colour, no escape sequences at all. Written to be safe
//! to pipe into a file or a pager: if stdout isn't a tty, nothing here changes,
//! because nothing here ever emits control characters in the first place.
//!
//! Kernel-supplied strings are already sanitised at the collector boundary
//! (see [`crate::sysfs::sanitize_kernel_string`]), so this module doesn't need
//! to re-do it — but it must not undo it either by, say, reading a raw file
//! itself. It doesn't; it only formats a [`Snapshot`].

use std::io::Write;
use std::path::PathBuf;

use crate::delta::Rates;
use crate::error::{Error, Result};
use crate::sample::{Snapshot, TempSeverity};

/// Write the whole snapshot as aligned text sections.
///
/// Each section is written only if the snapshot actually has it — a machine
/// with no GPU prints no `GPU` section at all, rather than an empty one.
pub fn write(out: &mut dyn Write, snapshot: &Snapshot, rates: Option<&Rates>, verbose: bool) -> Result<()> {
    let timestamp = super::json::format_rfc3339(snapshot.taken_at)?;
    writeln!(out, "rustmon — {timestamp}").map_err(io_err)?;
    writeln!(out).map_err(io_err)?;

    if snapshot.cpu.is_some() {
        write_cpu(out, snapshot, rates)?;
    }
    if snapshot.memory.is_some() {
        write_memory(out, snapshot)?;
    }
    if snapshot.thermal.is_some() {
        write_thermal(out, snapshot)?;
    }
    if snapshot.disks.is_some() {
        write_disk(out, snapshot, rates)?;
    }
    if snapshot.net.is_some() {
        write_net(out, snapshot, rates)?;
    }
    if snapshot.gpus.is_some() {
        write_gpu(out, snapshot)?;
    }
    if snapshot.connections.is_some() {
        write_connections(out, snapshot)?;
    }

    if verbose && !snapshot.errors.is_empty() {
        writeln!(out, "Errors").map_err(io_err)?;
        for e in &snapshot.errors {
            writeln!(out, "  {}: {}", e.collector, e.message).map_err(io_err)?;
        }
        writeln!(out).map_err(io_err)?;
    }

    Ok(())
}

fn write_cpu(out: &mut dyn Write, snapshot: &Snapshot, rates: Option<&Rates>) -> Result<()> {
    // `write()` only calls this when `snapshot.cpu` is `Some` — the
    // `expect` documents that invariant rather than threading an
    // `Option` through every line below.
    let cpu = snapshot.cpu.as_ref().expect("caller checked is_some");

    writeln!(out, "CPU").map_err(io_err)?;
    if let Some(model) = &cpu.model {
        writeln!(out, "  {model}").map_err(io_err)?;
    }
    writeln!(out, "  cores: {}", cpu.per_core.len()).map_err(io_err)?;
    if let Some(load) = cpu.load_avg {
        writeln!(out, "  load avg: {:.2} {:.2} {:.2}", load[0], load[1], load[2]).map_err(io_err)?;
    }
    if let Some(busy) = rates.and_then(|r| r.cpu_total) {
        writeln!(
            out,
            "  busy: {:>5.1}% {}",
            busy.as_f64(),
            ascii_bar(busy.as_f64() / 100.0, 20)
        )
        .map_err(io_err)?;
    }

    for (idx, freq) in cpu.freq_khz.iter().enumerate() {
        let busy = rates.and_then(|r| r.cpu_per_core.get(idx).copied().flatten());
        let busy_str = busy
            .map(|p| format!("{:>5.1}%", p.as_f64()))
            .unwrap_or_else(|| "  n/a".to_string());
        let freq_str = freq
            .map(|f| format!("{:.2} GHz", f.as_ghz()))
            .unwrap_or_else(|| "n/a".to_string());
        writeln!(out, "    {} {busy_str} {freq_str}", pad(&format!("core{idx}"), 7)).map_err(io_err)?;
    }

    writeln!(out).map_err(io_err)?;
    Ok(())
}

fn write_memory(out: &mut dyn Write, snapshot: &Snapshot) -> Result<()> {
    let mem = snapshot.memory.as_ref().expect("caller checked is_some");

    writeln!(out, "Memory").map_err(io_err)?;
    writeln!(out, "  {} {}", pad("total", 10), mem.total).map_err(io_err)?;

    let used_percent = mem.used_percent().map(|p| p.as_f64()).unwrap_or(0.0);
    writeln!(
        out,
        "  {} {} ({:.1}%) {}",
        pad("used", 10),
        mem.used(),
        used_percent,
        ascii_bar(used_percent / 100.0, 20)
    )
    .map_err(io_err)?;

    writeln!(out, "  {} {}", pad("available", 10), mem.available).map_err(io_err)?;

    // A swapless machine (many containers, some desktops) doesn't need an
    // always-zero line cluttering the output.
    if mem.swap_total.as_u64() > 0 {
        writeln!(out, "  {} {} / {}", pad("swap", 10), mem.swap_used(), mem.swap_total).map_err(io_err)?;
    }

    writeln!(out).map_err(io_err)?;
    Ok(())
}

fn write_thermal(out: &mut dyn Write, snapshot: &Snapshot) -> Result<()> {
    let thermal = snapshot.thermal.as_ref().expect("caller checked is_some");

    writeln!(out, "Thermal").map_err(io_err)?;
    for chip in &thermal.chips {
        writeln!(out, "  {}", chip.name).map_err(io_err)?;

        for t in &chip.temps {
            // A plain-ASCII marker, not colour: this renderer never emits
            // escape sequences at all, by design — see the module doc.
            let marker = match t.severity() {
                TempSeverity::Critical => "!!",
                TempSeverity::Warning => "! ",
                TempSeverity::Normal | TempSeverity::Unknown => "  ",
            };
            writeln!(out, "    {} {:>6.1} C {marker}", pad(&t.label, 24), t.value.as_celsius()).map_err(io_err)?;
        }

        for f in &chip.fans {
            writeln!(out, "    {} {} rpm", pad(&f.label, 24), f.rpm.as_u64()).map_err(io_err)?;
        }
    }
    writeln!(out).map_err(io_err)?;
    Ok(())
}

fn write_disk(out: &mut dyn Write, snapshot: &Snapshot, rates: Option<&Rates>) -> Result<()> {
    let disk = snapshot.disks.as_ref().expect("caller checked is_some");

    writeln!(out, "Disk").map_err(io_err)?;
    for d in &disk.devices {
        match rates.and_then(|r| r.disk.get(&d.name)) {
            Some(r) => {
                let util = r
                    .utilisation
                    .map(|p| format!("{:.1}%", p.as_f64()))
                    .unwrap_or_else(|| "n/a".to_string());
                writeln!(
                    out,
                    "  {} read {} write {} util {util}",
                    pad(&d.name, 10),
                    r.read.human(),
                    r.write.human()
                )
                .map_err(io_err)?;
            }
            // No previous snapshot to diff against yet — `--once` always
            // hits this branch, since a single reading has no rate.
            None => writeln!(out, "  {} (no rate yet)", pad(&d.name, 10)).map_err(io_err)?,
        }
    }

    for m in &disk.mounts {
        writeln!(out, "  {} -> {} ({})", m.source, m.mount_point, m.fs_type).map_err(io_err)?;
    }

    writeln!(out).map_err(io_err)?;
    Ok(())
}

fn write_net(out: &mut dyn Write, snapshot: &Snapshot, rates: Option<&Rates>) -> Result<()> {
    let net = snapshot.net.as_ref().expect("caller checked is_some");

    writeln!(out, "Net").map_err(io_err)?;
    for i in &net.interfaces {
        let state = i.operstate.as_deref().unwrap_or("?");
        match rates.and_then(|r| r.net.get(&i.name)) {
            Some(r) => writeln!(
                out,
                "  {} [{state}] rx {} tx {}",
                pad(&i.name, 12),
                r.rx.human(),
                r.tx.human()
            )
            .map_err(io_err)?,
            None => writeln!(out, "  {} [{state}] (no rate yet)", pad(&i.name, 12)).map_err(io_err)?,
        }
    }
    writeln!(out).map_err(io_err)?;
    Ok(())
}

fn write_gpu(out: &mut dyn Write, snapshot: &Snapshot) -> Result<()> {
    let gpu = snapshot.gpus.as_ref().expect("caller checked is_some");

    writeln!(out, "GPU").map_err(io_err)?;
    for g in &gpu.gpus {
        let busy = g
            .busy
            .map(|p| format!("{:.1}%", p.as_f64()))
            .unwrap_or_else(|| "n/a".to_string());
        writeln!(out, "  {} {:?} busy {busy}", g.name, g.vendor).map_err(io_err)?;

        if let (Some(used), Some(total)) = (g.vram_used, g.vram_total) {
            writeln!(out, "    vram {used} / {total}").map_err(io_err)?;
        }
        if let Some(t) = g.temp {
            writeln!(out, "    temp {:.1} C", t.as_celsius()).map_err(io_err)?;
        }
        if let Some(f) = g.freq_khz {
            writeln!(out, "    freq {:.2} GHz", f.as_ghz()).map_err(io_err)?;
        }
    }
    writeln!(out).map_err(io_err)?;
    Ok(())
}

fn write_connections(out: &mut dyn Write, snapshot: &Snapshot) -> Result<()> {
    let connections = snapshot.connections.as_ref().expect("caller checked is_some");

    writeln!(out, "Connections").map_err(io_err)?;
    for c in &connections.connections {
        let protocol = match c.protocol {
            crate::sample::ConnProtocol::Tcp => "tcp",
            crate::sample::ConnProtocol::Udp => "udp",
        };
        let owner = match (&c.program, c.pid) {
            (Some(program), Some(pid)) => format!("{program}[{pid}]"),
            _ => format!("uid {}", c.uid),
        };
        let state = c.state.map(|s| format!(" [{}]", s.as_str())).unwrap_or_default();
        writeln!(
            out,
            "  {owner} {protocol} {}:{} -> {}:{}{state}",
            c.local_addr, c.local_port, c.remote_addr, c.remote_port
        )
        .map_err(io_err)?;
    }
    writeln!(out).map_err(io_err)?;
    Ok(())
}

/// A pure-ASCII bar, e.g. `[######----]`, for percentage columns.
///
/// ASCII rather than Unicode block characters on purpose: this output is meant
/// to survive being piped anywhere, including terminals and logs with no UTF-8
/// support. The TUI uses proper block characters; this doesn't.
pub fn ascii_bar(fraction: f64, width: usize) -> String {
    // NaN before clamp: `f64::clamp` propagates NaN rather than clamping it
    // (the same trap `units::Percent::new` guards against), which would
    // otherwise make `filled` itself NaN and panic on the cast to `usize`.
    let fraction = if fraction.is_nan() { 0.0 } else { fraction.clamp(0.0, 1.0) };

    let filled = ((fraction * width as f64).round() as usize).min(width);
    format!("[{}{}]", "#".repeat(filled), "-".repeat(width - filled))
}

/// Right-pad to `width`, truncating if longer, so columns stay aligned even
/// with an absurdly long device name.
pub fn pad(s: &str, width: usize) -> String {
    // `.chars()`, never byte indexing: a device name can contain multi-byte
    // UTF-8 (a chip's sanitised-but-still-non-ASCII label), and truncating by
    // byte count could split a character in half.
    let truncated: String = s.chars().take(width).collect();
    let len = truncated.chars().count();

    if len < width {
        format!("{truncated}{}", " ".repeat(width - len))
    } else {
        truncated
    }
}

/// Same reasoning as `render::json`'s `io_err`: `out` is an arbitrary
/// [`Write`], not a `sysfs`-confined file, so there's no real path to put in
/// [`Error::Io`] — `"<output>"` says plainly where the failure was.
fn io_err(source: std::io::Error) -> Error {
    Error::Io {
        path: PathBuf::from("<output>"),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sample::{CollectorError, CpuSample, CpuTimes, MemorySample};
    use crate::units::Bytes;

    // ---- ascii_bar --------------------------------------------------------

    #[test]
    fn ascii_bar_fills_proportionally() {
        assert_eq!(ascii_bar(0.0, 10), "[----------]");
        assert_eq!(ascii_bar(1.0, 10), "[##########]");
        assert_eq!(ascii_bar(0.5, 10), "[#####-----]");
    }

    /// Jiffy-counter rounding can produce a busy% fractionally above 100 —
    /// the same case `units::Percent` clamps for. Must not overflow the bar.
    #[test]
    fn ascii_bar_clamps_out_of_range_fractions() {
        assert_eq!(ascii_bar(1.5, 10), "[##########]");
        assert_eq!(ascii_bar(-0.5, 10), "[----------]");
    }

    /// `f64::clamp` propagates NaN instead of clamping it — this is the
    /// specific trap the function's own doc comment calls out, and a NaN
    /// reaching the `usize` cast unguarded would panic.
    #[test]
    fn ascii_bar_treats_nan_as_zero_rather_than_panicking() {
        assert_eq!(ascii_bar(f64::NAN, 10), "[----------]");
    }

    #[test]
    fn ascii_bar_width_zero_does_not_panic() {
        assert_eq!(ascii_bar(0.5, 0), "[]");
    }

    // ---- pad ----------------------------------------------------------------

    #[test]
    fn pad_right_pads_short_strings() {
        assert_eq!(pad("hi", 5), "hi   ");
    }

    #[test]
    fn pad_truncates_long_strings() {
        assert_eq!(pad("toolongforthis", 6), "toolon");
    }

    #[test]
    fn pad_exact_length_is_unchanged() {
        assert_eq!(pad("exact", 5), "exact");
    }

    /// The reason this walks `chars()` and not bytes: a byte-index
    /// truncation could split a multi-byte UTF-8 character in half, the same
    /// bug class chunk 6 found in `unescape_mount_field`.
    #[test]
    fn pad_truncates_on_a_character_boundary_not_a_byte_boundary() {
        // Each "é" is 2 bytes; truncating to 3 *bytes* would split one.
        let result = pad("ééé", 2);
        assert_eq!(result, "éé");
        assert!(result.is_char_boundary(result.len()));
    }

    // ---- write() ------------------------------------------------------------

    fn full_snapshot() -> Snapshot {
        let mut s = Snapshot::now();
        s.cpu = Some(CpuSample {
            total: CpuTimes { user: 100, idle: 900, ..CpuTimes::default() },
            per_core: vec![CpuTimes::default(), CpuTimes::default()],
            freq_khz: vec![Some(crate::units::KiloHertz::from_khz(3_600_000)), None],
            load_avg: Some([1.0, 1.5, 2.0]),
            model: Some("Test CPU".to_string()),
            ctxt: Some(1),
            btime: Some(2),
        });
        s.memory = Some(MemorySample {
            total: Bytes::from_bytes(1000),
            available: Bytes::from_bytes(400),
            free: Bytes::from_bytes(100),
            ..MemorySample::default()
        });
        s
    }

    fn render(snapshot: &Snapshot, rates: Option<&Rates>, verbose: bool) -> String {
        let mut buf = Vec::new();
        write(&mut buf, snapshot, rates, verbose).expect("writes fine");
        String::from_utf8(buf).expect("valid utf-8")
    }

    #[test]
    fn writes_a_header_and_the_present_sections() {
        let text = render(&full_snapshot(), None, false);
        assert!(text.starts_with("rustmon —"));
        assert!(text.contains("CPU"));
        assert!(text.contains("Test CPU"));
        assert!(text.contains("cores: 2"));
        assert!(text.contains("Memory"));
        assert!(text.contains("load avg: 1.00 1.50 2.00"));
    }

    /// A section with no data must not print an empty, misleading heading —
    /// the whole point of per-section presence checks in `write()`.
    #[test]
    fn absent_sections_produce_no_heading_at_all() {
        let text = render(&full_snapshot(), None, false);
        for absent in ["Thermal", "Disk", "Net", "GPU", "Connections"] {
            assert!(!text.contains(absent), "unexpected section {absent}");
        }
    }

    #[test]
    fn connections_section_shows_owner_protocol_and_endpoints() {
        let mut snapshot = full_snapshot();
        snapshot.connections = Some(crate::sample::ConnectionSample {
            connections: vec![
                crate::sample::Connection {
                    protocol: crate::sample::ConnProtocol::Tcp,
                    local_addr: "192.168.1.1".parse().unwrap(),
                    local_port: 51000,
                    remote_addr: "8.8.8.8".parse().unwrap(),
                    remote_port: 443,
                    state: Some(crate::sample::TcpState::Established),
                    uid: 1000,
                    pid: Some(42),
                    program: Some("curl".to_string()),
                    ppid: Some(7),
                },
                crate::sample::Connection {
                    protocol: crate::sample::ConnProtocol::Udp,
                    local_addr: "192.168.1.1".parse().unwrap(),
                    local_port: 51001,
                    remote_addr: "1.1.1.1".parse().unwrap(),
                    remote_port: 53,
                    state: None,
                    uid: 1001,
                    pid: None,
                    program: None,
                    ppid: None,
                },
            ],
        });

        let text = render(&snapshot, None, false);
        assert!(text.contains("Connections"));
        assert!(text.contains("curl[42]"), "{text}");
        assert!(text.contains("8.8.8.8:443"), "{text}");
        assert!(text.contains("[established]"), "{text}");
        // An unattributed connection falls back to its uid, not a blank.
        assert!(text.contains("uid 1001"), "{text}");
    }

    #[test]
    fn a_core_with_no_frequency_reads_n_a_not_a_panic() {
        let text = render(&full_snapshot(), None, false);
        assert!(text.contains("n/a"));
    }

    #[test]
    fn verbose_prints_errors_and_quiet_mode_does_not() {
        let mut snapshot = full_snapshot();
        snapshot.errors.push(CollectorError { collector: "gpu", message: "boom".to_string() });

        let verbose_text = render(&snapshot, None, true);
        assert!(verbose_text.contains("Errors"));
        assert!(verbose_text.contains("gpu: boom"));

        let quiet_text = render(&snapshot, None, false);
        assert!(!quiet_text.contains("Errors"));
        assert!(!quiet_text.contains("boom"));
    }

    /// Every emitted line must contain no ANSI/control bytes at all — the
    /// module's central promise (see the module doc): this output is safe
    /// to pipe into a file or a pager unconditionally, not "safe unless a
    /// sensor label happens to be hostile."
    #[test]
    fn output_never_contains_control_bytes() {
        let mut snapshot = full_snapshot();
        snapshot.thermal = Some(crate::sample::ThermalSample {
            chips: vec![crate::sample::HwmonChip {
                name: "chip".to_string(),
                temps: vec![crate::sample::TempSensor {
                    label: "temp1".to_string(),
                    value: crate::units::MilliCelsius::from_millidegrees(45_000),
                    max: None,
                    crit: None,
                }],
                fans: Vec::new(),
            }],
        });

        let text = render(&snapshot, None, true);
        assert!(
            !text.chars().any(|c| (c as u32) < 0x20 && c != '\n'),
            "control byte leaked into text output"
        );
    }

    #[test]
    fn a_swapless_machine_has_no_swap_line() {
        let mut snapshot = full_snapshot();
        snapshot.memory = Some(MemorySample {
            total: Bytes::from_bytes(1000),
            available: Bytes::from_bytes(400),
            swap_total: Bytes::from_bytes(0),
            swap_free: Bytes::from_bytes(0),
            ..MemorySample::default()
        });
        assert!(!render(&snapshot, None, false).contains("swap"));
    }

    /// The whole snapshot renders through the full rate path too, not just
    /// the no-rates `--once` path — proves the disk/net "no rate yet"
    /// branches and their populated counterparts both produce sane output.
    #[test]
    fn renders_with_rates_present() {
        use crate::delta::{DiskRate, NetRate, Rates};
        use std::collections::HashMap;

        let mut snapshot = full_snapshot();
        snapshot.disks = Some(crate::sample::DiskSample {
            devices: vec![
                crate::sample::DiskDevice {
                    name: "sda".to_string(),
                    reads_completed: 1,
                    writes_completed: 2,
                    sectors_read: 3,
                    sectors_written: 4,
                    io_ticks_ms: 5,
                },
                crate::sample::DiskDevice {
                    name: "sdb-no-rate".to_string(),
                    reads_completed: 1,
                    writes_completed: 2,
                    sectors_read: 3,
                    sectors_written: 4,
                    io_ticks_ms: 5,
                },
            ],
            mounts: Vec::new(),
        });
        snapshot.net = Some(crate::sample::NetSample {
            interfaces: vec![crate::sample::NetInterface {
                name: "eth0".to_string(),
                rx_bytes: 1,
                tx_bytes: 2,
                rx_packets: 3,
                tx_packets: 4,
                rx_errors: 0,
                tx_errors: 0,
                rx_dropped: 0,
                tx_dropped: 0,
                operstate: Some("up".to_string()),
                mtu: Some(1500),
            }],
        });

        let mut disk_rates = HashMap::new();
        disk_rates.insert(
            "sda".to_string(),
            DiskRate {
                read: crate::units::BytesPerSec::new(100.0),
                write: crate::units::BytesPerSec::new(50.0),
                read_iops: 1.0,
                write_iops: 2.0,
                utilisation: Some(crate::units::Percent::new(5.0)),
            },
        );
        let mut net_rates = HashMap::new();
        net_rates.insert(
            "eth0".to_string(),
            NetRate {
                rx: crate::units::BytesPerSec::new(10.0),
                tx: crate::units::BytesPerSec::new(20.0),
                rx_packets_per_sec: 1.0,
                tx_packets_per_sec: 2.0,
            },
        );
        let rates = Rates {
            interval_secs: 1.0,
            cpu_total: Some(crate::units::Percent::new(12.5)),
            cpu_per_core: vec![Some(crate::units::Percent::new(12.5)), None],
            disk: disk_rates,
            net: net_rates,
        };

        let text = render(&snapshot, Some(&rates), false);
        assert!(text.contains("sda"));
        assert!(text.contains("no rate yet"), "sdb-no-rate should hit the no-rate branch");
        assert!(text.contains("eth0"));
        assert!(text.contains("[up]"));
    }
}
