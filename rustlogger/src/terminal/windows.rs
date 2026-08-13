//! Windows equivalent of `terminal/unix.rs`: puts the outer console's
//! input mode into the raw-mode-equivalent state for the session's
//! duration, restoring it on drop, via the Win32 Console API's
//! `GetConsoleMode`/`SetConsoleMode` rather than termios.
//!
//! There's no single "raw mode" flag on Windows - it's the same idea as
//! Unix's `cfmakeraw` (clear the flags that make the console line-buffer
//! and echo input itself, `session/windows.rs`'s proxy loop needs every
//! keystroke immediately and unmodified), assembled from the individual
//! `ENABLE_*` bits `cfmakeraw` bundles in one call on Unix:
//! - clear `ENABLE_ECHO_INPUT` (the console stops echoing typed characters -
//!   the wrapped shell's own pty/console does its own echo, same reason
//!   `terminal/unix.rs` clears `ECHO`)
//! - clear `ENABLE_LINE_INPUT` (stop buffering a whole line before handing
//!   it over - same role as clearing `ICANON`)
//! - clear `ENABLE_PROCESSED_INPUT` (stop the console intercepting Ctrl+C
//!   itself and turning it into a signal - `signals/windows.rs`'s
//!   `SetConsoleCtrlHandler` is how rustlogger wants to observe that
//!   instead, mirroring why `terminal/unix.rs` also implicitly gives up
//!   `ISIG`-driven signal generation on the outer fd)
//! - set `ENABLE_VIRTUAL_TERMINAL_INPUT` (ANSI/VT escape sequences for
//!   special keys arrive as raw byte sequences rather than a separate
//!   input-record format, so they can be logged and forwarded as plain
//!   bytes the same way an ANSI sequence from a Unix pty already is)
//!
//! **Unverified beyond `cargo check`** - see `pty_session/windows.rs`'s
//! module doc comment for why (no Windows machine available while writing
//! this).

use std::io;

use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Console::{
    CONSOLE_MODE, ENABLE_ECHO_INPUT, ENABLE_LINE_INPUT, ENABLE_PROCESSED_INPUT,
    ENABLE_VIRTUAL_TERMINAL_INPUT, GetConsoleMode, GetStdHandle, STD_INPUT_HANDLE, SetConsoleMode,
};

/// Raw-modes the console's input handle on construction; restores its
/// original mode when dropped. Unlike `terminal/unix.rs`'s `RawGuard`,
/// this doesn't borrow a caller-supplied fd - `GetStdHandle` fetches the
/// process's own console input handle directly, since Windows has no
/// equivalent notion of "raw-mode this arbitrary fd" the way a pty slave's
/// fd can be passed to `tcsetattr`.
pub struct RawGuard {
    handle: HANDLE,
    original: CONSOLE_MODE,
}

impl RawGuard {
    pub fn new() -> io::Result<Self> {
        unsafe {
            let handle = GetStdHandle(STD_INPUT_HANDLE).map_err(win_err_to_io)?;

            let mut original = CONSOLE_MODE(0);
            GetConsoleMode(handle, &mut original).map_err(win_err_to_io)?;

            let raw = (original
                & !(ENABLE_ECHO_INPUT | ENABLE_LINE_INPUT | ENABLE_PROCESSED_INPUT))
                | ENABLE_VIRTUAL_TERMINAL_INPUT;
            SetConsoleMode(handle, raw).map_err(win_err_to_io)?;

            Ok(Self { handle, original })
        }
    }
}

impl Drop for RawGuard {
    fn drop(&mut self) {
        // Best-effort, same reasoning as terminal/unix.rs's Drop: nothing
        // sensible to do with a failure here, and Drop must not panic.
        unsafe {
            let _ = SetConsoleMode(self.handle, self.original);
        }
    }
}

/// `windows::core::Error` (re-exported from `windows-result`) implements
/// `Display` with a human-readable HRESULT message - round-tripped through
/// that rather than a numeric code, matching the existing
/// `to_io_error`-style per-module helpers (see `pty_session/windows.rs`).
fn win_err_to_io(e: windows::core::Error) -> io::Error {
    io::Error::other(e.to_string())
}
