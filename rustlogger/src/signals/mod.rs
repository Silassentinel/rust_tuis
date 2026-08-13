//! Installs handlers so Ctrl+C, the terminal/console closing, and being
//! killed all route through the session loop's clean-shutdown path
//! instead of the platform's default disposition. Two independent
//! implementations selected by `cfg(unix)`/`cfg(windows)` - POSIX signals
//! (`SIGINT`/`SIGHUP`/`SIGTERM`) and Windows console-control events
//! (`CTRL_C_EVENT`/`CTRL_CLOSE_EVENT`/...) are different mechanisms
//! entirely, but both are exposed to `session.rs` through the same
//! `install`/`received` shape, returning a platform-agnostic
//! [`StopSignal`].

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::*;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::*;

/// What was received, described platform-agnostically for the log
/// footer, plus the exit code `session.rs` should use for this stop
/// reason - computed here (per platform) rather than in `session.rs`,
/// since "what's a sensible process exit code for having been signaled"
/// is itself a platform convention (POSIX's `128+n`; Windows has no
/// equivalent convention worth mimicking).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StopSignal {
    pub description: String,
    pub exit_code: i32,
}
