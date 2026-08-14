//! Screen layout.
//!
//! Responsive rules, decided here rather than scattered through the widgets:
//!
//! - **< 60 columns**: single column, CPU and memory only, no sparklines.
//! - **60-119 columns**: two columns, sparklines on.
//! - **>= 120 columns**: three columns, per-core CPU bars expanded.
//! - **< 20 rows**: drop the help footer, then the least-important panel.
//!
//! Panels for absent hardware (no GPU, no sensors) are omitted entirely rather
//! than shown empty — a permanently blank "GPU" box just wastes rows. This
//! module only decides which panels exist and where; whether a panel draws a
//! sparkline for the width it's given is [`crate::ui::widgets`]'s call, not
//! this one's.

/// A rectangle in terminal cells. Backend-independent so layout is testable
/// without a terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
}

/// Where each panel goes this frame. `None` = not shown at this size.
#[derive(Debug, Clone, Default)]
pub struct Layout {
    pub header: Option<Rect>,
    pub cpu: Option<Rect>,
    pub memory: Option<Rect>,
    pub thermal: Option<Rect>,
    pub disk: Option<Rect>,
    pub net: Option<Rect>,
    pub gpu: Option<Rect>,
    pub connections: Option<Rect>,
    pub footer: Option<Rect>,
}

/// Which panels have data worth showing this frame.
#[derive(Debug, Clone, Copy, Default)]
pub struct PanelPresence {
    pub thermal: bool,
    pub disk: bool,
    pub net: bool,
    pub gpu: bool,
    pub connections: bool,
}

const NARROW_WIDTH: u16 = 60;
const WIDE_WIDTH: u16 = 120;
const SHORT_HEIGHT: u16 = 20;

/// The smallest a panel is allowed to shrink to in the row-constrained grid
/// before it's dropped entirely rather than rendered unusably small — one
/// row for a label, one for a value.
const MIN_PANEL_ROWS: u16 = 2;

const HEADER_ROWS: u16 = 1;
const FOOTER_ROWS: u16 = 1;

/// Compute the layout for a terminal of this size.
///
/// Must never produce a `Rect` extending past `width`/`height`, and must never
/// produce a zero-width or zero-height rect — both are drawing bugs that
/// surface as a corrupted screen rather than an error. Under enough row
/// pressure this means showing fewer panels (or none beyond the header)
/// rather than shrinking one to nothing.
pub fn compute(width: u16, height: u16, presence: PanelPresence) -> Layout {
    let mut layout = Layout::default();
    if width == 0 || height == 0 {
        return layout;
    }

    let narrow = width < NARROW_WIDTH;
    let cols: usize = if width >= WIDE_WIDTH {
        3
    } else if narrow {
        1
    } else {
        2
    };

    // Most important first — also the drop order below, popped from the
    // end under row pressure. CPU and memory never drop: every machine this
    // crate targets has both. GPU and connections are dropped first among
    // the rest: GPU is the least universally applicable panel (plenty of
    // machines have none at all), and connections is opt-in enrichment
    // territory rather than core hardware telemetry, so it's the single
    // most natural thing to sacrifice under row pressure — appended last.
    let mut wanted: Vec<&'static str> = vec!["cpu", "memory"];
    if !narrow {
        if presence.thermal {
            wanted.push("thermal");
        }
        if presence.net {
            wanted.push("net");
        }
        if presence.disk {
            wanted.push("disk");
        }
        if presence.gpu {
            wanted.push("gpu");
        }
        if presence.connections {
            wanted.push("connections");
        }
    }

    layout.header = Some(Rect {
        x: 0,
        y: 0,
        width,
        height: HEADER_ROWS,
    });
    let mut rows_left = height - HEADER_ROWS;

    // Footer is the first thing dropped under row pressure (doc rule: "< 20
    // rows: drop the help footer, then the least-important panel"). The
    // `> FOOTER_ROWS` (not `>=`) check additionally means a footer is never
    // placed if doing so would leave zero rows for every panel — better to
    // show a sliver of real data than a footer over an empty screen.
    if height >= SHORT_HEIGHT && rows_left > FOOTER_ROWS {
        rows_left -= FOOTER_ROWS;
        layout.footer = Some(Rect {
            x: 0,
            y: height - FOOTER_ROWS,
            width,
            height: FOOTER_ROWS,
        });
    }

    while wanted.len() > 2 {
        let grid_rows = wanted.len().div_ceil(cols) as u16;
        if grid_rows.saturating_mul(MIN_PANEL_ROWS) <= rows_left {
            break;
        }
        wanted.pop();
    }

    let grid_rows = wanted.len().div_ceil(cols) as u16;
    if rows_left < grid_rows {
        // Not even one row per panel-row fits. Bail out with just the
        // header (and footer, if already placed above) rather than emit a
        // zero-height panel rect.
        return layout;
    }

    let panel_area = Rect {
        x: 0,
        y: HEADER_ROWS,
        width,
        height: rows_left,
    };
    let row_rects = split_rows(panel_area, &vec![1u16; grid_rows as usize]);

    for (row_idx, row_rect) in row_rects.iter().enumerate() {
        let start = row_idx * cols;
        let end = (start + cols).min(wanted.len());
        let names = &wanted[start..end];
        let cells = split_columns(*row_rect, names.len());
        for (name, rect) in names.iter().zip(cells) {
            match *name {
                "cpu" => layout.cpu = Some(rect),
                "memory" => layout.memory = Some(rect),
                "thermal" => layout.thermal = Some(rect),
                "disk" => layout.disk = Some(rect),
                "net" => layout.net = Some(rect),
                "gpu" => layout.gpu = Some(rect),
                "connections" => layout.connections = Some(rect),
                _ => unreachable!("wanted only ever contains the names matched above"),
            }
        }
    }

    layout
}

/// Split a rect into `n` columns of as-equal-as-possible width, distributing
/// the remainder rather than leaving a gap — the first `width % n` columns
/// get one extra cell.
///
/// `n == 0` returns an empty `Vec`. Meant for small `n` (a handful of
/// panels); not a general-purpose grid utility.
pub fn split_columns(area: Rect, n: usize) -> Vec<Rect> {
    if n == 0 {
        return Vec::new();
    }
    let n = n as u16;
    let base = area.width / n;
    let remainder = area.width % n;

    let mut rects = Vec::with_capacity(n as usize);
    let mut x = area.x;
    for i in 0..n {
        let w = base + u16::from(i < remainder);
        rects.push(Rect {
            x,
            y: area.y,
            width: w,
            height: area.height,
        });
        x += w;
    }
    rects
}

/// Split a rect into rows by weight.
///
/// Every row except the last gets `height * weight / total_weight`, floored;
/// the last row takes whatever height remains, so the rows' heights always
/// sum to exactly `area.height` regardless of rounding — the row-height
/// analogue of [`split_columns`]'s remainder distribution. An empty
/// `weights` or an all-zero one returns an empty `Vec` rather than dividing
/// by zero.
pub fn split_rows(area: Rect, weights: &[u16]) -> Vec<Rect> {
    if weights.is_empty() {
        return Vec::new();
    }
    let total_weight: u32 = weights.iter().map(|&w| u32::from(w)).sum();
    if total_weight == 0 {
        return Vec::new();
    }

    let total_height = u32::from(area.height);
    let mut rows = Vec::with_capacity(weights.len());
    let mut allocated: u32 = 0;
    let mut y = area.y;

    for (i, &w) in weights.iter().enumerate() {
        let h = if i + 1 == weights.len() {
            (total_height.saturating_sub(allocated)) as u16
        } else {
            ((total_height * u32::from(w)) / total_weight) as u16
        };
        allocated += u32::from(h);
        rows.push(Rect {
            x: area.x,
            y,
            width: area.width,
            height: h,
        });
        y += h;
    }

    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all_present() -> PanelPresence {
        PanelPresence {
            thermal: true,
            disk: true,
            net: true,
            gpu: true,
            connections: true,
        }
    }

    /// Every `Some(rect)` in a `Layout` stays within `width`/`height` and is
    /// never zero-sized — the two invariants `compute`'s own doc promises.
    fn assert_layout_is_valid(layout: &Layout, width: u16, height: u16) {
        for rect in [
            layout.header,
            layout.cpu,
            layout.memory,
            layout.thermal,
            layout.disk,
            layout.net,
            layout.gpu,
            layout.connections,
            layout.footer,
        ]
        .into_iter()
        .flatten()
        {
            assert!(rect.width > 0, "zero-width rect: {rect:?}");
            assert!(rect.height > 0, "zero-height rect: {rect:?}");
            assert!(
                rect.x + rect.width <= width,
                "rect {rect:?} extends past width {width}"
            );
            assert!(
                rect.y + rect.height <= height,
                "rect {rect:?} extends past height {height}"
            );
        }
    }

    #[test]
    fn narrow_terminal_shows_only_cpu_and_memory() {
        let layout = compute(40, 40, all_present());
        assert!(layout.cpu.is_some());
        assert!(layout.memory.is_some());
        assert!(layout.thermal.is_none());
        assert!(layout.disk.is_none());
        assert!(layout.net.is_none());
        assert!(layout.gpu.is_none());
        assert!(layout.connections.is_none());
        assert_layout_is_valid(&layout, 40, 40);
    }

    #[test]
    fn medium_terminal_shows_every_present_panel() {
        let layout = compute(90, 40, all_present());
        assert!(layout.cpu.is_some());
        assert!(layout.memory.is_some());
        assert!(layout.thermal.is_some());
        assert!(layout.disk.is_some());
        assert!(layout.net.is_some());
        assert!(layout.gpu.is_some());
        assert!(layout.connections.is_some());
        assert_layout_is_valid(&layout, 90, 40);
    }

    #[test]
    fn absent_hardware_panels_are_omitted_even_when_wide() {
        let layout = compute(200, 40, PanelPresence::default());
        assert!(layout.cpu.is_some());
        assert!(layout.memory.is_some());
        assert!(layout.thermal.is_none());
        assert!(layout.disk.is_none());
        assert!(layout.net.is_none());
        assert!(layout.gpu.is_none());
        assert!(layout.connections.is_none());
    }

    #[test]
    fn short_terminal_drops_the_footer() {
        let short = compute(90, 19, all_present());
        assert!(short.footer.is_none());

        let tall = compute(90, 20, all_present());
        assert!(tall.footer.is_some());
    }

    #[test]
    fn very_short_terminal_drops_panels_before_the_header() {
        let layout = compute(90, 3, all_present());
        assert!(layout.header.is_some(), "header must survive if anything does");
        assert!(layout.footer.is_none());
        // 2 rows left after the header, split across at least 3 panel-rows
        // at 2 cols — not enough for all 6, some must have been dropped,
        // but cpu/memory never do.
        assert!(layout.cpu.is_some());
        assert!(layout.memory.is_some());
        assert_layout_is_valid(&layout, 90, 3);
    }

    #[test]
    fn a_single_row_terminal_shows_only_the_header() {
        let layout = compute(90, 1, all_present());
        assert!(layout.header.is_some());
        assert!(layout.cpu.is_none());
        assert!(layout.memory.is_none());
        assert!(layout.footer.is_none());
        assert_layout_is_valid(&layout, 90, 1);
    }

    #[test]
    fn zero_width_or_height_produces_an_empty_layout_not_a_panic() {
        let by_width = compute(0, 40, all_present());
        assert!(by_width.header.is_none());

        let by_height = compute(90, 0, all_present());
        assert!(by_height.header.is_none());
    }

    #[test]
    fn split_columns_distributes_the_remainder_to_the_first_columns() {
        let area = Rect { x: 0, y: 0, width: 10, height: 5 };
        let cols = split_columns(area, 3);
        assert_eq!(cols.len(), 3);
        // 10 / 3 = 3 remainder 1: the first column gets the extra cell.
        assert_eq!(cols[0].width, 4);
        assert_eq!(cols[1].width, 3);
        assert_eq!(cols[2].width, 3);
        // No gaps: widths sum exactly to the original width, and each
        // column starts exactly where the previous one ended.
        assert_eq!(cols.iter().map(|r| r.width).sum::<u16>(), area.width);
        assert_eq!(cols[0].x, 0);
        assert_eq!(cols[1].x, 4);
        assert_eq!(cols[2].x, 7);
    }

    #[test]
    fn split_columns_zero_is_empty_not_a_panic() {
        let area = Rect { x: 0, y: 0, width: 10, height: 5 };
        assert!(split_columns(area, 0).is_empty());
    }

    #[test]
    fn split_rows_gives_the_remainder_to_the_last_row() {
        let area = Rect { x: 0, y: 0, width: 10, height: 10 };
        let rows = split_rows(area, &[1, 1, 1]);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].height, 3);
        assert_eq!(rows[1].height, 3);
        assert_eq!(rows[2].height, 4);
        assert_eq!(rows.iter().map(|r| r.height).sum::<u16>(), area.height);
    }

    #[test]
    fn split_rows_respects_weights() {
        let area = Rect { x: 0, y: 0, width: 10, height: 30 };
        let rows = split_rows(area, &[1, 2]);
        assert_eq!(rows[0].height, 10);
        assert_eq!(rows[1].height, 20);
    }

    #[test]
    fn split_rows_empty_or_zero_weight_is_empty_not_a_panic() {
        let area = Rect { x: 0, y: 0, width: 10, height: 10 };
        assert!(split_rows(area, &[]).is_empty());
        assert!(split_rows(area, &[0, 0]).is_empty());
    }

    /// No fixed set of examples proves the invariants hold everywhere, but a
    /// sweep across the breakpoints this module documents (and the sizes
    /// just below/above each one) is a reasonable stand-in for a terminal a
    /// user might actually resize to.
    #[test]
    fn invariants_hold_across_a_sweep_of_realistic_sizes() {
        for width in [0, 1, 20, 59, 60, 61, 90, 119, 120, 121, 300] {
            for height in [0, 1, 2, 3, 5, 10, 19, 20, 21, 50, 200] {
                let layout = compute(width, height, all_present());
                assert_layout_is_valid(&layout, width, height);
            }
        }
    }
}
