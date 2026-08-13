//! Spawns the wrapped shell/command attached to a pseudo-terminal, and
//! exposes a way to ask it to gracefully stop. Two independent
//! implementations selected by `cfg(unix)`/`cfg(windows)` - not a port,
//! since neither the pty mechanism (`openpty` vs ConPTY) nor "ask a
//! process to gracefully stop" (`SIGHUP` vs a Windows console-control
//! event) exist in a common form across both. See
//! `docs/rustlogger-design.md`'s chunk 9 notes for why.

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::*;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::*;
