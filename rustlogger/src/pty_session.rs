//! Spawns a shell attached to a freshly created pseudo-terminal (pty) -
//! the same fundamental mechanism the classic `script` command uses, so
//! interactive programs (`sudo`, `apt`, anything checking `isatty()`)
//! behave exactly as they would in a normal terminal.
//!
//! This module only deals with the *inner* pty pair (the one the wrapped
//! shell runs in) - it does not touch the real/outer terminal at all, so
//! it can be exercised in tests without a real tty attached to the test
//! process itself. Putting the *outer* terminal into raw mode and
//! proxying bytes between the two is chunk 4's job (see
//! `docs/TODO-rustlogger.md`).

use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};

use nix::libc;
use nix::pty::openpty;

/// A shell running attached to a pty, plus the master side used to talk
/// to it (write = keystrokes in, read = everything the shell prints).
pub struct PtySession {
    pub child: Child,
    pub master: File,
}

impl PtySession {
    /// Spawn `shell` (e.g. `/bin/bash`, or whatever `$SHELL` resolves to)
    /// attached to a new pty.
    pub fn spawn(shell: &str) -> io::Result<Self> {
        let pty = openpty(None, None).map_err(nix_err_to_io)?;
        let slave_fd = pty.slave.as_raw_fd();

        let mut command = Command::new(shell);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        // SAFETY: setsid(2), ioctl(2), dup2(2) and close(2) are all
        // async-signal-safe, so it's sound to call them here between
        // fork and exec (this closure runs in the forked child only,
        // before it execs into the target shell).
        unsafe {
            command.pre_exec(move || {
                if libc::setsid() == -1 {
                    return Err(io::Error::last_os_error());
                }
                // Make the pty's slave our controlling terminal, which is
                // what lets job control, Ctrl+C-as-SIGINT, etc. work
                // inside the wrapped shell the same as a normal terminal.
                if libc::ioctl(slave_fd, libc::TIOCSCTTY as _, 0) == -1 {
                    return Err(io::Error::last_os_error());
                }
                if libc::dup2(slave_fd, 0) == -1
                    || libc::dup2(slave_fd, 1) == -1
                    || libc::dup2(slave_fd, 2) == -1
                {
                    return Err(io::Error::last_os_error());
                }
                if slave_fd > 2 {
                    libc::close(slave_fd);
                }
                Ok(())
            });
        }

        let child = command.spawn()?;
        // The parent doesn't need the slave side once the child has it
        // (duped onto its own 0/1/2); drop it explicitly here rather than
        // relying on scope so the intent is clear at the call site.
        drop(pty.slave);

        let master = File::from(pty.master);
        Ok(PtySession { child, master })
    }

    /// Block until the wrapped shell exits, returning its exit code.
    /// `None` means it was killed by a signal rather than exiting
    /// normally.
    pub fn wait(&mut self) -> io::Result<Option<i32>> {
        let status = self.child.wait()?;
        Ok(status.code())
    }
}

/// `nix::Error` (an `Errno`) shares its discriminant values with the C
/// `errno` numbers, so this round-trips it into a normal `io::Error`
/// without pulling in an extra conversion crate.
fn nix_err_to_io(e: nix::Error) -> io::Error {
    io::Error::from_raw_os_error(e as i32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    #[test]
    fn runs_a_command_and_captures_its_output() {
        let mut session =
            PtySession::spawn("/bin/sh").expect("failed to spawn shell in pty");

        session
            .master
            .write_all(b"echo hello-from-pty\n")
            .expect("failed to write to pty master");
        session
            .master
            .write_all(b"exit 3\n")
            .expect("failed to write exit to pty master");

        let mut output = Vec::new();
        // The pty echoes input back plus the command's own output;
        // reading to EOF (which happens once the shell exits and its
        // side of the pty closes) captures everything.
        let _ = session.master.read_to_end(&mut output);

        let code = session.wait().expect("failed to wait on child");
        assert_eq!(code, Some(3));

        let text = String::from_utf8_lossy(&output);
        assert!(
            text.contains("hello-from-pty"),
            "expected pty output to contain the echoed text, got: {text:?}"
        );
    }
}
