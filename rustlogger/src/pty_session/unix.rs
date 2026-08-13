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
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};

use nix::libc;

/// A shell running attached to a pty, plus the master side used to talk
/// to it (write = keystrokes in, read = everything the shell prints).
pub struct PtySession {
    pub child: Child,
    pub master: File,
    /// The slave side's device path (e.g. `/dev/pts/4`). Captured here
    /// because `nix::unistd::ttyname` on the *master* fd returns
    /// `/dev/ptmx` (the control device), not the slave path - the slave
    /// fd itself is the only thing that reports it, and `spawn_command`
    /// closes that fd on the parent's side once the child has its own
    /// copy, so it has to be captured before that happens.
    pub tty: String,
}

impl PtySession {
    /// Spawn `shell` (e.g. `/bin/bash`, or whatever `$SHELL` resolves to)
    /// attached to a new pty.
    pub fn spawn(shell: &str) -> io::Result<Self> {
        Self::spawn_command(Command::new(shell))
    }

    /// Like `spawn`, but takes an already-configured `Command` - lets
    /// callers set args/env/cwd before the pty wiring happens. Used by the
    /// integration tests under `tests/` to spawn the compiled
    /// `rustlogger` binary itself (rather than a plain shell) attached to
    /// a real controlling terminal.
    pub fn spawn_command(mut command: Command) -> io::Result<Self> {
        // Deliberately NOT `openpty(3)`: it has no way to create either
        // side close-on-exec, and setting `FD_CLOEXEC` afterwards leaves a
        // window in which a concurrent fork+exec on another thread still
        // inherits the fds. That window is real, not theoretical - it
        // showed up immediately as a flaky failure in this module's own
        // `wrapped_command_does_not_inherit_the_pty_master_fd` test, since
        // cargo runs tests on parallel threads. Opening both sides with
        // `O_CLOEXEC` from the start closes it with no race at all.
        let master = nix::pty::posix_openpt(
            nix::fcntl::OFlag::O_RDWR
                | nix::fcntl::OFlag::O_NOCTTY
                | nix::fcntl::OFlag::O_CLOEXEC,
        )
        .map_err(nix_err_to_io)?;
        nix::pty::grantpt(&master).map_err(nix_err_to_io)?;
        nix::pty::unlockpt(&master).map_err(nix_err_to_io)?;

        // The slave's device path (e.g. /dev/pts/4). Taken straight from
        // the master here, which is both simpler and more reliable than the
        // old `ttyname` call on the slave fd - `ttyname` on the *master*
        // reports /dev/ptmx, so this used to have to be captured from the
        // slave before that fd was dropped.
        let tty = nix::pty::ptsname_r(&master).map_err(nix_err_to_io)?;

        // O_NOCTTY: opening the slave must not make it this (parent)
        // process's controlling terminal - only the child wants that, via
        // the TIOCSCTTY ioctl in pre_exec below.
        let slave = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOCTTY | libc::O_CLOEXEC)
            .open(&tty)?;

        let slave_fd = slave.as_raw_fd();
        let master_fd = master.as_raw_fd();

        // Both sides are already close-on-exec by construction above, which
        // is what closes `.security/findings.md` RT-core-2026-07-30-03/04.
        //
        // Why it matters for the MASTER: without O_CLOEXEC the wrapped
        // shell - and transitively every process it spawns, including
        // anything deliberately dropped to a lower privilege (`sudo -u`, a
        // sandbox account, an untrusted build script) - inherits an open,
        // writable handle to it as fd 3. That breaks rustlogger in both
        // directions at once:
        //   - writing to a pty master pushes bytes into the slave's *input*
        //     queue, i.e. keystroke injection into the user's interactive
        //     shell - an unprivileged `TIOCSTI` equivalent that the kernel
        //     hardening which disabled `TIOCSTI` does not cover;
        //   - reading from it steals the child's output before rustlogger's
        //     own proxy loop sees it, so that output silently never reaches
        //     the transcript - an audit-log bypass with no gap marker.
        // Only the parent ever talks to the master (the proxy loop and the
        // logging path both live here), so nothing legitimately needs it to
        // survive exec.
        //
        // Why the SLAVE needs it too: same defect class (a stray writable
        // handle to this pty escaping into unrelated processes), plus a
        // plain correctness bug - otherwise *every* pty this process holds
        // leaks into *every* child it spawns, so a single long-lived
        // grandchild pins other sessions' slaves open and their masters
        // never reach EOF. That is not hypothetical: it showed up as a
        // 300-second hang in an unrelated test in this crate as soon as a
        // deliberately-unkillable child existed to hold the fds.
        //
        // O_CLOEXEC on the slave is compatible with `pre_exec` still using
        // it: the flag applies at exec, not fork, and `pre_exec` runs in
        // between - by then the slave has already been dup2'd onto 0/1/2
        // (dup2 clears the flag on the new descriptors) and closed.

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
                // Belt-and-braces alongside the O_CLOEXEC above: drop this
                // (forked) child's copy of the master explicitly, so it's
                // gone even on a path where the exec doesn't happen the way
                // we expect. Closing it here cannot affect the parent's own
                // master fd - after fork the two are separate descriptors
                // onto the same open file description.
                if master_fd > 2 {
                    libc::close(master_fd);
                }
                Ok(())
            });
        }

        let child = command.spawn()?;
        // The parent doesn't need the slave side once the child has it
        // (duped onto its own 0/1/2); drop it explicitly here rather than
        // relying on scope so the intent is clear at the call site.
        drop(slave);

        let master = File::from(std::os::fd::OwnedFd::from(master));
        Ok(PtySession { child, master, tty })
    }

    /// Block until the wrapped shell exits, returning its exit code.
    /// `None` means it was killed by a signal rather than exiting
    /// normally.
    pub fn wait(&mut self) -> io::Result<Option<i32>> {
        let status = self.child.wait()?;
        Ok(status.code())
    }

    /// Non-blocking counterpart to [`wait`](Self::wait): `Ok(None)` means
    /// the child is still running, `Ok(Some(code))` that it has exited and
    /// been reaped (with `code` itself `None` if a signal killed it).
    /// Needed by `session::stop_and_reap`'s escalation loop, which must
    /// never block indefinitely - see `.security/findings.md`
    /// RT-core-2026-07-30-05.
    pub fn try_wait(&mut self) -> io::Result<Option<Option<i32>>> {
        Ok(self.child.try_wait()?.map(|status| status.code()))
    }

    /// Sends `sig` to the wrapped process. Private because callers should
    /// go through the named wrappers below (or, better, through
    /// `session::stop_and_reap`, which escalates rather than sending any
    /// single signal and hoping).
    fn signal(&self, sig: nix::sys::signal::Signal) -> io::Result<()> {
        let pid = nix::unistd::Pid::from_raw(self.child.id() as i32);
        nix::sys::signal::kill(pid, sig).map_err(nix_err_to_io)
    }

    /// Asks the wrapped shell/command to gracefully stop, the same way
    /// `session.rs` needs to whenever a session ends for a reason other
    /// than the child exiting on its own (the `stoplogger` phrase, the
    /// outer terminal closing, or rustlogger itself being signaled) -
    /// sends `SIGHUP`, the same signal a real terminal hanging up would
    /// send, so nothing is left running detached from anything. This
    /// method exists on `PtySession` (rather than as a free function
    /// taking a pid, as it used to be in `session.rs`) specifically so
    /// `session.rs` doesn't need to know *how* a platform asks a process
    /// to stop - `pty_session/windows.rs` implements the same method
    /// name with Windows' own equivalent mechanism.
    pub fn terminate(&self) -> io::Result<()> {
        self.signal(nix::sys::signal::Signal::SIGHUP)
    }

    /// Second step of `session::stop_and_reap`'s escalation: `SIGTERM`,
    /// for a child that ignored or blocked `SIGHUP` (`trap '' HUP`,
    /// anything `nohup`-style). Still catchable, so it isn't the last
    /// resort - see [`kill_now`](Self::kill_now).
    pub fn terminate_forcefully(&self) -> io::Result<()> {
        self.signal(nix::sys::signal::Signal::SIGTERM)
    }

    /// Last step of the escalation: `SIGKILL`, which cannot be caught,
    /// blocked or ignored. Reached only when a child has already ignored
    /// both `SIGHUP` and `SIGTERM`; without it, rustlogger would block in
    /// `wait()` forever and strand the user's terminal in raw mode
    /// (`.security/findings.md` RT-core-2026-07-30-05).
    pub fn kill_now(&self) -> io::Result<()> {
        self.signal(nix::sys::signal::Signal::SIGKILL)
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

    // Regression guard for RT-core-2026-07-30-03/04: the pty master must
    // not survive into the wrapped command. Inheriting it gave the child
    // (and anything it spawns, at any privilege level) both a keystroke
    // injection primitive into the user's shell and a way to steal output
    // before it reached the transcript. This is the finding's own repro:
    // have the child exec and list its own fd table.
    #[test]
    fn wrapped_command_does_not_inherit_the_pty_master_fd() {
        let mut command = Command::new("/bin/sh");
        // `ls -l /proc/self/fd` after exec: any inherited fd shows up here
        // with the target it points at. A leaked master reads as /dev/ptmx.
        command.arg("-c").arg("ls -l /proc/self/fd; exit 0");
        let mut session =
            PtySession::spawn_command(command).expect("failed to spawn the fd-listing command");

        let master_fd_in_parent = session.master.as_raw_fd();
        let mut output = Vec::new();
        let _ = session.master.read_to_end(&mut output);
        session.wait().expect("failed to wait on child");

        let listing = String::from_utf8_lossy(&output);
        assert!(
            !listing.contains("ptmx"),
            "the wrapped command inherited the pty master (shows as /dev/ptmx) - \
             keystroke injection and log evasion are both open again. fd listing:\n{listing}"
        );
        // Sanity check that the probe actually produced a plausible fd
        // number, so a listing that's empty for some unrelated reason
        // can't make this test vacuously pass.
        assert!(
            master_fd_in_parent > 2,
            "probe pty master landed on an implausible fd: {master_fd_in_parent}"
        );
        assert!(
            listing.contains(" 0 -> ") || listing.contains("0 ->"),
            "expected a real fd listing from the child, got: {listing:?}"
        );
    }

    #[test]
    fn terminate_stops_a_still_running_child() {
        let mut session = PtySession::spawn("/bin/sh").expect("failed to spawn shell in pty");

        session.terminate().expect("failed to terminate the child");

        let code = session.wait().expect("failed to wait on child");
        assert_eq!(
            code, None,
            "expected the shell to be killed by SIGHUP (no exit code), got: {code:?}"
        );
    }

    #[test]
    fn spawn_command_carries_extra_args_and_env() {
        let mut command = Command::new("/bin/sh");
        command.arg("-c").arg("echo \"$GREETING\" \"$1\"").arg("--").arg("world");
        command.env("GREETING", "hello");
        let mut session =
            PtySession::spawn_command(command).expect("failed to spawn /bin/sh with args/env");

        let mut output = Vec::new();
        let _ = session.master.read_to_end(&mut output);
        let code = session.wait().expect("failed to wait on child");
        assert_eq!(code, Some(0));

        let text = String::from_utf8_lossy(&output);
        assert!(
            text.contains("hello world"),
            "expected args and env to reach the spawned command, got: {text:?}"
        );
    }

    #[test]
    fn tty_path_looks_like_a_real_pty_device() {
        let mut session = PtySession::spawn("/bin/sh").expect("failed to spawn shell in pty");
        assert!(
            session.tty.starts_with("/dev/"),
            "expected a real device path, got: {:?}",
            session.tty
        );
        session.terminate().expect("failed to terminate the child");
        let _ = session.wait();
    }
}
