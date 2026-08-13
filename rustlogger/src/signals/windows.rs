//! Windows equivalent of `signals/unix.rs`: installs a console-control
//! handler via `SetConsoleCtrlHandler` so Ctrl+C, Ctrl+Break, the console
//! window closing, the user logging off, or the system shutting down all
//! route through the session loop's clean-shutdown path, the same role
//! `sigaction`-installed `SIGINT`/`SIGHUP`/`SIGTERM` handlers play on Unix.
//!
//! Same shared-state approach as `signals/unix.rs`: the handler only
//! records *which* event arrived (in a global `AtomicU32`) and returns -
//! Windows runs console-control handlers on a dedicated OS-created thread
//! within the process rather than Unix's stricter async-signal-safe-only
//! context, but keeping the handler itself minimal avoids needing to
//! reason about what's safe to do from that thread at all.
//!
//! **Unverified beyond `cargo check`** - see `pty_session/windows.rs`'s
//! module doc comment for why. One specific behavior this can't confirm
//! without a real Windows console: for `CTRL_CLOSE_EVENT`/
//! `CTRL_LOGOFF_EVENT`/`CTRL_SHUTDOWN_EVENT` specifically, Windows only
//! gives a process a short grace period (historically ~5 seconds) after
//! the handler returns before force-terminating it regardless of what the
//! handler asked for - unlike `SIGHUP` on Unix, which imposes no such
//! OS-enforced deadline. That means `session/windows.rs`'s clean-shutdown
//! path (finishing the log, tearing down the child) may not always have
//! time to complete for those three events specifically, even though it
//! always does for `CTRL_C_EVENT`/`CTRL_BREAK_EVENT`.

use std::io;
use std::sync::atomic::{AtomicU32, Ordering};

use windows::Win32::System::Console::{
    CTRL_BREAK_EVENT, CTRL_CLOSE_EVENT, CTRL_C_EVENT, CTRL_LOGOFF_EVENT, CTRL_SHUTDOWN_EVENT,
    SetConsoleCtrlHandler,
};
use windows::core::BOOL;

use super::StopSignal;

/// `signals/unix.rs` reuses `0` as "nothing received yet", which works
/// there because `0` isn't a valid `libc` signal number. Windows'
/// `CTRL_C_EVENT` is itself `0`, so that trick would make a real
/// `CTRL_C_EVENT` indistinguishable from "nothing happened" - `u32::MAX`
/// instead, which none of the real `CTRL_*_EVENT` constants use.
const NONE_RECEIVED: u32 = u32::MAX;

static RECEIVED: AtomicU32 = AtomicU32::new(NONE_RECEIVED);

unsafe extern "system" fn record(ctrl_type: u32) -> BOOL {
    RECEIVED.store(ctrl_type, Ordering::SeqCst);
    // Tell Windows this event was handled, so it neither runs the default
    // action (which for CTRL_C_EVENT/CTRL_BREAK_EVENT would terminate the
    // process immediately, before the session loop ever sees it) nor
    // falls through to any other registered handler.
    BOOL::from(true)
}

/// Install the console-control handler.
pub fn install() -> io::Result<()> {
    // SAFETY: `record` only performs an atomic store and returns a plain
    // value, which is safe to run on the dedicated handler thread Windows
    // invokes it from.
    unsafe { SetConsoleCtrlHandler(Some(record), true).map_err(win_err_to_io) }
}

/// Returns (and clears) the most recently received console-control event,
/// if any has arrived since the last call, described platform-agnostically:
/// `session.rs` never sees a raw Windows `CTRL_*_EVENT` code. There's no
/// Windows equivalent of POSIX's `128+n` exit-code convention, so each
/// event is assigned an exit code directly here: `130` for
/// `CTRL_C_EVENT`/`CTRL_BREAK_EVENT`, matching the `128+SIGINT`
/// exit code Unix itself would use for the equivalent "user asked to
/// interrupt this" case (and the convention several cross-platform CLI
/// tools already follow on Windows for the same event); `1` for the
/// close/logoff/shutdown events, which have no equivalent Unix analogue
/// worth mimicking.
pub fn received() -> Option<StopSignal> {
    match RECEIVED.swap(NONE_RECEIVED, Ordering::SeqCst) {
        NONE_RECEIVED => None,
        CTRL_C_EVENT => Some(StopSignal {
            description: "CTRL_C_EVENT".to_string(),
            exit_code: 130,
        }),
        CTRL_BREAK_EVENT => Some(StopSignal {
            description: "CTRL_BREAK_EVENT".to_string(),
            exit_code: 130,
        }),
        CTRL_CLOSE_EVENT => Some(StopSignal {
            description: "CTRL_CLOSE_EVENT".to_string(),
            exit_code: 1,
        }),
        CTRL_LOGOFF_EVENT => Some(StopSignal {
            description: "CTRL_LOGOFF_EVENT".to_string(),
            exit_code: 1,
        }),
        CTRL_SHUTDOWN_EVENT => Some(StopSignal {
            description: "CTRL_SHUTDOWN_EVENT".to_string(),
            exit_code: 1,
        }),
        other => Some(StopSignal {
            description: format!("unknown console-control event {other}"),
            exit_code: 1,
        }),
    }
}

fn win_err_to_io(e: windows::core::Error) -> io::Error {
    io::Error::other(e.to_string())
}
