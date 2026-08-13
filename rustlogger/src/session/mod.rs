//! Orchestrates the pieces from `pty_session`, `terminal`, `signals`,
//! `stop_trigger`, and `logfile` into a full session, for both
//! interactive (`run`) and headless (`run_headless`) modes. The actual
//! proxy loop is platform-specific (`cfg(unix)`/`cfg(windows)` - see
//! `docs/rustlogger-design.md`'s chunk 9 notes for why Windows needs its
//! own, not a port of the POSIX `poll()`-based one), but [`StopReason`]
//! and [`finish_session`] are genuinely shared: both platforms' loops
//! produce the same `StopReason`, and reacting to one (terminate the
//! child if it didn't already exit, reap it, write the log's footer,
//! work out rustlogger's own exit code) doesn't need to know which
//! platform got there.

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::*;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::*;

use std::fmt;
use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

#[cfg(unix)]
use nix::libc;

use crate::logfile::LogFile;
use crate::pty_session::PtySession;
use crate::signals::StopSignal;
use crate::timestamp::format_utc_compact;

/// Why the session loop stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    /// The user typed the `stoplogger` trigger phrase. Only reachable in
    /// interactive mode (`run`) - headless mode (`run_headless`) has no
    /// live outer keystroke stream to watch for it.
    StopPhrase,
    /// The wrapped shell or tracked command exited on its own (the pty's
    /// master side hit EOF).
    ChildExited,
    /// The outer terminal's input closed from under us - not one of the
    /// documented stop conditions, but reading 0 bytes forever in a loop
    /// would spin, so it's treated as an implicit "session ends any other
    /// way" case. Only reachable in interactive mode.
    OuterClosed,
    /// rustlogger itself received a stop signal (`SIGINT`/`SIGHUP`/`SIGTERM`
    /// on Unix; a console-control event on Windows) - described
    /// platform-agnostically, see [`StopSignal`].
    Signal(StopSignal),
}

impl fmt::Display for StopReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StopReason::StopPhrase => write!(f, "stoplogger command"),
            StopReason::ChildExited => write!(f, "process exited"),
            StopReason::OuterClosed => write!(f, "outer terminal input closed"),
            StopReason::Signal(info) => write!(f, "caught signal {}", info.description),
        }
    }
}

/// How many times to retry with a fresh disambiguated filename when the
/// exclusive create loses a race (or simply collides with another session
/// started in the same second). Small: each attempt appends more entropy,
/// so needing even the second one is already unusual.
const LOG_CREATE_ATTEMPTS: u32 = 16;

/// Builds the base filename for this session's log:
/// `rustlogger-<timestamp>.log`. One-second resolution, which is why
/// [`create_log_file`] can't just trust it to be unique - see the
/// collision handling there.
fn log_filename(started_at: SystemTime) -> String {
    format!("rustlogger-{}.log", format_utc_compact(started_at))
}

/// Creates the directory rustlogger's own logs go into, restricting it to
/// the owner (`0700`) on Unix rather than `0777 & ~umask`. The log is a
/// full session transcript and routinely contains secrets, so neither it
/// nor the directory holding it should be group/world readable, even
/// under a permissive umask.
#[cfg(unix)]
fn create_log_dir(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    // `recursive(true)` gives mkdir -p semantics; `mode` applies to every
    // directory this call creates. A pre-existing directory keeps whatever
    // mode it already had - deliberately, since it isn't ours to retighten.
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
}

/// Windows has no mode bits to set here; the default ACL (inherited from
/// the parent directory) is what applies. Kept as a separate `cfg` fn
/// purely so the call site stays platform-agnostic.
#[cfg(windows)]
fn create_log_dir(dir: &Path) -> io::Result<()> {
    std::fs::create_dir_all(dir)
}

/// Opens `path` as a brand-new log file, failing rather than writing to
/// anything that already exists at that name.
///
/// The security-relevant flags, all of which the previous `File::create`
/// lacked (see `.security/findings.md` RT-core-2026-07-30-01/02/07):
/// - `create_new` = `O_CREAT | O_EXCL`: refuses to open an existing path.
///   That closes the symlink attack outright - POSIX specifies
///   `O_CREAT | O_EXCL` fails with `EEXIST` when the final component is a
///   symlink, *without* following it - and it makes same-second filename
///   collisions a detectable error instead of two sessions silently
///   sharing (and `O_TRUNC`-ing) one file.
/// - `mode(0o600)`: owner-only, instead of `0666 & ~umask` (0644/0664 in
///   practice), so other local accounts can't read the transcript.
/// - `O_NOFOLLOW`: belt-and-braces against the symlink case. `O_EXCL`
///   already covers it; this makes the intent explicit and guards the
///   final component even if the `O_EXCL` reasoning above ever changes.
#[cfg(unix)]
fn open_new_log(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
}

/// Windows equivalent: `create_new` maps to `CREATE_NEW`, which fails if
/// anything already exists at the path - including a pre-planted symlink
/// or reparse point, so the same attack is closed without needing an
/// explicit `FILE_FLAG_OPEN_REPARSE_POINT`. There's no `mode` to set;
/// the file inherits the directory's ACL.
#[cfg(windows)]
fn open_new_log(path: &Path) -> io::Result<File> {
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
}

/// Creates this session's log file and returns both the path it settled
/// on and the open handle, which have to be decided together: the
/// exclusive create is what *detects* a name collision, so the retry that
/// picks a different name can only live here.
///
/// `log_dir` comes from `--log-dir`/`RUSTLOGGER_LOG_DIR` (resolved in
/// `main.rs` - this only ever sees the already-resolved directory, never
/// the flag/env var itself); `None` means the process's own cwd, today's
/// default behavior. The directory is created if missing (mkdir -p,
/// owner-only) so callers - a git hook firing on every commit, say -
/// don't have to pre-create it.
///
/// On collision the retry appends the pid and an attempt counter rather
/// than reusing the same name, so two sessions starting in the same
/// second get two distinct files instead of one corrupted, interleaved
/// one (RT-core-2026-07-30-07). Shared by both platforms'
/// `run`/`run_headless` so none of this is duplicated four times over.
pub(crate) fn create_log_file(
    log_dir: Option<&Path>,
    started_at: SystemTime,
) -> io::Result<(PathBuf, File)> {
    if let Some(dir) = log_dir {
        create_log_dir(dir)?;
    }

    let base = log_filename(started_at);
    let in_dir = |name: &str| match log_dir {
        Some(dir) => dir.join(name),
        None => PathBuf::from(name),
    };

    let mut last_err = None;
    for attempt in 0..LOG_CREATE_ATTEMPTS {
        let name = if attempt == 0 {
            base.clone()
        } else {
            // Keep the .log extension last so anything globbing
            // `rustlogger-*.log` still finds these.
            let stem = base.trim_end_matches(".log");
            format!("{stem}-{}-{attempt}.log", std::process::id())
        };
        let path = in_dir(&name);

        match open_new_log(&path) {
            Ok(file) => return Ok((path, file)),
            // Someone else holds this exact name (a concurrent session, or
            // a pre-planted file/symlink). Both are handled the same way:
            // never write to it, just pick another name.
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => last_err = Some(e),
            Err(e) => return Err(e),
        }
    }

    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!(
            "could not create a log file after {LOG_CREATE_ATTEMPTS} attempts \
             (last tried a name based on {base:?}); refusing to write to an \
             existing path: {}",
            last_err
                .map(|e| e.to_string())
                .unwrap_or_else(|| "unknown".to_string())
        ),
    ))
}

/// How long the child gets to exit on its own after each escalation step
/// before rustlogger tries something harder. Chosen to be quick enough
/// that a hung session never strands the user's terminal in raw mode for
/// long, but generous enough that a well-behaved shell finishes its own
/// exit/cleanup handling first.
const SIGHUP_GRACE: Duration = Duration::from_secs(2);
const SIGTERM_GRACE: Duration = Duration::from_secs(3);
/// `SIGKILL` can't be caught, so this only covers the kernel actually
/// tearing the process down and the reap landing - it never needs to be
/// long.
const SIGKILL_GRACE: Duration = Duration::from_secs(2);
/// How often to re-check for exit while waiting out a grace period. Short
/// enough that a normal, prompt exit isn't noticeably delayed by the
/// polling granularity.
const REAP_POLL_INTERVAL: Duration = Duration::from_millis(25);

/// Polls for the child's exit for up to `grace`. `Ok(Some(code))` means it
/// exited and was reaped within the window; `Ok(None)` means it's still
/// running and the caller should escalate.
fn wait_for_exit(
    session: &mut PtySession,
    grace: Duration,
) -> io::Result<Option<Option<i32>>> {
    let deadline = Instant::now() + grace;
    loop {
        if let Some(code) = session.try_wait()? {
            return Ok(Some(code));
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        std::thread::sleep(REAP_POLL_INTERVAL);
    }
}

/// Asks the wrapped process to stop and reaps it, escalating
/// `SIGHUP` -> `SIGTERM` -> `SIGKILL` rather than sending one signal and
/// blocking forever on `wait()`.
///
/// This exists because the naive version was a real hang
/// (`.security/findings.md` RT-core-2026-07-30-05): a child that ignores
/// `SIGHUP` - `trap '' HUP`, anything `nohup`-style, or a process stopped
/// in `T` state - left rustlogger blocked in `waitpid` with no timeout.
/// By that point `signals::install()` has already replaced rustlogger's
/// own default dispositions, so rustlogger was itself immune to
/// `SIGINT`/`SIGHUP`/`SIGTERM` and only `SIGKILL` could end it. The
/// consequences were: the log's footer never written, the MCP server's
/// "stop tracking" never completing, and - worst for an interactive user -
/// `RawGuard` still alive, leaving the real terminal in raw mode
/// indefinitely and needing rescue from another terminal.
///
/// `Ok(Some(code))` = reaped (`code` itself `None` if a signal killed it).
/// `Ok(None)` = still not reaped even after `SIGKILL`, which should be
/// unreachable in practice (an unkillable process is stuck in
/// uninterruptible sleep, i.e. a kernel/driver problem); the caller
/// records that in the footer rather than hanging.
fn stop_and_reap(session: &mut PtySession) -> io::Result<Option<Option<i32>>> {
    // Step 1: SIGHUP - what a real terminal hanging up would send, so a
    // well-behaved shell runs its normal exit path.
    session.terminate()?;
    if let Some(code) = wait_for_exit(session, SIGHUP_GRACE)? {
        return Ok(Some(code));
    }

    // Step 2: SIGTERM - still catchable, but a much stronger hint.
    session.terminate_forcefully()?;
    if let Some(code) = wait_for_exit(session, SIGTERM_GRACE)? {
        return Ok(Some(code));
    }

    // Step 3: SIGKILL - cannot be caught, blocked or ignored.
    session.kill_now()?;
    wait_for_exit(session, SIGKILL_GRACE)
}

/// Shared tail end of both `run` and `run_headless`: reacts to the stop
/// reason (asking the tracked process to stop first if the session
/// didn't end on its own), reaps it, writes the log's footer, and
/// returns the exit code rustlogger itself should exit with. Matches on
/// `&reason` (rather than consuming it) so the caller can still use
/// `reason` afterward for the log's footer text - `StopReason` isn't
/// `Copy` (it can carry a `String` description), so this isn't just a
/// style preference.
fn finish_session<LW: Write>(
    session: &mut PtySession,
    log: &mut LogFile<LW>,
    reason: StopReason,
) -> io::Result<i32> {
    // Tracks whether the child outlived the full escalation, so the footer
    // can say so instead of silently implying a clean stop.
    let mut unreaped = false;
    // Every non-`ChildExited` path stops the child the same way; only the
    // exit code rustlogger itself reports differs between them.
    let mut stop = |session: &mut PtySession| -> io::Result<Option<i32>> {
        match stop_and_reap(session)? {
            Some(code) => Ok(code),
            None => {
                unreaped = true;
                Ok(None)
            }
        }
    };

    let (exit_code, child_code) = match &reason {
        StopReason::ChildExited => {
            let code = session.wait()?;
            (code.unwrap_or(1), code)
        }
        StopReason::StopPhrase => {
            let code = stop(session)?;
            (0, code)
        }
        StopReason::OuterClosed => {
            let code = stop(session)?;
            (1, code)
        }
        StopReason::Signal(info) => {
            let code = stop(session)?;
            (info.exit_code, code)
        }
    };

    // An unreaped child is recorded explicitly rather than left to look
    // like an ordinary signal-killed exit (both would otherwise show
    // `exit code: (none)`), so a transcript never implies the tracked
    // process stopped when it may still be running.
    let footer_reason = if unreaped {
        format!("{reason} (child did not exit; gave up after SIGKILL)")
    } else {
        reason.to_string()
    };
    log.finish(&footer_reason, child_code, SystemTime::now())?;

    Ok(exit_code)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Unique scratch directory per test - no tempfile crate in the tree
    /// (see `docs/crate-checklist.md`), and these tests each need a
    /// directory nothing else is writing into.
    fn scratch_dir(label: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "rustlogger-{label}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&path).expect("failed to create scratch dir");
        path
    }

    // Regression guard for RT-core-2026-07-30-05. Before the escalation
    // existed this exact child (the finding's own repro) made
    // finish_session block in wait() forever: no footer, and in
    // interactive mode the user's terminal left in raw mode indefinitely.
    // Bounded generously - the point is "terminates at all", not the exact
    // timing, so this can't go flaky on a loaded machine.
    #[cfg(unix)]
    #[test]
    fn stop_and_reap_escalates_past_a_child_that_ignores_sighup() {
        use std::io::Read;
        use std::process::Command;

        let mut command = Command::new("/bin/sh");
        // `echo READY` after the trap, so the parent can wait for the trap
        // to actually be armed instead of racing it. Signaling immediately
        // after spawn would land before `sh` had even run `trap`, hit the
        // default SIGHUP disposition, and kill it on step 1 - making the
        // test pass while proving nothing about escalation.
        //
        // The trailing `exit 0` matters too: without a command after
        // `sleep`, `sh` execs directly into `sleep` as a last-command
        // optimization, and the exec discards the ignore-disposition the
        // trap installed.
        // The loop (rather than one long `sleep`) keeps any process that
        // outlives this test short-lived: when the shell is finally
        // SIGKILLed, at most a one-second `sleep` is orphaned, instead of a
        // five-minute one holding fds open for whatever runs next.
        // The loop also prevents `sh` from exec'ing directly into the final
        // command as a last-command optimization, which would discard the
        // ignore-disposition the trap installed.
        command
            .arg("-c")
            .arg("trap '' HUP; echo READY; while :; do sleep 1; done");
        let mut session =
            PtySession::spawn_command(command).expect("failed to spawn the SIGHUP-ignoring child");

        let ready_deadline = Instant::now() + Duration::from_secs(10);
        let mut seen = Vec::new();
        let mut buf = [0u8; 256];
        while !String::from_utf8_lossy(&seen).contains("READY") {
            assert!(
                Instant::now() < ready_deadline,
                "child never reported READY; got: {:?}",
                String::from_utf8_lossy(&seen)
            );
            match session.master.read(&mut buf) {
                Ok(0) => panic!("pty master hit EOF before the child was ready"),
                Ok(n) => seen.extend_from_slice(&buf[..n]),
                Err(e) => panic!("failed reading from pty master: {e}"),
            }
        }

        let started = Instant::now();
        let reaped = stop_and_reap(&mut session).expect("stop_and_reap returned an error");
        let elapsed = started.elapsed();

        assert!(
            reaped.is_some(),
            "the child outlived even SIGKILL - escalation did not work"
        );
        // SIGHUP grace (2s) elapses, then SIGTERM ends it. Must not have
        // returned instantly (that would mean SIGHUP worked and the test
        // isn't exercising escalation at all), and must be well inside the
        // full 2+3+2s budget.
        assert!(
            elapsed >= SIGHUP_GRACE,
            "returned before the SIGHUP grace even elapsed ({elapsed:?}) - \
             the child can't have been ignoring SIGHUP, so this test proves nothing"
        );
        assert!(
            elapsed < Duration::from_secs(20),
            "took {elapsed:?}; escalation should have ended this in a few seconds"
        );
    }

    #[test]
    fn log_filename_is_the_bare_timestamped_name() {
        assert_eq!(
            log_filename(SystemTime::UNIX_EPOCH),
            "rustlogger-19700101-000000.log"
        );
    }

    #[test]
    fn create_log_file_defaults_to_the_cwd_with_no_directory_component() {
        // Runs in a scratch cwd so the created file doesn't litter the
        // crate root the way a bare relative create otherwise would.
        let scratch = scratch_dir("create-log-cwd");
        let (path, _file) =
            create_log_file(Some(&scratch), SystemTime::UNIX_EPOCH).expect("create_log_file failed");
        assert_eq!(path, scratch.join("rustlogger-19700101-000000.log"));
        assert!(path.is_file(), "expected the log file to actually exist");

        std::fs::remove_dir_all(&scratch).ok();
    }

    #[test]
    fn create_log_file_auto_creates_a_given_dir_including_missing_parents() {
        let scratch = scratch_dir("create-log-mkdirp");
        // Nested and not yet created - proves mkdir -p semantics, not just
        // "the directory already existed."
        let log_dir = scratch.join("nested").join("logs");
        assert!(!log_dir.exists(), "test setup assumption violated");

        let (path, _file) = create_log_file(Some(&log_dir), SystemTime::UNIX_EPOCH)
            .expect("create_log_file failed");

        assert!(log_dir.is_dir(), "expected mkdir -p of {log_dir:?}");
        assert_eq!(path, log_dir.join("rustlogger-19700101-000000.log"));

        std::fs::remove_dir_all(&scratch).ok();
    }

    // Regression guard for RT-core-2026-07-30-01: the transcript routinely
    // contains secrets, so it must not be readable by other local accounts
    // regardless of the caller's umask.
    #[cfg(unix)]
    #[test]
    fn create_log_file_is_owner_only_and_so_is_the_directory_it_creates() {
        use std::os::unix::fs::PermissionsExt;

        let scratch = scratch_dir("create-log-mode");
        let log_dir = scratch.join("logs");
        let (path, _file) = create_log_file(Some(&log_dir), SystemTime::UNIX_EPOCH)
            .expect("create_log_file failed");

        let file_mode = std::fs::metadata(&path)
            .expect("failed to stat the log file")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            file_mode, 0o600,
            "log must be owner-only, got {file_mode:04o}"
        );

        let dir_mode = std::fs::metadata(&log_dir)
            .expect("failed to stat the log dir")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            dir_mode, 0o700,
            "a log directory rustlogger creates itself must be owner-only, got {dir_mode:04o}"
        );

        std::fs::remove_dir_all(&scratch).ok();
    }

    // Regression guard for RT-core-2026-07-30-02: a symlink pre-planted at
    // the (fully predictable) log path must never be written through - that
    // was an arbitrary-file-overwrite primitive.
    #[cfg(unix)]
    #[test]
    fn create_log_file_refuses_to_write_through_a_pre_planted_symlink() {
        let scratch = scratch_dir("create-log-symlink");
        let log_dir = scratch.join("shared");
        std::fs::create_dir_all(&log_dir).expect("failed to create log dir");

        let victim = scratch.join("victim.txt");
        std::fs::write(&victim, b"IMPORTANT VICTIM FILE\n").expect("failed to write victim file");

        // Exactly the attack: predict the log name and point it at a file
        // the victim can write but the attacker wants clobbered.
        let predicted = log_dir.join(log_filename(SystemTime::UNIX_EPOCH));
        std::os::unix::fs::symlink(&victim, &predicted).expect("failed to plant symlink");

        let (path, _file) = create_log_file(Some(&log_dir), SystemTime::UNIX_EPOCH)
            .expect("create_log_file should route around the symlink, not fail outright");

        assert_ne!(
            path, predicted,
            "must not have used the symlinked path at all"
        );
        assert_eq!(
            std::fs::read_to_string(&victim).expect("failed to re-read victim"),
            "IMPORTANT VICTIM FILE\n",
            "the victim file was written through the symlink - the vulnerability is still open"
        );
        assert!(
            !path.symlink_metadata()
                .expect("failed to stat the real log")
                .file_type()
                .is_symlink(),
            "the log rustlogger settled on is itself a symlink"
        );

        std::fs::remove_dir_all(&scratch).ok();
    }

    // Regression guard for RT-core-2026-07-30-07: the filename has
    // one-second resolution, so two sessions starting in the same second
    // used to open (and O_TRUNC) the same file, destroying the first
    // transcript. They must get distinct files instead.
    #[test]
    fn create_log_file_gives_concurrent_same_second_sessions_distinct_files() {
        let scratch = scratch_dir("create-log-collision");
        let started_at = SystemTime::UNIX_EPOCH;

        let (first, _f1) =
            create_log_file(Some(&scratch), started_at).expect("first create_log_file failed");
        let (second, _f2) =
            create_log_file(Some(&scratch), started_at).expect("second create_log_file failed");

        assert_ne!(
            first, second,
            "two sessions in the same second must not share one log file"
        );
        assert!(first.is_file() && second.is_file());
        assert!(
            second
                .file_name()
                .unwrap()
                .to_string_lossy()
                .ends_with(".log"),
            "the disambiguated name must still end in .log so globs find it: {second:?}"
        );

        std::fs::remove_dir_all(&scratch).ok();
    }

    #[test]
    fn stop_reason_display_text_matches_the_documented_log_footer_wording() {
        assert_eq!(StopReason::StopPhrase.to_string(), "stoplogger command");
        assert_eq!(StopReason::ChildExited.to_string(), "process exited");
        assert_eq!(
            StopReason::OuterClosed.to_string(),
            "outer terminal input closed"
        );
        assert_eq!(
            StopReason::Signal(StopSignal {
                description: "SIGTERM".to_string(),
                exit_code: 143
            })
            .to_string(),
            "caught signal SIGTERM"
        );
    }
}
