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
//!
//! `poll()` is POSIX-specific, which is why this whole module - not just
//! the pty/termios/signal primitives it calls - is `cfg(unix)`-gated; see
//! `session/windows.rs` for the equivalent using Windows-appropriate
//! multiplexing, and `super::{StopReason, finish_session}` for what's
//! actually shared between the two.

use std::fs::File;
use std::io::{self, BufWriter, Read, Write};
use std::os::fd::AsFd;
use std::path::Path;
use std::time::SystemTime;

use nix::libc;
use nix::poll::{poll, PollFd, PollFlags, PollTimeout};

use super::{create_log_file, finish_session, StopReason};
use crate::logfile::LogFile;
use crate::pty_session::PtySession;
use crate::signals;
use crate::stop_trigger::StopTrigger;
use crate::terminal::RawGuard;

const BUF_SIZE: usize = 4096;

/// Reads from the pty master, treating both a clean 0-byte read and the
/// `EIO`-at-EOF quirk (see the comment this used to carry inline - once
/// every fd pointing at the pty's slave side is closed, Linux fails the
/// *next* master read with `EIO` rather than returning `Ok(0)` the way a
/// pipe would) as "the child has exited" rather than distinct data/error
/// cases. Shared between `proxy_loop` and `headless_loop` so this subtlety
/// only has to be handled once.
fn read_master(master: &mut File, buf: &mut [u8]) -> io::Result<Option<usize>> {
    match master.read(buf) {
        Ok(0) => Ok(None),
        Ok(n) => Ok(Some(n)),
        Err(e) if e.raw_os_error() == Some(libc::EIO) => Ok(None),
        Err(e) => Err(e),
    }
}

/// Copies bytes both ways between the outer terminal (`outer_in`/`outer_out`)
/// and the wrapped shell's pty (`master`) until one of the stop conditions
/// fires. Generic over the outer terminal's reader/writer, rather than
/// hardcoded to `Stdin`/`Stdout`, so tests can stand in a pipe pair for "the
/// real terminal" without one actually attached to the test process - the
/// same reasoning `pty_session/unix.rs`'s tests use for the wrapped shell's
/// side.
///
/// Note: the `Signal` stop reason is reached via `poll()` returning `EINTR`,
/// which requires an actual signal to interrupt a blocked syscall. That's
/// exercised manually and via `signals::unix::tests` (which verifies the
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
                    if let Some(info) = signals::received() {
                        return Ok(StopReason::Signal(info));
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
            match read_master(master, &mut buf)? {
                None => return Ok(StopReason::ChildExited),
                Some(n) => {
                    outer_out.write_all(&buf[..n])?;
                    outer_out.flush()?;
                    log.write_output(&buf[..n])?;
                }
            }
        }
    }
}

/// Like `proxy_loop`, but for headless tracking (`run_headless`): there's
/// no live outer terminal to proxy input from or watch for the
/// `stoplogger` phrase, just the tracked command's own pty output, which
/// gets logged and mirrored to rustlogger's own stdout (harmless, and
/// useful if rustlogger is run directly rather than backgrounded by
/// something else). Only `ChildExited` and `Signal` are reachable here.
fn headless_loop<LW: Write>(
    session: &mut PtySession,
    log: &mut LogFile<LW>,
) -> io::Result<StopReason> {
    let mut buf = [0u8; BUF_SIZE];
    let mut stdout = io::stdout();

    loop {
        {
            let mut fds = [PollFd::new(session.master.as_fd(), PollFlags::POLLIN)];
            match poll(&mut fds, PollTimeout::NONE) {
                Ok(_) => {}
                Err(nix::Error::EINTR) => {
                    if let Some(info) = signals::received() {
                        return Ok(StopReason::Signal(info));
                    }
                    continue;
                }
                Err(e) => return Err(io::Error::from_raw_os_error(e as i32)),
            }
        }

        match read_master(&mut session.master, &mut buf)? {
            None => return Ok(StopReason::ChildExited),
            Some(n) => {
                let chunk = &buf[..n];
                stdout.write_all(chunk)?;
                stdout.flush()?;
                log.write_output(chunk)?;
            }
        }
    }
}

/// Runs a full session: spawns `shell` in a pty, raw-modes the real stdin,
/// proxies until a stop condition fires, logs the whole thing, and
/// returns the exit code rustlogger itself should exit with. `log_dir`
/// (from `--log-dir`/`RUSTLOGGER_LOG_DIR`, resolved by `main.rs`) places
/// the log file there instead of the process's own cwd - see
/// `super::log_path`.
pub fn run(shell: &str, log_dir: Option<&Path>) -> io::Result<i32> {
    signals::install()?;

    let mut session = PtySession::spawn(shell)?;
    let mut trigger = StopTrigger::new();

    let started_at = SystemTime::now();
    let (log_path, log_file) = create_log_file(log_dir, started_at)?;
    let tty = nix::unistd::ttyname(io::stdin())
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "unknown".to_string());
    let mut log = LogFile::new(BufWriter::new(log_file), shell, &tty, started_at)?;
    eprintln!("rustlogger: logging session to {}", log_path.display());

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

    finish_session(&mut session, &mut log, reason)
}

/// Runs a headless tracked session: spawns `command` (with `args`) in a
/// pty - no outer terminal is touched at all, since headless tracking is
/// meant to be driven by something else (e.g. the rustlogger MCP server)
/// rather than a human sitting at a live terminal. Logs the whole thing
/// and mirrors it to rustlogger's own stdout, until the command exits or
/// rustlogger itself is signaled to stop tracking (typically via
/// `SIGTERM`, which is what the MCP server's "stop tracking" tool sends).
/// `log_dir` (from `--log-dir`/`RUSTLOGGER_LOG_DIR`, resolved by
/// `main.rs`) places the log file there instead of the process's own cwd
/// - see `super::log_path`.
pub fn run_headless(command: &str, args: &[String], log_dir: Option<&Path>) -> io::Result<i32> {
    signals::install()?;

    let mut cmd = std::process::Command::new(command);
    cmd.args(args);
    let mut session = PtySession::spawn_command(cmd)?;

    let started_at = SystemTime::now();
    let (log_path, log_file) = create_log_file(log_dir, started_at)?;
    let command_line = std::iter::once(command.to_string())
        .chain(args.iter().cloned())
        .collect::<Vec<_>>()
        .join(" ");
    let mut log = LogFile::new(
        BufWriter::new(log_file),
        &command_line,
        &session.tty,
        started_at,
    )?;
    eprintln!(
        "rustlogger: tracking `{command_line}`, logging to {}",
        log_path.display()
    );

    let reason = headless_loop(&mut session, &mut log)?;

    finish_session(&mut session, &mut log, reason)
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

        session.terminate().expect("failed to send SIGHUP to child");
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

        session.terminate().expect("failed to send SIGHUP to child");
        session.wait().expect("failed to reap child");
    }

    #[test]
    fn headless_loop_captures_output_and_reports_child_exited() {
        let mut echo_command = std::process::Command::new("/bin/echo");
        echo_command.arg("hello-headless");
        let mut session = PtySession::spawn_command(echo_command)
            .expect("failed to spawn /bin/echo in a pty");

        let mut log = test_log();
        let reason = headless_loop(&mut session, &mut log).expect("headless_loop failed");

        assert_eq!(reason, StopReason::ChildExited);

        let exit_code =
            finish_session(&mut session, &mut log, reason).expect("finish_session failed");
        assert_eq!(exit_code, 0);

        let logged = String::from_utf8(log.into_writer()).unwrap();
        assert!(
            logged.contains("hello-headless"),
            "expected the command's output in the log, got: {logged:?}"
        );
        assert!(
            logged.contains("reason: process exited"),
            "log: {logged:?}"
        );
    }

    #[test]
    fn finish_session_preserves_output_logged_before_it_was_called() {
        // Regression guard for the refactor that split session.rs into
        // session/mod.rs (finish_session) + session/unix.rs (the loops
        // that call it): finish_session must append the footer after
        // whatever was already written, not clobber or reorder it. Uses
        // LogFile directly, rather than driving a real pty through
        // proxy_loop, because two lines written back-to-back into a pipe
        // can coalesce into a single read() - a real, pre-existing
        // characteristic of proxy_loop's stoplogger detection (it returns
        // the moment the trigger phrase is found in a chunk, without
        // waiting for a prior command's output to round-trip back through
        // master first), not something this refactor changed or something
        // worth pinning down with a timing-dependent test here.
        let mut session = PtySession::spawn("/bin/sh").expect("failed to spawn shell");
        let mut log = test_log();
        log.write_output(b"output from before the stop condition fired\n")
            .expect("failed to write to log");

        let exit_code = finish_session(&mut session, &mut log, StopReason::StopPhrase)
            .expect("finish_session failed");
        assert_eq!(exit_code, 0, "stoplogger should be a clean exit");

        let logged = String::from_utf8(log.into_writer()).unwrap();
        assert!(
            logged.contains("output from before the stop condition fired"),
            "expected prior log content to survive finish_session, got: {logged:?}"
        );
        assert!(
            logged.contains("=== rustlogger session ended "),
            "log: {logged:?}"
        );
        assert!(
            logged.contains("reason: stoplogger command"),
            "log: {logged:?}"
        );
        assert!(
            logged.find("output from before").unwrap() < logged.find("session ended").unwrap(),
            "prior output should appear before the footer, not after: {logged:?}"
        );
    }
}
