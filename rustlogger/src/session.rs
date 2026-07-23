//! Ties the pieces from chunks 2-3 together: raw-modes the outer terminal,
//! proxies bytes both ways between it and the wrapped shell's pty, and
//! wires in the three stop conditions from `docs/rustlogger-design.md` -
//! the `stoplogger` phrase, the wrapped shell exiting (any code), and
//! rustlogger itself receiving `SIGINT`/`SIGHUP`/`SIGTERM`.
//!
//! The proxy loop is structurally the same request/response-style loop as
//! the Rust Book's Ch. 20 final project (read from one side, react, write
//! to the other, repeat) with the two directions multiplexed via `poll()`
//! (Ch. 16's discussion of shared state/concurrency applies here too, just
//! via a single thread plus `poll()` instead of a reader/writer thread
//! pair, since both directions are small, interactive byte streams rather
//! than independent long-running transfers).

use std::fmt;
use std::fs::File;
use std::io::{self, BufWriter, Read, Write};
use std::os::fd::AsFd;
use std::time::SystemTime;

use nix::libc;
use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
use nix::sys::signal::{self, Signal};
use nix::unistd::Pid;

use crate::logfile::LogFile;
use crate::pty_session::PtySession;
use crate::signals;
use crate::stop_trigger::StopTrigger;
use crate::terminal::RawGuard;
use crate::timestamp::format_utc_compact;

const BUF_SIZE: usize = 4096;

/// Why the session loop stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    /// The user typed the `stoplogger` trigger phrase.
    StopPhrase,
    /// The wrapped shell exited on its own (the pty's master side hit EOF).
    ChildExited,
    /// The outer terminal's input closed from under us - not one of the
    /// documented stop conditions, but reading 0 bytes forever in a loop
    /// would spin, so it's treated as an implicit "session ends any other
    /// way" case.
    OuterClosed,
    /// rustlogger itself received `SIGINT`/`SIGHUP`/`SIGTERM`.
    Signal(Signal),
}

impl fmt::Display for StopReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StopReason::StopPhrase => write!(f, "stoplogger command"),
            StopReason::ChildExited => write!(f, "wrapped shell exited"),
            StopReason::OuterClosed => write!(f, "outer terminal input closed"),
            StopReason::Signal(sig) => write!(f, "caught signal {sig}"),
        }
    }
}

/// Copies bytes both ways between the outer terminal (`outer_in`/`outer_out`)
/// and the wrapped shell's pty (`master`) until one of the stop conditions
/// fires. Generic over the outer terminal's reader/writer, rather than
/// hardcoded to `Stdin`/`Stdout`, so tests can stand in a pipe pair for "the
/// real terminal" without one actually attached to the test process - the
/// same reasoning `pty_session.rs`'s tests use for the wrapped shell's side.
///
/// Note: the `Signal` stop reason is reached via `poll()` returning `EINTR`,
/// which requires an actual signal to interrupt a blocked syscall. That's
/// exercised manually and via `signals::tests` (which verifies the
/// record/clear mechanism directly); it isn't re-tested here with a real
/// concurrently-delivered signal because `sigaction` state and the
/// `signals` module's flag are process-global, and cargo runs test
/// functions on parallel threads within one process - a second test
/// raising the same signals at an arbitrary time would race with it.
pub fn proxy_loop<R, W, LW>(
    outer_in: &mut R,
    outer_out: &mut W,
    master: &mut File,
    trigger: &mut StopTrigger,
    log: &mut LogFile<LW>,
) -> io::Result<StopReason>
where
    R: Read + AsFd,
    W: Write,
    LW: Write,
{
    let mut buf = [0u8; BUF_SIZE];

    loop {
        let outer_ready;
        let master_ready;
        {
            let mut fds = [
                PollFd::new(outer_in.as_fd(), PollFlags::POLLIN),
                PollFd::new(master.as_fd(), PollFlags::POLLIN),
            ];
            match poll(&mut fds, PollTimeout::NONE) {
                Ok(_) => {}
                Err(nix::Error::EINTR) => {
                    if let Some(sig) = signals::received() {
                        return Ok(StopReason::Signal(sig));
                    }
                    continue;
                }
                Err(e) => return Err(io::Error::from_raw_os_error(e as i32)),
            }
            outer_ready = fds[0].any().unwrap_or(false);
            master_ready = fds[1].any().unwrap_or(false);
        }

        if outer_ready {
            let n = outer_in.read(&mut buf)?;
            if n == 0 {
                return Ok(StopReason::OuterClosed);
            }
            let chunk = &buf[..n];
            master.write_all(chunk)?;
            if trigger.feed(chunk) {
                return Ok(StopReason::StopPhrase);
            }
        }

        if master_ready {
            // Once the child has closed every fd pointing at the pty's
            // slave side (typically because it exited), Linux reports
            // that on the master by failing the *next* read with `EIO`
            // rather than returning a clean 0-byte EOF like a pipe would -
            // a longstanding BSD-pty quirk Linux kept for compatibility.
            let n = match master.read(&mut buf) {
                Ok(0) => return Ok(StopReason::ChildExited),
                Ok(n) => n,
                Err(e) if e.raw_os_error() == Some(libc::EIO) => {
                    return Ok(StopReason::ChildExited);
                }
                Err(e) => return Err(e),
            };
            outer_out.write_all(&buf[..n])?;
            outer_out.flush()?;
            log.write_output(&buf[..n])?;
        }
    }
}

/// Runs a full session: spawns `shell` in a pty, raw-modes the real stdin,
/// proxies until a stop condition fires, logs the whole thing, and
/// returns the exit code rustlogger itself should exit with.
pub fn run(shell: &str) -> io::Result<i32> {
    signals::install().map_err(nix_err_to_io)?;

    let mut session = PtySession::spawn(shell)?;
    let mut trigger = StopTrigger::new();

    let started_at = SystemTime::now();
    let log_path = format!("rustlogger-{}.log", format_utc_compact(started_at));
    let tty = nix::unistd::ttyname(io::stdin())
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "unknown".to_string());
    let mut log = LogFile::new(
        BufWriter::new(File::create(&log_path)?),
        shell,
        &tty,
        started_at,
    )?;
    eprintln!("rustlogger: logging session to {log_path}");

    let stdin = io::stdin();
    let stdout = io::stdout();
    // Borrows `stdin` (not `stdin_lock`) so the guard's borrow and the
    // lock's borrow are independent - both are shared borrows of `stdin`
    // itself, rather than the guard borrowing the lock, which would keep
    // it alive (and stdin un-mutably-borrowable) for as long as the guard
    // is in scope.
    let _raw_guard = RawGuard::new(stdin.as_fd())?;
    let reason = {
        let mut stdin_lock = stdin.lock();
        let mut stdout_lock = stdout.lock();
        proxy_loop(
            &mut stdin_lock,
            &mut stdout_lock,
            &mut session.master,
            &mut trigger,
            &mut log,
        )?
    };

    let (exit_code, child_code) = match reason {
        StopReason::ChildExited => {
            let code = session.wait()?;
            (code.unwrap_or(1), code)
        }
        StopReason::StopPhrase => {
            terminate_child(&session)?;
            (0, session.wait()?)
        }
        StopReason::OuterClosed => {
            terminate_child(&session)?;
            (1, session.wait()?)
        }
        StopReason::Signal(sig) => {
            terminate_child(&session)?;
            (128 + sig as i32, session.wait()?)
        }
    };

    log.finish(&reason.to_string(), child_code, SystemTime::now())?;

    Ok(exit_code)
}

/// Sends `SIGHUP` to the wrapped shell - the same signal it would get from
/// a real terminal hanging up - so ending the rustlogger session doesn't
/// leave the shell running, detached from anything, connected to a pty
/// nobody is proxying any more.
fn terminate_child(session: &PtySession) -> io::Result<()> {
    let pid = Pid::from_raw(session.child.id() as i32);
    signal::kill(pid, Signal::SIGHUP).map_err(nix_err_to_io)
}

fn nix_err_to_io(e: nix::Error) -> io::Error {
    io::Error::from_raw_os_error(e as i32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::pipe;

    fn test_log() -> LogFile<Vec<u8>> {
        LogFile::new(Vec::new(), "/bin/sh", "test", SystemTime::UNIX_EPOCH)
            .expect("failed to build test log")
    }

    #[test]
    fn stop_phrase_ends_the_loop_immediately() {
        let mut session = PtySession::spawn("/bin/sh").expect("failed to spawn shell");
        let (mut outer_in_reader, mut outer_in_writer) = pipe().expect("failed to create pipe");
        let (_outer_out_reader, mut outer_out_writer) = pipe().expect("failed to create pipe");

        outer_in_writer
            .write_all(b"stoplogger\n")
            .expect("failed to write trigger phrase");

        let mut trigger = StopTrigger::new();
        let mut log = test_log();
        let reason = proxy_loop(
            &mut outer_in_reader,
            &mut outer_out_writer,
            &mut session.master,
            &mut trigger,
            &mut log,
        )
        .expect("proxy_loop failed");

        assert_eq!(reason, StopReason::StopPhrase);

        terminate_child(&session).expect("failed to send SIGHUP to child");
        session.wait().expect("failed to reap child");
    }

    #[test]
    fn master_eof_reports_child_exited_with_its_code() {
        let mut session = PtySession::spawn("/bin/sh").expect("failed to spawn shell");
        // Kept alive so the outer-input side isn't seen as closed: only the
        // pty master should hit EOF in this test.
        let (mut outer_in_reader, _outer_in_writer) = pipe().expect("failed to create pipe");
        let (_outer_out_reader, mut outer_out_writer) = pipe().expect("failed to create pipe");

        session
            .master
            .write_all(b"exit 7\n")
            .expect("failed to write exit to pty master");

        let mut trigger = StopTrigger::new();
        let mut log = test_log();
        let reason = proxy_loop(
            &mut outer_in_reader,
            &mut outer_out_writer,
            &mut session.master,
            &mut trigger,
            &mut log,
        )
        .expect("proxy_loop failed");

        assert_eq!(reason, StopReason::ChildExited);
        assert_eq!(session.wait().expect("failed to reap child"), Some(7));

        let logged = String::from_utf8(log.into_writer()).unwrap();
        assert!(
            logged.contains("exit 7"),
            "expected the echoed shell output in the log, got: {logged:?}"
        );
    }

    #[test]
    fn outer_input_eof_is_reported_without_touching_the_child() {
        let mut session = PtySession::spawn("/bin/sh").expect("failed to spawn shell");
        let (mut outer_in_reader, outer_in_writer) = pipe().expect("failed to create pipe");
        drop(outer_in_writer); // simulate the outer terminal's input closing
        let (_outer_out_reader, mut outer_out_writer) = pipe().expect("failed to create pipe");

        let mut trigger = StopTrigger::new();
        let mut log = test_log();
        let reason = proxy_loop(
            &mut outer_in_reader,
            &mut outer_out_writer,
            &mut session.master,
            &mut trigger,
            &mut log,
        )
        .expect("proxy_loop failed");

        assert_eq!(reason, StopReason::OuterClosed);

        terminate_child(&session).expect("failed to send SIGHUP to child");
        session.wait().expect("failed to reap child");
    }
}
