//! Windows equivalent of `session/unix.rs`: ties the same pieces
//! together (raw-mode the outer console, proxy bytes both ways, wire in
//! the stop conditions), but multiplexed differently, since `poll()` is
//! POSIX-only and has no direct Windows equivalent for arbitrary
//! `Read`/`Write` handles the way `portable-pty` exposes them (see
//! `pty_session/windows.rs`'s module doc comment for why those are
//! separate boxed trait objects rather than one fd/HANDLE this could
//! `poll()` directly).
//!
//! **The multiplexing strategy**: one dedicated OS thread per direction
//! (reading the ConPTY / reading the real stdin), each forwarding raw
//! byte chunks back to the main thread over an `mpsc` channel, which also
//! carries "this side closed" and "this side errored" events. The main
//! thread does all the actual decision-making (feeding the `StopTrigger`,
//! writing to the log) so `StopTrigger`/`LogFile` never have to be
//! `Send`, and just polls `signals::received()` in between channel
//! messages via `recv_timeout` - the closest thing to `poll()`'s
//! multi-source wait available without reaching for Windows'
//! `WaitForMultipleObjects` directly (which needs raw `HANDLE`s
//! `portable-pty`'s `Box<dyn Read + Send>`/`Box<dyn Write + Send>`
//! abstraction doesn't expose).
//!
//! **Known limitation, left as such rather than papered over**: the
//! worker threads use plain blocking `Read`/`Write` calls with no
//! portable way to cancel one from another thread (Windows' own
//! mechanism for this, `CancelSynchronousIo`, needs a Win32 API surface,
//! `Win32_System_IO`/`Win32_System_Threading`, beyond the two namespaces
//! `docs/crate-checklist.md`'s "native Windows pty/console handling"
//! entry scoped the `windows` crate dependency to; adding it would need
//! going back through that same governance process, not a silent
//! expansion here). Concretely: once a stop reason is decided,
//! whichever worker thread is *not* the one that reported it (e.g. the
//! stdin-reading thread, after the child has already exited) is simply
//! abandoned rather than joined - it keeps blocking on its last
//! `read()`/`write()` call until the whole process exits at the end of
//! `run`/`run_headless`, which tears it down along with everything else.
//! This is the reason `proxy_loop`/`headless_loop` don't use
//! `std::thread::scope`, which would force a join (and could hang
//! waiting for exactly that abandoned thread) before returning.
//!
//! **Unverified beyond `cargo check`** - see `pty_session/windows.rs`'s
//! module doc comment for why.

use std::io::{self, BufWriter, Read, Write};
use std::path::Path;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, SystemTime};

use super::{create_log_file, finish_session, StopReason};
use crate::logfile::LogFile;
use crate::pty_session::PtySession;
use crate::signals;
use crate::stop_trigger::StopTrigger;
use crate::terminal::RawGuard;

const BUF_SIZE: usize = 4096;

/// How often the main thread wakes up (via `recv_timeout`) purely to
/// check `signals::received()`, even with nothing to report. Windows
/// console-control handlers run on their own OS thread rather than
/// interrupting a blocked syscall the way a Unix signal interrupts
/// `poll()` with `EINTR`, so there's no way to be woken immediately - this
/// bounds how long a Ctrl+C might sit unnoticed while the child and the
/// user are both otherwise silent. Short enough to feel immediate to a
/// human, long enough not to matter for CPU usage.
const SIGNAL_POLL_INTERVAL: Duration = Duration::from_millis(200);

/// What a worker thread discovered, sent back to the main thread - the
/// pushed analogue of `session/unix.rs`'s `poll()`-driven readiness
/// checks.
enum ProxyEvent {
    /// Bytes read from the pty master (the wrapped shell/command's
    /// output) - main thread mirrors these to stdout and the log.
    FromChild(Vec<u8>),
    /// Bytes read from the real stdin - already written to the pty
    /// master by the thread that read them; the main thread only needs
    /// these to feed the `StopTrigger`.
    FromOuter(Vec<u8>),
    /// The pty master hit EOF: the wrapped shell/command exited.
    ChildExited,
    /// The real stdin closed.
    OuterClosed,
    /// A worker thread's read or write failed.
    Io(io::Error),
}

/// Spawns the thread that reads the ConPTY's output and forwards it to
/// `tx` in chunks, until EOF or an error. Detached (not joined) - see the
/// module doc comment's "known limitation" section for why that's
/// deliberate rather than an oversight.
fn spawn_reader_thread(mut reader: Box<dyn Read + Send>, tx: mpsc::Sender<ProxyEvent>) {
    thread::spawn(move || {
        let mut buf = [0u8; BUF_SIZE];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => {
                    let _ = tx.send(ProxyEvent::ChildExited);
                    return;
                }
                Ok(n) => {
                    if tx.send(ProxyEvent::FromChild(buf[..n].to_vec())).is_err() {
                        return; // main thread already gone
                    }
                }
                Err(e) => {
                    let _ = tx.send(ProxyEvent::Io(e));
                    return;
                }
            }
        }
    });
}

/// Spawns the thread that reads the outer input (`R`, the real stdin in
/// `run`) and both (a) writes each chunk straight through to the pty
/// master and (b) forwards it to `tx` so the main thread can feed the
/// `StopTrigger`, until EOF or an error. Detached, same reasoning as
/// `spawn_reader_thread`. Takes `outer_in` by value (rather than
/// `proxy_loop` taking a borrow the way `session/unix.rs` does) because
/// it has to be moved into this thread - see `proxy_loop`'s doc comment.
fn spawn_writer_thread<R: Read + Send + 'static>(
    mut outer_in: R,
    mut writer: Box<dyn Write + Send>,
    tx: mpsc::Sender<ProxyEvent>,
) {
    thread::spawn(move || {
        let mut buf = [0u8; BUF_SIZE];
        loop {
            match outer_in.read(&mut buf) {
                Ok(0) => {
                    let _ = tx.send(ProxyEvent::OuterClosed);
                    return;
                }
                Ok(n) => {
                    let chunk = &buf[..n];
                    if let Err(e) = writer.write_all(chunk) {
                        let _ = tx.send(ProxyEvent::Io(e));
                        return;
                    }
                    if tx.send(ProxyEvent::FromOuter(chunk.to_vec())).is_err() {
                        return; // main thread already gone
                    }
                }
                Err(e) => {
                    let _ = tx.send(ProxyEvent::Io(e));
                    return;
                }
            }
        }
    });
}

/// Copies bytes both ways between the outer terminal (`outer_in`/
/// `outer_out`) and the wrapped shell/command's ConPTY until one of the
/// stop conditions fires. Generic over the outer terminal's reader/writer,
/// the same reasoning `session/unix.rs`'s `proxy_loop` gives for its own
/// `R`/`W` type parameters (so tests can stand in a pipe pair rather than
/// needing a real console attached to the test process), except `outer_in`
/// is taken **by value**, not `&mut`, and bounded `Send + 'static`: it has
/// to be moved wholesale into the dedicated writer thread (see the module
/// doc comment for why there's a thread per direction rather than one
/// `poll()`-multiplexed loop). `outer_out` stays on the main thread, so it
/// only needs `Write`, the same bound the Unix version uses.
pub fn proxy_loop<R, W, LW>(
    outer_in: R,
    outer_out: &mut W,
    session: &mut PtySession,
    trigger: &mut StopTrigger,
    log: &mut LogFile<LW>,
) -> io::Result<StopReason>
where
    R: Read + Send + 'static,
    W: Write,
    LW: Write,
{
    let (tx, rx) = mpsc::channel::<ProxyEvent>();
    spawn_reader_thread(session.take_reader(), tx.clone());
    spawn_writer_thread(outer_in, session.take_writer(), tx);

    loop {
        match rx.recv_timeout(SIGNAL_POLL_INTERVAL) {
            Ok(ProxyEvent::FromChild(chunk)) => {
                outer_out.write_all(&chunk)?;
                outer_out.flush()?;
                log.write_output(&chunk)?;
            }
            Ok(ProxyEvent::FromOuter(chunk)) => {
                if trigger.feed(&chunk) {
                    return Ok(StopReason::StopPhrase);
                }
            }
            Ok(ProxyEvent::ChildExited) => return Ok(StopReason::ChildExited),
            Ok(ProxyEvent::OuterClosed) => return Ok(StopReason::OuterClosed),
            Ok(ProxyEvent::Io(e)) => return Err(e),
            Err(RecvTimeoutError::Timeout) => {
                if let Some(info) = signals::received() {
                    return Ok(StopReason::Signal(info));
                }
            }
            Err(RecvTimeoutError::Disconnected) => {
                return Err(io::Error::other(
                    "both proxy worker threads exited without reporting a reason",
                ));
            }
        }
    }
}

/// Like `proxy_loop`, but for headless tracking (`run_headless`): only
/// one direction exists (the tracked command's own pty output, mirrored
/// to rustlogger's own stdout and logged) - no outer terminal to proxy
/// input from or watch for the `stoplogger` phrase. Only `ChildExited`
/// and `Signal` are reachable here, matching `session/unix.rs`'s
/// `headless_loop`.
fn headless_loop<LW: Write>(session: &mut PtySession, log: &mut LogFile<LW>) -> io::Result<StopReason> {
    let (tx, rx) = mpsc::channel::<ProxyEvent>();
    spawn_reader_thread(session.take_reader(), tx);

    let mut stdout = io::stdout();
    loop {
        match rx.recv_timeout(SIGNAL_POLL_INTERVAL) {
            Ok(ProxyEvent::FromChild(chunk)) => {
                stdout.write_all(&chunk)?;
                stdout.flush()?;
                log.write_output(&chunk)?;
            }
            Ok(ProxyEvent::ChildExited) => return Ok(StopReason::ChildExited),
            Ok(ProxyEvent::Io(e)) => return Err(e),
            Ok(ProxyEvent::FromOuter(_)) | Ok(ProxyEvent::OuterClosed) => {
                unreachable!("headless_loop never spawns a writer thread")
            }
            Err(RecvTimeoutError::Timeout) => {
                if let Some(info) = signals::received() {
                    return Ok(StopReason::Signal(info));
                }
            }
            Err(RecvTimeoutError::Disconnected) => {
                return Err(io::Error::other(
                    "the proxy reader thread exited without reporting a reason",
                ));
            }
        }
    }
}

/// Runs a full session: spawns `shell` attached to a ConPTY, raw-modes
/// the real console, proxies until a stop condition fires, logs the
/// whole thing, and returns the exit code rustlogger itself should exit
/// with. `log_dir` (from `--log-dir`/`RUSTLOGGER_LOG_DIR`, resolved by
/// `main.rs`) places the log file there instead of the process's own cwd
/// - see `super::log_path`.
pub fn run(shell: &str, log_dir: Option<&Path>) -> io::Result<i32> {
    signals::install()?;

    let mut session = PtySession::spawn(shell)?;
    let mut trigger = StopTrigger::new();

    let started_at = SystemTime::now();
    let (log_path, log_file) = create_log_file(log_dir, started_at)?;
    let tty = session.tty.clone();
    let mut log = LogFile::new(BufWriter::new(log_file), shell, &tty, started_at)?;
    eprintln!("rustlogger: logging session to {}", log_path.display());

    let _raw_guard = RawGuard::new()?;
    let mut stdout = io::stdout();
    let reason = proxy_loop(io::stdin(), &mut stdout, &mut session, &mut trigger, &mut log)?;

    finish_session(&mut session, &mut log, reason)
}

/// Runs a headless tracked session: spawns `command` (with `args`)
/// attached to a ConPTY - no outer console is touched at all, since
/// headless tracking is meant to be driven by something else (e.g. the
/// rustlogger MCP server) rather than a human sitting at a live console.
/// Logs the whole thing and mirrors it to rustlogger's own stdout, until
/// the command exits or rustlogger itself is signaled to stop tracking.
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
    let tty = session.tty.clone();
    let mut log = LogFile::new(
        BufWriter::new(log_file),
        &command_line,
        &tty,
        started_at,
    )?;
    eprintln!(
        "rustlogger: tracking `{command_line}`, logging to {}",
        log_path.display()
    );

    let reason = headless_loop(&mut session, &mut log)?;

    finish_session(&mut session, &mut log, reason)
}
