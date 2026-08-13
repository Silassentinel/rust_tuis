//! TUI application state and event loop.
//!
//! The loop shape, which is the part worth deciding now:
//!
//! ```text
//! loop {
//!     poll for input, with a timeout of (next refresh - now)
//!     if input arrived    -> handle it, redraw
//!     if timeout elapsed  -> collect a snapshot, update rates and history, redraw
//! }
//! ```
//!
//! Polling with a computed timeout rather than sleeping a fixed interval means
//! keypresses feel instant while collection still happens on a steady cadence.
//! Sleeping the full interval and checking input afterwards makes the UI feel
//! laggy at a 2 s refresh; a busy-poll burns the CPU the tool is measuring.
//! The loop itself lives in [`crate::ui::run_loop`], since it also needs the
//! `ratatui`/`crossterm` types this module deliberately stays free of — see
//! [`KeyPress`]'s own doc for why.

use std::collections::VecDeque;
use std::path::Path;
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::delta::{RateTracker, Rates};
use crate::error::Result;
use crate::sysfs::{sanitize_kernel_string, SysfsReader};
use crate::sample::Snapshot;

/// Cap on the hostname/kernel-release strings read once at startup. Both are
/// always short in practice; this is the same defensive cap every other
/// kernel-supplied string in this crate gets before display.
const MAX_IDENTITY_LEN: usize = 256;

/// Which panel has focus, for scrolling and detail views.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Panel {
    #[default]
    Overview,
    Cpu,
    Memory,
    Thermal,
    Disk,
    Net,
    Gpu,
}

impl Panel {
    const ALL: [Panel; 7] = [
        Panel::Overview,
        Panel::Cpu,
        Panel::Memory,
        Panel::Thermal,
        Panel::Disk,
        Panel::Net,
        Panel::Gpu,
    ];

    fn index(self) -> usize {
        Self::ALL.iter().position(|p| *p == self).unwrap_or(0)
    }

    pub fn next(self) -> Panel {
        Self::ALL[(self.index() + 1) % Self::ALL.len()]
    }

    pub fn prev(self) -> Panel {
        Self::ALL[(self.index() + Self::ALL.len() - 1) % Self::ALL.len()]
    }

    /// `'1'..='6'` map onto the six data panels in the order they're listed
    /// in `ALL` (skipping `Overview`, which has no digit of its own — it's
    /// reached only by cycling with `Tab`/`Shift-Tab`). Any other character
    /// is `None`.
    pub fn from_digit(c: char) -> Option<Panel> {
        match c {
            '1' => Some(Panel::Cpu),
            '2' => Some(Panel::Memory),
            '3' => Some(Panel::Thermal),
            '4' => Some(Panel::Disk),
            '5' => Some(Panel::Net),
            '6' => Some(Panel::Gpu),
            _ => None,
        }
    }
}

/// Everything the UI needs between frames.
pub struct App {
    pub config: Config,
    pub registry: crate::collector::Registry,
    pub tracker: RateTracker,

    /// Most recent snapshot.
    pub current: Option<Snapshot>,
    /// Most recent rates, `None` on the first refresh or a dropped interval.
    pub rates: Option<Rates>,

    /// Fixed-capacity history for sparklines. A `VecDeque` with an explicit cap
    /// rather than a growing `Vec` — memory must stay bounded no matter how
    /// long the tool runs (security model item 8).
    pub history: VecDeque<Snapshot>,

    pub focus: Panel,
    pub paused: bool,
    pub show_help: bool,
    pub should_quit: bool,

    /// Machine identity, read once via the same [`SysfsReader`] every
    /// collector uses (`proc/sys/kernel/hostname`/`osrelease`), not
    /// refreshed every cycle — this is static machine identity, not a
    /// per-refresh reading, so it doesn't belong on [`Snapshot`]. Empty
    /// string if unreadable (masked `/proc`, permission denied); the header
    /// widget falls back to the crate name rather than showing a blank.
    pub hostname: String,
    pub kernel: String,

    last_refresh: Instant,
}

impl App {
    pub fn new(config: Config) -> Result<Self> {
        let reader = SysfsReader::with_root(config.sysfs_root.clone())?;
        let hostname = read_identity_field(&reader, "proc/sys/kernel/hostname");
        let kernel = read_identity_field(&reader, "proc/sys/kernel/osrelease");
        let registry = crate::collector::Registry::from_config(&config, reader)?;

        // Backdated so the very first `time_until_refresh()` call reports a
        // refresh as already due — the live TUI's first frame should show
        // real data immediately, not an interval's worth of blank panels.
        let last_refresh = Instant::now()
            .checked_sub(config.interval)
            .unwrap_or_else(Instant::now);

        Ok(App {
            config,
            registry,
            tracker: RateTracker::new(),
            current: None,
            rates: None,
            history: VecDeque::new(),
            focus: Panel::default(),
            paused: false,
            show_help: false,
            should_quit: false,
            hostname,
            kernel,
            last_refresh,
        })
    }

    /// One collection cycle: collect, update rates, push to history evicting
    /// the oldest past `config.history_len`.
    pub fn refresh(&mut self) -> Result<()> {
        let snapshot = self.registry.collect_all()?;
        self.rates = self.tracker.update(&snapshot);

        self.history.push_back(snapshot.clone());
        while self.history.len() > self.config.history_len {
            self.history.pop_front();
        }
        // `widgets::draw_cpu` wants a contiguous `&[Snapshot]`; doing the
        // rotation once here (only when the buffer actually changed) is
        // cheaper than a fresh `Vec` copy on every single frame draw.
        self.history.make_contiguous();

        self.current = Some(snapshot);
        self.last_refresh = Instant::now();
        Ok(())
    }

    /// Handle one key press.
    ///
    /// Bindings: `q`/`Esc` quit, `Tab`/`Shift-Tab` cycle panels, `1`-`6` jump
    /// to a panel, `space` pause, `r` reset the rate tracker, `?` help,
    /// `+`/`-` adjust the interval (clamped to [`crate::config::MIN_INTERVAL`]).
    pub fn on_key(&mut self, key: KeyPress) -> Result<()> {
        match key {
            KeyPress::CtrlC | KeyPress::Esc => self.should_quit = true,
            KeyPress::Tab => self.focus = self.focus.next(),
            KeyPress::BackTab => self.focus = self.focus.prev(),
            KeyPress::Char(c) => match c {
                'q' => self.should_quit = true,
                '?' => self.show_help = !self.show_help,
                ' ' => self.paused = !self.paused,
                'r' => self.tracker.reset(),
                '+' | '=' => {
                    self.config.interval = self.config.interval.saturating_add(Duration::from_millis(250));
                }
                '-' => {
                    self.config.interval = self
                        .config
                        .interval
                        .saturating_sub(Duration::from_millis(250))
                        .max(crate::config::MIN_INTERVAL);
                }
                '1'..='6' => {
                    if let Some(panel) = Panel::from_digit(c) {
                        self.focus = panel;
                    }
                }
                _ => {}
            },
            _ => {}
        }
        Ok(())
    }

    /// Handle a terminal resize.
    ///
    /// A deliberate no-op: [`crate::ui::layout::compute`] is called fresh
    /// from the live terminal size on every frame (`Frame::area()`), so a
    /// resize is already picked up by the very next draw with no state to
    /// update here. Kept as a real method (not deleted) to preserve the
    /// event-loop's dispatch shape documented in this module's own doc, and
    /// in case a later feature (e.g. persisting a user's manual column
    /// choice) needs to react to a resize explicitly.
    pub fn on_resize(&mut self, _width: u16, _height: u16) -> Result<()> {
        Ok(())
    }

    /// Time until the next scheduled refresh, for the poll timeout.
    /// `Duration::ZERO` when a refresh is due. When paused, returns a large
    /// duration instead — there is no scheduled refresh to wait for, so the
    /// caller's `poll` call ends up blocking on the next key press for all
    /// practical purposes, which is what "the caller blocks on input
    /// instead" (this method's original chunk-0 doc) means: paused doesn't
    /// special-case the caller's poll loop, it just makes a refresh never
    /// come due.
    pub fn time_until_refresh(&self) -> Duration {
        if self.paused {
            return Duration::from_secs(3600);
        }
        self.config.interval.saturating_sub(self.last_refresh.elapsed())
    }
}

fn read_identity_field(reader: &SysfsReader, path: &str) -> String {
    match reader.read_first_line(Path::new(path)) {
        Ok(Ok(line)) => sanitize_kernel_string(&line, MAX_IDENTITY_LEN),
        _ => String::new(),
    }
}

/// Backend-independent key event.
///
/// Deliberately not `crossterm::event::KeyEvent`: keeping the app's own type
/// means [`App::on_key`] is unit-testable with no terminal, and swapping the
/// backend later touches one conversion function instead of the whole event
/// handler.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyPress {
    Char(char),
    Enter,
    Esc,
    Tab,
    BackTab,
    Up,
    Down,
    Left,
    Right,
    PageUp,
    PageDown,
    /// Ctrl-C, which must exit as cleanly as `q` — restoring the terminal, not
    /// dying mid-frame.
    CtrlC,
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- Panel --------------------------------------------------------------

    #[test]
    fn next_and_prev_cycle_through_every_panel_and_wrap() {
        let mut p = Panel::Overview;
        for _ in 0..Panel::ALL.len() {
            p = p.next();
        }
        assert_eq!(p, Panel::Overview, "a full cycle of next() must return to the start");

        let mut p = Panel::Overview;
        for _ in 0..Panel::ALL.len() {
            p = p.prev();
        }
        assert_eq!(p, Panel::Overview, "a full cycle of prev() must return to the start");
    }

    #[test]
    fn next_and_prev_are_inverses() {
        for &p in &Panel::ALL {
            assert_eq!(p.next().prev(), p);
            assert_eq!(p.prev().next(), p);
        }
    }

    #[test]
    fn from_digit_covers_one_through_six_and_nothing_else() {
        assert_eq!(Panel::from_digit('1'), Some(Panel::Cpu));
        assert_eq!(Panel::from_digit('2'), Some(Panel::Memory));
        assert_eq!(Panel::from_digit('3'), Some(Panel::Thermal));
        assert_eq!(Panel::from_digit('4'), Some(Panel::Disk));
        assert_eq!(Panel::from_digit('5'), Some(Panel::Net));
        assert_eq!(Panel::from_digit('6'), Some(Panel::Gpu));
        assert_eq!(Panel::from_digit('0'), None);
        assert_eq!(Panel::from_digit('7'), None);
        assert_eq!(Panel::from_digit('a'), None);
    }

    // ---- App::new / refresh --------------------------------------------------

    fn test_config() -> Config {
        let mut c = Config::new();
        c.sysfs_root = std::env::temp_dir();
        c.only = vec!["cpu".to_string()];
        c.validate(crate::collectors::ALL).expect("temp dir is a valid sysfs_root");
        c
    }

    #[test]
    fn new_reads_identity_fields_without_erroring_even_when_absent() {
        // `std::env::temp_dir()` has no `proc/sys/kernel/hostname` under it,
        // so both fields must come back empty rather than making `App::new`
        // fail — identity is decoration, not something the whole UI should
        // refuse to start over.
        let app = App::new(test_config()).expect("App::new must not fail on missing identity files");
        assert_eq!(app.hostname, "");
        assert_eq!(app.kernel, "");
    }

    #[test]
    fn a_fresh_app_has_no_snapshot_yet() {
        let app = App::new(test_config()).expect("constructs");
        assert!(app.current.is_none());
        assert!(app.rates.is_none());
        assert!(app.history.is_empty());
    }

    #[test]
    fn time_until_refresh_is_zero_immediately_after_construction() {
        let app = App::new(test_config()).expect("constructs");
        // Backdated `last_refresh` means a refresh is already due.
        assert_eq!(app.time_until_refresh(), Duration::ZERO);
    }

    #[test]
    fn time_until_refresh_is_large_when_paused_regardless_of_the_interval() {
        let mut app = App::new(test_config()).expect("constructs");
        app.paused = true;
        assert!(app.time_until_refresh() > app.config.interval);
    }

    // ---- on_key ---------------------------------------------------------------

    #[test]
    fn q_and_esc_and_ctrl_c_all_request_quit() {
        for key in [KeyPress::Char('q'), KeyPress::Esc, KeyPress::CtrlC] {
            let mut app = App::new(test_config()).expect("constructs");
            app.on_key(key).expect("on_key must not fail");
            assert!(app.should_quit, "{key:?} did not set should_quit");
        }
    }

    #[test]
    fn space_toggles_paused() {
        let mut app = App::new(test_config()).expect("constructs");
        assert!(!app.paused);
        app.on_key(KeyPress::Char(' ')).unwrap();
        assert!(app.paused);
        app.on_key(KeyPress::Char(' ')).unwrap();
        assert!(!app.paused);
    }

    #[test]
    fn question_mark_toggles_help() {
        let mut app = App::new(test_config()).expect("constructs");
        assert!(!app.show_help);
        app.on_key(KeyPress::Char('?')).unwrap();
        assert!(app.show_help);
    }

    #[test]
    fn digit_keys_jump_focus_directly() {
        let mut app = App::new(test_config()).expect("constructs");
        app.on_key(KeyPress::Char('3')).unwrap();
        assert_eq!(app.focus, Panel::Thermal);
    }

    #[test]
    fn tab_and_backtab_cycle_focus() {
        let mut app = App::new(test_config()).expect("constructs");
        let start = app.focus;
        app.on_key(KeyPress::Tab).unwrap();
        assert_eq!(app.focus, start.next());
        app.on_key(KeyPress::BackTab).unwrap();
        assert_eq!(app.focus, start);
    }

    #[test]
    fn plus_and_minus_adjust_the_interval() {
        let mut app = App::new(test_config()).expect("constructs");
        let start = app.config.interval;
        app.on_key(KeyPress::Char('+')).unwrap();
        assert!(app.config.interval > start);
        app.on_key(KeyPress::Char('-')).unwrap();
        app.on_key(KeyPress::Char('-')).unwrap();
        assert!(app.config.interval >= crate::config::MIN_INTERVAL);
    }

    #[test]
    fn minus_never_drops_the_interval_below_the_configured_minimum() {
        let mut app = App::new(test_config()).expect("constructs");
        app.config.interval = crate::config::MIN_INTERVAL;
        for _ in 0..10 {
            app.on_key(KeyPress::Char('-')).unwrap();
        }
        assert_eq!(app.config.interval, crate::config::MIN_INTERVAL);
    }

    #[test]
    fn r_resets_the_rate_tracker() {
        let mut app = App::new(test_config()).expect("constructs");
        app.refresh().expect("first refresh");
        app.refresh().expect("second refresh");
        // Reset must not error and must be callable at any time, including
        // with no snapshots collected yet.
        app.on_key(KeyPress::Char('r')).expect("reset must not fail");
    }

    #[test]
    fn an_unbound_key_is_a_no_op() {
        let mut app = App::new(test_config()).expect("constructs");
        let focus = app.focus;
        app.on_key(KeyPress::Char('z')).unwrap();
        assert_eq!(app.focus, focus);
        assert!(!app.should_quit);
    }

    // ---- refresh + history bound ----------------------------------------------

    #[test]
    fn refresh_populates_current_and_bounds_history_to_history_len() {
        let mut config = test_config();
        config.history_len = 2;
        let mut app = App::new(config).expect("constructs");

        for _ in 0..5 {
            app.refresh().expect("refresh must not fail against a real sysfs_root");
        }

        assert!(app.current.is_some());
        assert!(app.history.len() <= 2, "history grew past history_len: {}", app.history.len());
    }
}
