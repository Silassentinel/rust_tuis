//! Per-area panel rendering.
//!
//! Every function here takes a [`Snapshot`] and optional [`Rates`] and draws
//! one panel. None of them read the filesystem — that's the collectors' job,
//! and keeping the boundary strict is what lets the whole UI be exercised
//! against a hand-built `Snapshot` in a test.
//!
//! Absent values render as `—`, never as `0`. A fan that isn't reporting and a
//! fan that is stopped are different facts, and a monitor that conflates them
//! is worse than useless — it's misleading in exactly the situation you'd be
//! looking at it. A stopped fan (a real, present `FanSensor` reporting `0`
//! rpm) still prints `0 rpm`: fail-soft sensor reads (`collectors::thermal`)
//! already drop a sensor that isn't reporting at all before it ever reaches a
//! `Snapshot`, so every `FanSensor` this module sees is a real reading, and
//! `0` there is a fact worth showing, not an absence to hide.

use std::time::Duration;

use ratatui::layout::{Constraint, Layout as RLayout, Rect as RRect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Gauge, List, ListItem, Paragraph, Row, Sparkline, Table};
use ratatui::Frame;

use crate::delta::Rates;
use crate::sample::{Snapshot, TempSeverity};
use crate::ui::layout::Rect;
use crate::units::Percent;

fn to_ratatui(area: Rect) -> RRect {
    RRect::new(area.x, area.y, area.width, area.height)
}

/// Title bar: hostname, kernel, uptime, CPU model, refresh interval, paused
/// indicator.
///
/// `hostname`/`kernel` are read once by [`crate::ui::app`] via the same
/// [`crate::sysfs::SysfsReader`] every collector uses (`proc/sys/kernel/
/// hostname`/`osrelease`) — not part of any [`Snapshot`], since they're
/// static machine identity, not a per-refresh reading. The chunk-0 skeleton's
/// own doc promised this header content but `Snapshot` had nowhere to carry
/// it; extending this function's signature rather than silently dropping
/// half of what the header was meant to show.
pub fn draw_header(
    frame: &mut Frame,
    area: Rect,
    snapshot: &Snapshot,
    hostname: &str,
    kernel: &str,
    interval: Duration,
    paused: bool,
) {
    let mut spans = Vec::new();
    spans.push(Span::styled(
        if hostname.is_empty() { "rustmon".to_string() } else { hostname.to_string() },
        Style::default().add_modifier(Modifier::BOLD),
    ));
    if !kernel.is_empty() {
        spans.push(Span::raw(format!("  {kernel}")));
    }
    if let Some(cpu) = &snapshot.cpu {
        if let Some(model) = &cpu.model {
            spans.push(Span::raw(format!("  {model}")));
        }
        if let Some(btime) = cpu.btime {
            if let Ok(now) = snapshot.taken_at.duration_since(std::time::UNIX_EPOCH) {
                let uptime_secs = now.as_secs().saturating_sub(btime);
                spans.push(Span::raw(format!("  up {}", human_duration(uptime_secs))));
            }
        }
    }
    spans.push(Span::raw(format!("  every {:.1}s", interval.as_secs_f64())));
    if paused {
        spans.push(Span::styled(
            "  PAUSED",
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        ));
    }

    frame.render_widget(Paragraph::new(Line::from(spans)), to_ratatui(area));
}

fn human_duration(total_secs: u64) -> String {
    let days = total_secs / 86_400;
    let hours = (total_secs % 86_400) / 3_600;
    let minutes = (total_secs % 3_600) / 60;
    if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {minutes}m")
    } else {
        format!("{minutes}m")
    }
}

/// Aggregate gauge, per-core bars, frequency, load average, and a sparkline of
/// recent history.
///
/// The sparkline is built from `history` here, not precomputed — `history` is
/// raw [`Snapshot`]s (counters), and turning consecutive pairs into a busy%
/// trail is [`crate::delta::cpu_busy_percent`], a pure computation with no
/// I/O, so doing it here doesn't violate this module's "no filesystem access"
/// rule any more than reading `rates.cpu_total` does.
pub fn draw_cpu(frame: &mut Frame, area: Rect, snapshot: &Snapshot, rates: Option<&Rates>, history: &[Snapshot]) {
    let outer = to_ratatui(area);
    let block = Block::bordered().title("CPU");
    let inner = block.inner(outer);
    frame.render_widget(block, outer);

    let Some(cpu) = &snapshot.cpu else {
        frame.render_widget(Paragraph::new("no CPU data"), inner);
        return;
    };

    let busy = rates.and_then(|r| r.cpu_total);
    let load = cpu
        .load_avg
        .map(|l| format!("{:.2} {:.2} {:.2}", l[0], l[1], l[2]))
        .unwrap_or_else(|| "—".to_string());

    let has_history = history.len() >= 2;
    let chunks = RLayout::vertical([
        Constraint::Length(1),
        Constraint::Length(3),
        Constraint::Min(0),
        Constraint::Length(if has_history { 3 } else { 0 }),
    ])
    .split(inner);

    frame.render_widget(Paragraph::new(format!("load avg {load}")), chunks[0]);

    let ratio = busy.map(|p| (p.as_f64() / 100.0).clamp(0.0, 1.0)).unwrap_or(0.0);
    let label = busy.map(|p| format!("{:.1}%", p.as_f64())).unwrap_or_else(|| "—".to_string());
    let gauge = Gauge::default()
        .block(Block::new().title("busy"))
        .gauge_style(Style::default().fg(percent_colour(busy)))
        .ratio(ratio)
        .label(label);
    frame.render_widget(gauge, chunks[1]);

    let items: Vec<ListItem> = cpu
        .freq_khz
        .iter()
        .enumerate()
        .map(|(idx, freq)| {
            let core_busy = rates.and_then(|r| r.cpu_per_core.get(idx).copied().flatten());
            let bar = unicode_bar(core_busy.map(|p| p.as_f64() / 100.0).unwrap_or(0.0), 12);
            let busy_str = core_busy
                .map(|p| format!("{:>5.1}%", p.as_f64()))
                .unwrap_or_else(|| "    —%".to_string());
            let freq_str = freq
                .map(|f| format!("{:.2} GHz", f.as_ghz()))
                .unwrap_or_else(|| "—".to_string());
            ListItem::new(format!("core{idx:<3} {bar} {busy_str} {freq_str}"))
        })
        .collect();
    frame.render_widget(List::new(items), chunks[2]);

    if has_history {
        let data: Vec<u64> = history
            .windows(2)
            .filter_map(|pair| {
                let prev = pair[0].cpu.as_ref()?;
                let curr = pair[1].cpu.as_ref()?;
                crate::delta::cpu_busy_percent(&prev.total, &curr.total).map(|p| p.as_f64().round() as u64)
            })
            .collect();
        let sparkline = Sparkline::default().block(Block::new().title("history")).data(&data);
        frame.render_widget(sparkline, chunks[3]);
    }
}

fn percent_colour(p: Option<Percent>) -> Color {
    match p.map(|p| p.as_f64()) {
        None => Color::Gray,
        Some(v) if v >= 90.0 => Color::Red,
        Some(v) if v >= 70.0 => Color::Yellow,
        Some(_) => Color::Green,
    }
}

/// `fraction` in `[0, 1]` as a row of solid/empty block characters. NaN is
/// treated as `0.0` before clamping — the same trap `render::text::ascii_bar`
/// guards against, for the same reason (`f64::clamp` propagates NaN rather
/// than clamping it). Unicode blocks, not the ASCII renderer's `#`/`-`: that
/// renderer is deliberately plain-ASCII so piped output survives anywhere,
/// which doesn't apply to a real terminal UI.
fn unicode_bar(fraction: f64, width: usize) -> String {
    let fraction = if fraction.is_nan() { 0.0 } else { fraction.clamp(0.0, 1.0) };
    let filled = ((fraction * width as f64).round() as usize).min(width);
    format!("{}{}", "█".repeat(filled), "░".repeat(width - filled))
}

/// RAM and swap gauges with used/total figures.
pub fn draw_memory(frame: &mut Frame, area: Rect, snapshot: &Snapshot) {
    let outer = to_ratatui(area);
    let block = Block::bordered().title("Memory");
    let inner = block.inner(outer);
    frame.render_widget(block, outer);

    let Some(mem) = &snapshot.memory else {
        frame.render_widget(Paragraph::new("no memory data"), inner);
        return;
    };

    let has_swap = mem.swap_total.as_u64() > 0;
    let chunks = RLayout::vertical(vec![Constraint::Length(3); if has_swap { 2 } else { 1 }]).split(inner);

    let ram_percent = mem.used_percent();
    let ram_ratio = ram_percent.map(|p| (p.as_f64() / 100.0).clamp(0.0, 1.0)).unwrap_or(0.0);
    let ram_label = ram_percent.map(|p| format!("{:.1}%", p.as_f64())).unwrap_or_else(|| "—".to_string());
    let ram_gauge = Gauge::default()
        .block(Block::new().title(format!("RAM {} / {}", mem.used(), mem.total)))
        .gauge_style(Style::default().fg(percent_colour(ram_percent)))
        .ratio(ram_ratio)
        .label(ram_label);
    frame.render_widget(ram_gauge, chunks[0]);

    if has_swap {
        let swap_percent = Percent::from_ratio(mem.swap_used().as_u64(), mem.swap_total.as_u64());
        let swap_ratio = swap_percent.map(|p| (p.as_f64() / 100.0).clamp(0.0, 1.0)).unwrap_or(0.0);
        let swap_label = swap_percent
            .map(|p| format!("{:.1}%", p.as_f64()))
            .unwrap_or_else(|| "—".to_string());
        let swap_gauge = Gauge::default()
            .block(Block::new().title(format!("swap {} / {}", mem.swap_used(), mem.swap_total)))
            .gauge_style(Style::default().fg(percent_colour(swap_percent)))
            .ratio(swap_ratio)
            .label(swap_label);
        frame.render_widget(swap_gauge, chunks[1]);
    }
}

/// Sensors grouped by chip, coloured by [`TempSeverity`], with fan RPMs.
pub fn draw_thermal(frame: &mut Frame, area: Rect, snapshot: &Snapshot) {
    let outer = to_ratatui(area);
    let block = Block::bordered().title("Thermal");
    let inner = block.inner(outer);
    frame.render_widget(block, outer);

    let Some(thermal) = &snapshot.thermal else {
        frame.render_widget(Paragraph::new("no thermal data"), inner);
        return;
    };

    let mut items: Vec<ListItem> = Vec::new();
    for chip in &thermal.chips {
        items.push(ListItem::new(Line::from(Span::styled(
            chip.name.clone(),
            Style::default().add_modifier(Modifier::BOLD),
        ))));
        for t in &chip.temps {
            let colour = ratatui_colour(severity_colour(t.severity()));
            let line = format!("  {:<24} {:>6.1} C", t.label, t.value.as_celsius());
            items.push(ListItem::new(Line::from(Span::styled(line, Style::default().fg(colour)))));
        }
        for f in &chip.fans {
            items.push(ListItem::new(format!("  {:<24} {} rpm", f.label, f.rpm.as_u64())));
        }
    }

    if items.is_empty() {
        frame.render_widget(Paragraph::new("no sensors"), inner);
    } else {
        frame.render_widget(List::new(items), inner);
    }
}

/// Per-device read/write throughput and utilisation.
pub fn draw_disk(frame: &mut Frame, area: Rect, snapshot: &Snapshot, rates: Option<&Rates>) {
    let outer = to_ratatui(area);
    let block = Block::bordered().title("Disk");
    let inner = block.inner(outer);
    frame.render_widget(block, outer);

    let Some(disk) = &snapshot.disks else {
        frame.render_widget(Paragraph::new("no disk data"), inner);
        return;
    };

    let rows: Vec<Row> = disk
        .devices
        .iter()
        .map(|d| match rates.and_then(|r| r.disk.get(&d.name)) {
            Some(r) => Row::new(vec![
                d.name.clone(),
                r.read.human(),
                r.write.human(),
                r.utilisation
                    .map(|p| format!("{:.0}%", p.as_f64()))
                    .unwrap_or_else(|| "—".to_string()),
            ]),
            None => Row::new(vec![d.name.clone(), "—".to_string(), "—".to_string(), "—".to_string()]),
        })
        .collect();

    let widths = [
        Constraint::Length(10),
        Constraint::Length(12),
        Constraint::Length(12),
        Constraint::Length(6),
    ];
    let table = Table::new(rows, widths).header(
        Row::new(vec!["device", "read", "write", "util"]).style(Style::default().add_modifier(Modifier::BOLD)),
    );

    if disk.mounts.is_empty() {
        frame.render_widget(table, inner);
        return;
    }

    let device_rows = (disk.devices.len() as u16 + 1).max(2);
    let chunks = RLayout::vertical([Constraint::Length(device_rows), Constraint::Min(0)]).split(inner);
    frame.render_widget(table, chunks[0]);

    let mount_lines: Vec<Line> = disk
        .mounts
        .iter()
        .map(|m| {
            let capacity = match (m.available, m.total) {
                (Some(available), Some(total)) => format!("{available} / {total}"),
                _ => "—".to_string(),
            };
            Line::from(format!("{} -> {} [{}] {capacity}", m.source, m.mount_point, m.fs_type))
        })
        .collect();
    frame.render_widget(Paragraph::new(mount_lines), chunks[1]);
}

/// Per-interface RX/TX throughput and link state.
pub fn draw_net(frame: &mut Frame, area: Rect, snapshot: &Snapshot, rates: Option<&Rates>) {
    let outer = to_ratatui(area);
    let block = Block::bordered().title("Net");
    let inner = block.inner(outer);
    frame.render_widget(block, outer);

    let Some(net) = &snapshot.net else {
        frame.render_widget(Paragraph::new("no net data"), inner);
        return;
    };

    let rows: Vec<Row> = net
        .interfaces
        .iter()
        .map(|i| {
            let state = i.operstate.as_deref().unwrap_or("—");
            match rates.and_then(|r| r.net.get(&i.name)) {
                Some(r) => Row::new(vec![i.name.clone(), state.to_string(), r.rx.human(), r.tx.human()]),
                None => Row::new(vec![i.name.clone(), state.to_string(), "—".to_string(), "—".to_string()]),
            }
        })
        .collect();

    let widths = [
        Constraint::Length(12),
        Constraint::Length(8),
        Constraint::Length(12),
        Constraint::Length(12),
    ];
    let table = Table::new(rows, widths).header(
        Row::new(vec!["interface", "state", "rx", "tx"]).style(Style::default().add_modifier(Modifier::BOLD)),
    );
    frame.render_widget(table, inner);
}

/// Per-GPU utilisation, VRAM, temperature, power.
pub fn draw_gpu(frame: &mut Frame, area: Rect, snapshot: &Snapshot) {
    let outer = to_ratatui(area);
    let block = Block::bordered().title("GPU");
    let inner = block.inner(outer);
    frame.render_widget(block, outer);

    let Some(gpu) = &snapshot.gpus else {
        frame.render_widget(Paragraph::new("no GPU data"), inner);
        return;
    };

    let mut items: Vec<ListItem> = Vec::new();
    for g in &gpu.gpus {
        let busy = g.busy.map(|p| format!("{:.0}%", p.as_f64())).unwrap_or_else(|| "—".to_string());
        items.push(ListItem::new(Line::from(Span::styled(
            format!("{} ({:?}) {busy}", g.name, g.vendor),
            Style::default().add_modifier(Modifier::BOLD),
        ))));

        if let (Some(used), Some(total)) = (g.vram_used, g.vram_total) {
            items.push(ListItem::new(format!("  vram {used} / {total}")));
        }
        if let Some(t) = g.temp {
            items.push(ListItem::new(format!("  temp {:.1} C", t.as_celsius())));
        }
        if let Some(f) = g.freq_khz {
            items.push(ListItem::new(format!("  freq {:.2} GHz", f.as_ghz())));
        }
    }

    if items.is_empty() {
        frame.render_widget(Paragraph::new("no GPUs found"), inner);
    } else {
        frame.render_widget(List::new(items), inner);
    }
}

const HELP_HINT: &str = "q quit  Tab next  1-6 jump  space pause  r reset  ? help  +/- interval";

/// Key hints, and collector errors when `--verbose`.
///
/// The footer is a single row (see `layout::compute`'s `FOOTER_ROWS`), so
/// there's room for one line: the key hints normally, or — under
/// `--verbose`, when a collector actually failed this refresh — the first
/// error instead, with a `(+N more)` suffix rather than trying to cram every
/// failure in.
pub fn draw_footer(frame: &mut Frame, area: Rect, snapshot: &Snapshot, verbose: bool) {
    let outer = to_ratatui(area);

    if verbose {
        if let Some(first) = snapshot.errors.first() {
            let suffix = if snapshot.errors.len() > 1 {
                format!(" (+{} more)", snapshot.errors.len() - 1)
            } else {
                String::new()
            };
            let text = format!("! {}: {}{suffix}", first.collector, first.message);
            frame.render_widget(Paragraph::new(text).style(Style::default().fg(Color::Red)), outer);
            return;
        }
    }

    frame.render_widget(Paragraph::new(HELP_HINT), outer);
}

/// Full-screen help overlay.
pub fn draw_help(frame: &mut Frame, area: Rect) {
    let outer = to_ratatui(area);
    frame.render_widget(Clear, outer);
    let block = Block::bordered().title("Help");
    let inner = block.inner(outer);
    frame.render_widget(block, outer);

    let text = [
        "q / Esc        quit",
        "Tab / S-Tab    cycle panels",
        "1-6            jump to a panel",
        "space          pause",
        "r              reset the rate tracker",
        "?              toggle this help",
        "+ / -          adjust the refresh interval",
    ]
    .join("\n");
    frame.render_widget(Paragraph::new(text), inner);
}

/// Map a temperature severity to a display colour.
///
/// Returns the crate's own enum rather than a backend colour type, so the
/// mapping is testable and the backend is swappable.
pub fn severity_colour(severity: TempSeverity) -> Colour {
    match severity {
        TempSeverity::Unknown => Colour::Grey,
        TempSeverity::Normal => Colour::Green,
        TempSeverity::Warning => Colour::Yellow,
        TempSeverity::Critical => Colour::Red,
    }
}

fn ratatui_colour(c: Colour) -> Color {
    match c {
        Colour::Default => Color::Reset,
        Colour::Green => Color::Green,
        Colour::Yellow => Color::Yellow,
        Colour::Red => Color::Red,
        Colour::Cyan => Color::Cyan,
        Colour::Grey => Color::Gray,
    }
}

/// Backend-independent colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Colour {
    Default,
    Green,
    Yellow,
    Red,
    Cyan,
    Grey,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sample::{CpuSample, CpuTimes, FanSensor, Gpu, GpuSample, GpuVendor, HwmonChip, MemorySample, TempSensor, ThermalSample};
    use crate::units::{Bytes, KiloHertz, MilliCelsius};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn full_area(width: u16, height: u16) -> Rect {
        Rect { x: 0, y: 0, width, height }
    }

    /// Renders `draw` into a `TestBackend` of the given size and returns the
    /// screen as plain text, one line per row — enough to assert specific
    /// content appeared without coupling tests to exact cell styling.
    fn render_to_text(width: u16, height: u16, draw: impl FnOnce(&mut Frame)) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal.draw(|frame| draw(frame)).expect("draw must not panic");

        let buffer = terminal.backend().buffer();
        let mut out = String::new();
        for y in 0..height {
            for x in 0..width {
                if let Some(cell) = buffer.cell((x, y)) {
                    out.push_str(cell.symbol());
                }
            }
            out.push('\n');
        }
        out
    }

    fn snapshot_with_cpu() -> Snapshot {
        let mut s = Snapshot::now();
        s.cpu = Some(CpuSample {
            total: CpuTimes { user: 100, idle: 900, ..CpuTimes::default() },
            per_core: vec![CpuTimes::default(); 2],
            freq_khz: vec![Some(KiloHertz::from_khz(3_500_000)), None],
            load_avg: Some([0.5, 0.4, 0.3]),
            model: Some("Test CPU".to_string()),
            ctxt: None,
            btime: Some(0),
        });
        s
    }

    // ---- draw_header --------------------------------------------------------

    #[test]
    fn header_shows_hostname_and_model_and_does_not_panic() {
        let snapshot = snapshot_with_cpu();
        let text = render_to_text(80, 3, |frame| {
            draw_header(frame, full_area(80, 3), &snapshot, "myhost", "6.1.0", Duration::from_secs(2), false);
        });
        assert!(text.contains("myhost"), "{text}");
        assert!(text.contains("Test CPU"), "{text}");
        assert!(!text.contains("PAUSED"), "{text}");
    }

    #[test]
    fn header_shows_paused_indicator_when_paused() {
        let snapshot = Snapshot::now();
        let text = render_to_text(80, 3, |frame| {
            draw_header(frame, full_area(80, 3), &snapshot, "", "", Duration::from_secs(2), true);
        });
        assert!(text.contains("PAUSED"), "{text}");
    }

    #[test]
    fn header_with_no_hostname_falls_back_to_the_crate_name() {
        let snapshot = Snapshot::now();
        let text = render_to_text(80, 3, |frame| {
            draw_header(frame, full_area(80, 3), &snapshot, "", "", Duration::from_secs(1), false);
        });
        assert!(text.contains("rustmon"), "{text}");
    }

    // ---- draw_cpu -------------------------------------------------------------

    #[test]
    fn cpu_panel_renders_model_and_per_core_rows_without_panic() {
        let snapshot = snapshot_with_cpu();
        let text = render_to_text(80, 20, |frame| {
            draw_cpu(frame, full_area(80, 20), &snapshot, None, &[]);
        });
        assert!(text.contains("CPU"), "{text}");
        assert!(text.contains("core0"), "{text}");
        assert!(text.contains("core1"), "{text}");
    }

    #[test]
    fn cpu_panel_with_no_data_does_not_panic() {
        let snapshot = Snapshot::now();
        let text = render_to_text(80, 20, |frame| {
            draw_cpu(frame, full_area(80, 20), &snapshot, None, &[]);
        });
        assert!(text.contains("no CPU data"), "{text}");
    }

    #[test]
    fn cpu_panel_draws_a_sparkline_when_history_has_at_least_two_snapshots() {
        let a = snapshot_with_cpu();
        let mut b = snapshot_with_cpu();
        if let Some(cpu) = &mut b.cpu {
            cpu.total = CpuTimes { user: 200, idle: 1_000, ..CpuTimes::default() };
        }
        let history = [a, b];
        // Must not panic building the sparkline data from raw counters.
        let _text = render_to_text(80, 20, |frame| {
            draw_cpu(frame, full_area(80, 20), &history[1], None, &history);
        });
    }

    #[test]
    fn absent_per_core_frequency_and_rate_render_as_an_em_dash_not_zero() {
        let snapshot = snapshot_with_cpu(); // core1's freq is None
        let text = render_to_text(80, 20, |frame| {
            draw_cpu(frame, full_area(80, 20), &snapshot, None, &[]);
        });
        assert!(text.contains('—'), "expected an em-dash for absent data: {text}");
    }

    // ---- draw_memory ----------------------------------------------------------

    #[test]
    fn memory_panel_shows_used_and_total() {
        let mut snapshot = Snapshot::now();
        snapshot.memory = Some(MemorySample {
            total: Bytes::from_bytes(1_000_000_000),
            available: Bytes::from_bytes(400_000_000),
            swap_total: Bytes::from_bytes(0),
            swap_free: Bytes::from_bytes(0),
            ..MemorySample::default()
        });
        let text = render_to_text(60, 10, |frame| {
            draw_memory(frame, full_area(60, 10), &snapshot);
        });
        assert!(text.contains("RAM"), "{text}");
    }

    #[test]
    fn memory_panel_with_no_swap_omits_the_swap_gauge() {
        let mut snapshot = Snapshot::now();
        snapshot.memory = Some(MemorySample {
            total: Bytes::from_bytes(1_000),
            available: Bytes::from_bytes(500),
            swap_total: Bytes::from_bytes(0),
            swap_free: Bytes::from_bytes(0),
            ..MemorySample::default()
        });
        let text = render_to_text(60, 10, |frame| {
            draw_memory(frame, full_area(60, 10), &snapshot);
        });
        assert!(!text.contains("swap"), "{text}");
    }

    #[test]
    fn memory_panel_with_no_data_does_not_panic() {
        let snapshot = Snapshot::now();
        let text = render_to_text(60, 10, |frame| {
            draw_memory(frame, full_area(60, 10), &snapshot);
        });
        assert!(text.contains("no memory data"), "{text}");
    }

    // ---- draw_thermal -----------------------------------------------------------

    #[test]
    fn thermal_panel_lists_chip_name_and_sensor_label() {
        let mut snapshot = Snapshot::now();
        snapshot.thermal = Some(ThermalSample {
            chips: vec![HwmonChip {
                name: "k10temp".to_string(),
                temps: vec![TempSensor {
                    label: "Tctl".to_string(),
                    value: MilliCelsius::from_millidegrees(45_000),
                    max: Some(MilliCelsius::from_millidegrees(90_000)),
                    crit: Some(MilliCelsius::from_millidegrees(100_000)),
                }],
                fans: vec![FanSensor { label: "fan1".to_string(), rpm: crate::units::Rpm::from_rpm(0) }],
            }],
        });
        let text = render_to_text(60, 10, |frame| {
            draw_thermal(frame, full_area(60, 10), &snapshot);
        });
        assert!(text.contains("k10temp"), "{text}");
        assert!(text.contains("Tctl"), "{text}");
        // A stopped fan (a real reading of 0) must show "0 rpm", not an
        // absence marker — see this module's own doc.
        assert!(text.contains("0 rpm"), "{text}");
    }

    #[test]
    fn thermal_panel_with_no_data_does_not_panic() {
        let snapshot = Snapshot::now();
        let text = render_to_text(60, 10, |frame| {
            draw_thermal(frame, full_area(60, 10), &snapshot);
        });
        assert!(text.contains("no thermal data"), "{text}");
    }

    // ---- draw_disk / draw_net / draw_gpu: no-panic + no-data coverage --------

    #[test]
    fn disk_panel_with_no_data_does_not_panic() {
        let snapshot = Snapshot::now();
        let text = render_to_text(60, 10, |frame| {
            draw_disk(frame, full_area(60, 10), &snapshot, None);
        });
        assert!(text.contains("no disk data"), "{text}");
    }

    #[test]
    fn net_panel_with_no_data_does_not_panic() {
        let snapshot = Snapshot::now();
        let text = render_to_text(60, 10, |frame| {
            draw_net(frame, full_area(60, 10), &snapshot, None);
        });
        assert!(text.contains("no net data"), "{text}");
    }

    #[test]
    fn gpu_panel_with_no_data_does_not_panic() {
        let snapshot = Snapshot::now();
        let text = render_to_text(60, 10, |frame| {
            draw_gpu(frame, full_area(60, 10), &snapshot);
        });
        assert!(text.contains("no GPU data"), "{text}");
    }

    #[test]
    fn gpu_panel_lists_card_name_and_vendor() {
        let mut snapshot = Snapshot::now();
        snapshot.gpus = Some(GpuSample {
            gpus: vec![Gpu {
                vendor: GpuVendor::Amd,
                name: "card1".to_string(),
                busy: Percent::from_ratio(9, 100),
                vram_total: Some(Bytes::from_bytes(1_000)),
                vram_used: Some(Bytes::from_bytes(200)),
                temp: Some(MilliCelsius::from_millidegrees(40_000)),
                power: None,
                fan_rpm: None,
                freq_khz: None,
            }],
        });
        let text = render_to_text(60, 10, |frame| {
            draw_gpu(frame, full_area(60, 10), &snapshot);
        });
        assert!(text.contains("card1"), "{text}");
    }

    // ---- draw_footer ------------------------------------------------------------

    #[test]
    fn footer_shows_key_hints_when_quiet() {
        let snapshot = Snapshot::now();
        let text = render_to_text(80, 1, |frame| {
            draw_footer(frame, full_area(80, 1), &snapshot, false);
        });
        assert!(text.contains("quit"), "{text}");
    }

    #[test]
    fn footer_shows_the_first_error_under_verbose() {
        let mut snapshot = Snapshot::now();
        snapshot.errors.push(crate::sample::CollectorError {
            collector: "thermal",
            message: "boom".to_string(),
        });
        let text = render_to_text(80, 1, |frame| {
            draw_footer(frame, full_area(80, 1), &snapshot, true);
        });
        assert!(text.contains("thermal"), "{text}");
        assert!(text.contains("boom"), "{text}");
    }

    #[test]
    fn footer_ignores_errors_when_not_verbose() {
        let mut snapshot = Snapshot::now();
        snapshot.errors.push(crate::sample::CollectorError {
            collector: "thermal",
            message: "boom".to_string(),
        });
        let text = render_to_text(80, 1, |frame| {
            draw_footer(frame, full_area(80, 1), &snapshot, false);
        });
        assert!(!text.contains("boom"), "{text}");
        assert!(text.contains("quit"), "{text}");
    }

    // ---- draw_help / severity_colour -----------------------------------------

    #[test]
    fn help_overlay_lists_every_documented_key() {
        let text = render_to_text(60, 12, |frame| {
            draw_help(frame, full_area(60, 12));
        });
        for key in ["quit", "cycle panels", "pause", "reset", "help", "interval"] {
            assert!(text.contains(key), "missing {key:?} in help text: {text}");
        }
    }

    #[test]
    fn severity_colour_ranks_critical_above_warning_above_normal() {
        assert_eq!(severity_colour(TempSeverity::Critical), Colour::Red);
        assert_eq!(severity_colour(TempSeverity::Warning), Colour::Yellow);
        assert_eq!(severity_colour(TempSeverity::Normal), Colour::Green);
        assert_eq!(severity_colour(TempSeverity::Unknown), Colour::Grey);
    }

    #[test]
    fn unicode_bar_handles_nan_like_ascii_bar_does() {
        // Must not panic on NaN, and must not silently fill the bar either.
        assert_eq!(unicode_bar(f64::NAN, 10), "░".repeat(10));
        assert_eq!(unicode_bar(0.0, 10), "░".repeat(10));
        assert_eq!(unicode_bar(1.0, 10), "█".repeat(10));
    }
}
