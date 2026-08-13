//! Puts the outer terminal into raw mode for the session's duration,
//! restoring it on drop. Two independent implementations selected by
//! `cfg(unix)`/`cfg(windows)` - Unix's termios flags (`ECHO`, `ICANON`,
//! `ISIG`, ...) and Windows' console mode flags (`ENABLE_ECHO_INPUT`,
//! `ENABLE_LINE_INPUT`, `ENABLE_PROCESSED_INPUT`, ...) are conceptually
//! the same knob, but there's no shared API to reach either from `std`.

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::*;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::*;
