//! Live terminal UI. Entirely behind the `tui` feature.
//!
//! # Two things that are this module's responsibility, not a follow-up
//!
//! 1. **Terminal restoration on panic.** If the process dies with the terminal
//!    in raw mode and the alternate screen active, the user's shell is left
//!    unusable. `rustlogger/src/terminal/` solves the same problem with a
//!    `Drop` guard; the reasoning transfers even though the code doesn't
//!    (different crate, different backend). A `Drop` guard alone is not enough
//!    — a panic during drawing needs a panic hook too. Here both come from
//!    `ratatui::try_init`/`restore`: the panic hook is installed by
//!    `try_init` itself, and [`TerminalGuard`]'s `Drop` covers the
//!    non-panicking exit paths (`should_quit`, or an `Err` bubbling up
//!    through the event loop). Building on `ratatui`'s own blessed
//!    init/restore pair rather than hand-rolling raw `crossterm` calls: it
//!    already solves exactly this problem, correctly, and re-solving it here
//!    would just be a second, unpracticed implementation of the same thing.
//! 2. **Sanitising kernel strings before drawing.** Device and sensor names are
//!    already sanitised at the collector boundary
//!    ([`crate::sysfs::sanitize_kernel_string`]), and this module must not
//!    render any string that didn't come through a [`crate::sample::Snapshot`].
//!    A crafted device name reaching the terminal raw can rewrite the screen.
//!    The one string [`app::App`] reads outside a `Snapshot` — the hostname
//!    and kernel release for the header — goes through the same
//!    `sanitize_kernel_string` call every collector uses, at the point it's
//!    read (`app.rs`), not here.

pub mod app;
pub mod layout;
pub mod widgets;

use std::io;
use std::path::PathBuf;

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout as RLayout, Rect as RRect};
use ratatui::{DefaultTerminal, Frame};

use crate::config::Config;
use crate::error::{Error, Result};
use crate::sample::Snapshot;

use app::{App, KeyPress};

/// Run the TUI until the user quits. Returns the process exit code.
pub fn run(config: &Config) -> Result<i32> {
    let mut guard = enter_terminal()?;
    let mut app = App::new(config.clone())?;

    let result = run_loop(guard.terminal_mut(), &mut app);

    // `guard` is dropped here regardless of how `run_loop` returned —
    // including on `Err`, via the `?` below — which is exactly what
    // restores the terminal on every exit path. See this module's doc.
    result?;
    Ok(0)
}

fn run_loop(terminal: &mut DefaultTerminal, app: &mut App) -> Result<()> {
    // The first frame shows real data immediately rather than a blank
    // screen for one whole interval — `App::new` backdates `last_refresh`
    // for exactly this, but a refresh still has to actually happen once.
    app.refresh()?;

    while !app.should_quit {
        // Non-blocking: folds in whatever DNS results have arrived since
        // the last iteration before drawing, so a resolved domain shows up
        // as soon as it's ready rather than waiting for the next refresh.
        app.drain_enrichment();

        terminal.draw(|frame| draw_frame(frame, app)).map_err(io_err)?;

        let timeout = app.time_until_refresh();
        if event::poll(timeout).map_err(io_err)? {
            match event::read().map_err(io_err)? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    if let Some(mapped) = map_key(key) {
                        app.on_key(mapped)?;
                    }
                }
                Event::Resize(width, height) => app.on_resize(width, height)?,
                _ => {}
            }
        } else if !app.paused {
            // Timed out with no input: this is what "a refresh is due"
            // (`time_until_refresh` returned `Duration::ZERO`) looks like
            // from here. Paused never reaches this branch in practice —
            // its timeout is long enough that a key press almost always
            // wins the race — but the `!app.paused` guard is the actual
            // contract, not an accident of the timeout's length.
            app.refresh()?;
        }
    }

    Ok(())
}

fn map_key(key: crossterm::event::KeyEvent) -> Option<KeyPress> {
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        return Some(KeyPress::CtrlC);
    }

    match key.code {
        KeyCode::Char(c) => Some(KeyPress::Char(c)),
        KeyCode::Enter => Some(KeyPress::Enter),
        KeyCode::Esc => Some(KeyPress::Esc),
        KeyCode::Tab => Some(KeyPress::Tab),
        KeyCode::BackTab => Some(KeyPress::BackTab),
        KeyCode::Up => Some(KeyPress::Up),
        KeyCode::Down => Some(KeyPress::Down),
        KeyCode::Left => Some(KeyPress::Left),
        KeyCode::Right => Some(KeyPress::Right),
        KeyCode::PageUp => Some(KeyPress::PageUp),
        KeyCode::PageDown => Some(KeyPress::PageDown),
        _ => None,
    }
}

fn draw_frame(frame: &mut Frame, app: &App) {
    let area = frame.area();
    let presence = app.current.as_ref().map(presence_of).unwrap_or_default();
    let computed = layout::compute(area.width, area.height, presence);

    // Only possible if the very first `refresh()` in `run_loop` failed and
    // this got called anyway (it can't — that `?` would have already
    // returned) — guarded regardless, so this function can never index into
    // an absent snapshot.
    let Some(snapshot) = &app.current else {
        return;
    };

    if let Some(rect) = computed.header {
        widgets::draw_header(frame, rect, snapshot, &app.hostname, &app.kernel, app.config.interval, app.paused);
    }
    if let Some(rect) = computed.cpu {
        widgets::draw_cpu(frame, rect, snapshot, app.rates.as_ref(), app.history.as_slices().0);
    }
    if let Some(rect) = computed.memory {
        widgets::draw_memory(frame, rect, snapshot);
    }
    if let Some(rect) = computed.thermal {
        widgets::draw_thermal(frame, rect, snapshot);
    }
    if let Some(rect) = computed.disk {
        widgets::draw_disk(frame, rect, snapshot, app.rates.as_ref());
    }
    if let Some(rect) = computed.net {
        widgets::draw_net(frame, rect, snapshot, app.rates.as_ref());
    }
    if let Some(rect) = computed.gpu {
        widgets::draw_gpu(frame, rect, snapshot);
    }
    if let Some(rect) = computed.connections {
        widgets::draw_connections(
            frame,
            rect,
            snapshot,
            &app.connections_rows(),
            app.connections_cursor,
            &app.enrichment,
        );
    }
    if let Some(rect) = computed.footer {
        widgets::draw_footer(frame, rect, snapshot, app.config.verbose);
    }

    if app.show_help {
        let help = centered_rect(area, 50, 60);
        widgets::draw_help(
            frame,
            layout::Rect {
                x: help.x,
                y: help.y,
                width: help.width,
                height: help.height,
            },
        );
    }
}

fn presence_of(snapshot: &Snapshot) -> layout::PanelPresence {
    layout::PanelPresence {
        thermal: snapshot.thermal.as_ref().is_some_and(|t| !t.chips.is_empty()),
        disk: snapshot
            .disks
            .as_ref()
            .is_some_and(|d| !d.devices.is_empty() || !d.mounts.is_empty()),
        net: snapshot.net.as_ref().is_some_and(|n| !n.interfaces.is_empty()),
        gpu: snapshot.gpus.as_ref().is_some_and(|g| !g.gpus.is_empty()),
        connections: snapshot.connections.as_ref().is_some_and(|c| !c.connections.is_empty()),
    }
}

/// A rect covering `percent_x`% × `percent_y`% of `area`, centred — the
/// standard ratatui recipe for a popup overlay. Only used for the help
/// screen, so it lives here rather than in [`layout`]: [`layout::compute`]'s
/// contract is specifically about the six data panels plus header/footer,
/// not arbitrary popups.
fn centered_rect(area: RRect, percent_x: u16, percent_y: u16) -> RRect {
    let vertical = RLayout::vertical([
        Constraint::Percentage((100 - percent_y) / 2),
        Constraint::Percentage(percent_y),
        Constraint::Percentage((100 - percent_y) / 2),
    ])
    .split(area);
    RLayout::horizontal([
        Constraint::Percentage((100 - percent_x) / 2),
        Constraint::Percentage(percent_x),
        Constraint::Percentage((100 - percent_x) / 2),
    ])
    .split(vertical[1])[1]
}

/// Set up raw mode + alternate screen, and install the panic hook that undoes
/// them. Returns a guard that restores on `Drop`.
pub fn enter_terminal() -> Result<TerminalGuard> {
    let terminal = ratatui::try_init().map_err(io_err)?;
    Ok(TerminalGuard { terminal })
}

/// Restores the terminal on drop, including when unwinding from a panic.
///
/// The panic case is actually handled by the hook `ratatui::try_init`
/// installs — it runs during unwind, before this `Drop` impl would. This
/// `Drop` covers the normal-exit and early-`Err`-return paths, neither of
/// which panics at all.
pub struct TerminalGuard {
    terminal: DefaultTerminal,
}

impl TerminalGuard {
    fn terminal_mut(&mut self) -> &mut DefaultTerminal {
        &mut self.terminal
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        // `ratatui::restore()` is the "must not panic" half of this
        // contract: it prints any failure to stderr and swallows it rather
        // than propagating, which is exactly right for a `Drop` impl.
        ratatui::restore();
    }
}

/// Same reasoning as `render::json`'s/`render::text`'s `io_err`: there's no
/// real file path for a terminal I/O failure, so `Error::Io` gets a
/// synthetic one.
fn io_err(source: io::Error) -> Error {
    Error::Io {
        path: PathBuf::from("<terminal>"),
        source,
    }
}
