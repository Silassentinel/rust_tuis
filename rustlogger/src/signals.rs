//! Installs handlers for `SIGINT`, `SIGHUP` and `SIGTERM` on the rustlogger
//! process itself, so Ctrl+C reaching the *outer* terminal (before it's put
//! into raw mode, or delivered directly rather than as a byte), the
//! controlling terminal hanging up (window/tab closed), and an ordinary
//! `kill` all funnel through the same clean-shutdown path in chunk 4's
//! session loop instead of the default dispositions (which would tear the
//! process down before the outer terminal mode is restored or the wrapped
//! shell is dealt with).
//!
//! The handler only records *that* a signal arrived (in a global, since
//! signal handlers are process-wide, not per-thread) and returns
//! immediately - anything more (touching a `Termios`, writing to files) is
//! not async-signal-safe. The session loop notices via `poll()` returning
//! `EINTR` (see Ch. 16 of the Rust Book on shared state between the signal
//! handler and the main loop, here an `AtomicI32` standing in for a
//! mutex/channel since only a single `store`/`load` is needed) and calls
//! `received()` to find out which one.

use std::sync::atomic::{AtomicI32, Ordering};

use nix::libc;
use nix::sys::signal::{self, SaFlags, SigAction, SigHandler, SigSet, Signal};

static RECEIVED: AtomicI32 = AtomicI32::new(0);

extern "C" fn record(signal: libc::c_int) {
    RECEIVED.store(signal, Ordering::SeqCst);
}

/// Install handlers for `SIGINT`/`SIGHUP`/`SIGTERM`. Deliberately does not
/// set `SA_RESTART`, so that a blocking `poll()` in the session loop is
/// interrupted (returns `EINTR`) the moment one of these arrives, rather
/// than transparently resuming as if nothing happened.
pub fn install() -> nix::Result<()> {
    let action = SigAction::new(SigHandler::Handler(record), SaFlags::empty(), SigSet::empty());
    // SAFETY: `record` only performs an atomic store, which is
    // async-signal-safe; installing it as the handler for these three
    // signals is otherwise a plain `sigaction(2)` call.
    unsafe {
        signal::sigaction(Signal::SIGINT, &action)?;
        signal::sigaction(Signal::SIGHUP, &action)?;
        signal::sigaction(Signal::SIGTERM, &action)?;
    }
    Ok(())
}

/// Returns (and clears) the most recently received signal, if any of
/// `SIGINT`/`SIGHUP`/`SIGTERM` has arrived since the last call.
pub fn received() -> Option<Signal> {
    match RECEIVED.swap(0, Ordering::SeqCst) {
        0 => None,
        raw => Signal::try_from(raw).ok(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // All three signals share process-global state (the handler table and
    // `RECEIVED`), so they're exercised sequentially in one test rather
    // than split across tests that the harness might run concurrently.
    #[test]
    fn records_each_installed_signal_and_then_clears() {
        install().expect("failed to install signal handlers");

        assert_eq!(received(), None);

        signal::raise(Signal::SIGTERM).expect("failed to raise SIGTERM");
        assert_eq!(received(), Some(Signal::SIGTERM));
        assert_eq!(received(), None, "received() should clear the flag");

        signal::raise(Signal::SIGHUP).expect("failed to raise SIGHUP");
        assert_eq!(received(), Some(Signal::SIGHUP));

        signal::raise(Signal::SIGINT).expect("failed to raise SIGINT");
        assert_eq!(received(), Some(Signal::SIGINT));
    }
}
