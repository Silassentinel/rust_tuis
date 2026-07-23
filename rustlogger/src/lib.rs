//! rustlogger: wraps the shell in the terminal it's launched from and
//! records the whole session (input + output) to a log file until the
//! session is stopped. See `docs/rustlogger-design.md` for the
//! architecture and `docs/TODO-rustlogger.md` for build status.
//!
//! Split into a library crate with a thin `main.rs` over it - Ch. 12 of
//! the Rust Book (`minigrep`) is the reason cited for this project's
//! structure back in chunk 1, and it pays for itself here in chunk 6:
//! the integration tests under `tests/` need to spawn the compiled
//! `rustlogger` binary itself attached to a pty, which reuses
//! `pty_session::PtySession::spawn_command` rather than duplicating the
//! `setsid`/`TIOCSCTTY`/`dup2` dance a second time.

pub mod logfile;
pub mod pty_session;
pub mod session;
pub mod signals;
pub mod stop_trigger;
pub mod terminal;
pub mod timestamp;
