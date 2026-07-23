//! Puts a terminal file descriptor into raw mode for the duration of the
//! session, restoring the original settings on drop - the same idea as
//! Ch. 9 of the Rust Book's guidance on using RAII/`Drop` rather than a
//! manual "restore" call on every exit path (including panics and the
//! early-return stop conditions chunk 4 wires up), so a crash mid-session
//! can't leave the user's real terminal stuck in raw mode.
//!
//! Raw mode (no line editing, no echo, no signal-generating special
//! characters) is what lets bytes typed by the user pass through
//! rustlogger untouched to the wrapped shell's own pty, which has its own,
//! separate terminal discipline (see `pty_session.rs`) - otherwise the
//! *outer* terminal would echo and line-buffer on top of the *inner* one
//! doing the same, garbling interactive programs.

use std::io;
use std::os::fd::BorrowedFd;

use nix::sys::termios::{self, SetArg, Termios};

/// Raw-modes the given fd on construction; restores its original
/// termios settings when dropped.
pub struct RawGuard<'fd> {
    fd: BorrowedFd<'fd>,
    original: Termios,
}

impl<'fd> RawGuard<'fd> {
    pub fn new(fd: BorrowedFd<'fd>) -> io::Result<Self> {
        let original = termios::tcgetattr(fd).map_err(nix_err_to_io)?;
        let mut raw = original.clone();
        termios::cfmakeraw(&mut raw);
        termios::tcsetattr(fd, SetArg::TCSANOW, &raw).map_err(nix_err_to_io)?;
        Ok(Self { fd, original })
    }
}

impl Drop for RawGuard<'_> {
    fn drop(&mut self) {
        // Best-effort: nothing sensible to do if restoring fails while
        // already unwinding/exiting, and this must not panic in a Drop.
        let _ = termios::tcsetattr(self.fd, SetArg::TCSANOW, &self.original);
    }
}

fn nix_err_to_io(e: nix::Error) -> io::Error {
    io::Error::from_raw_os_error(e as i32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use nix::pty::openpty;
    use nix::sys::termios::{LocalFlags, tcgetattr};
    use std::os::fd::AsFd;

    // There's no real attached tty available to a test process, but a
    // pty's slave side behaves like a genuine terminal device for
    // termios purposes (same trick `pty_session.rs`'s tests use for the
    // inner pty), so it stands in for "the outer terminal" here.
    #[test]
    fn raw_mode_clears_echo_and_canonical_flags_and_drop_restores_them() {
        let pty = openpty(None, None).expect("failed to open pty for test");
        let fd = pty.slave.as_fd();

        let before = tcgetattr(fd).expect("tcgetattr before raw mode");
        assert!(before.local_flags.contains(LocalFlags::ECHO));
        assert!(before.local_flags.contains(LocalFlags::ICANON));

        {
            let _guard = RawGuard::new(fd).expect("failed to enter raw mode");
            let during = tcgetattr(fd).expect("tcgetattr during raw mode");
            assert!(!during.local_flags.contains(LocalFlags::ECHO));
            assert!(!during.local_flags.contains(LocalFlags::ICANON));
        }

        let after = tcgetattr(fd).expect("tcgetattr after drop");
        assert!(after.local_flags.contains(LocalFlags::ECHO));
        assert!(after.local_flags.contains(LocalFlags::ICANON));
    }
}
