//! End-to-end test: spawns the actual compiled `rustlogger` binary
//! attached to a real pty (reusing `PtySession::spawn_command`, the same
//! way `pty_session.rs`'s own unit test spawns a plain shell - see
//! `docs/rustlogger-design.md`'s note on why `lib.rs` exists), feeds it a
//! short-lived shell command through that pty as if a user had typed it,
//! and checks both rustlogger's own exit code and the log file it wrote.

use std::fs;
use std::io::{self, Read, Write};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use rustlogger::pty_session::PtySession;

/// Drives the *outer* pty this test spawns rustlogger under, as if a user
/// were typing into rustlogger's own controlling terminal. `PtySession`'s
/// shape differs by platform - Unix exposes a single bidirectional
/// `master: File`, Windows only offers one-shot owned
/// `take_reader`/`take_writer` handles (see `pty_session/windows.rs`) -
/// so this wraps whichever the platform gives us behind one `Read + Write`
/// type, letting the test bodies below stay identical across platforms
/// rather than needing their own `#[cfg]`.
struct OuterPty {
    #[cfg(unix)]
    file: std::fs::File,
    #[cfg(windows)]
    reader: Box<dyn Read + Send>,
    #[cfg(windows)]
    writer: Box<dyn Write + Send>,
}

impl OuterPty {
    #[cfg(unix)]
    fn new(session: &mut PtySession) -> Self {
        // Cloning the fd (rather than moving `session.master` out) keeps
        // `session` itself fully intact for the `wait()`/`terminate()`
        // calls the tests still make on it afterward.
        Self {
            file: session
                .master
                .try_clone()
                .expect("failed to clone pty master fd"),
        }
    }

    #[cfg(windows)]
    fn new(session: &mut PtySession) -> Self {
        Self {
            reader: session.take_reader(),
            writer: session.take_writer(),
        }
    }
}

#[cfg(unix)]
impl Read for OuterPty {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.file.read(buf)
    }
}
#[cfg(unix)]
impl Write for OuterPty {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.file.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

#[cfg(windows)]
impl Read for OuterPty {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.reader.read(buf)
    }
}
#[cfg(windows)]
impl Write for OuterPty {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.writer.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }
}

/// A scratch directory under the OS temp dir, unique per test run - no
/// tempfile crate in the tree (see `docs/crate-checklist.md`), and a
/// hand-rolled unique name is all a test needs.
struct ScratchDir {
    path: std::path::PathBuf,
}

impl ScratchDir {
    fn create(label: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock is before the epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "rustlogger-it-{label}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("failed to create scratch dir");
        Self { path }
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// Finds the single `rustlogger-*.log` file expected in `dir` and returns
/// its contents.
fn read_the_log_file(dir: &std::path::Path) -> String {
    let mut logs: Vec<_> = fs::read_dir(dir)
        .expect("failed to read scratch dir")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|p| {
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            name.starts_with("rustlogger-") && name.ends_with(".log")
        })
        .collect();

    assert_eq!(
        logs.len(),
        1,
        "expected exactly one rustlogger-*.log file in {dir:?}, found {logs:?}"
    );
    fs::read_to_string(logs.remove(0)).expect("failed to read log file")
}

#[test]
fn wraps_a_short_shell_session_and_logs_it() {
    let scratch = ScratchDir::create("basic");

    let mut command = Command::new(env!("CARGO_BIN_EXE_rustlogger"));
    command.env("SHELL", "/bin/sh").current_dir(&scratch.path);

    let mut session =
        PtySession::spawn_command(command).expect("failed to spawn rustlogger under a pty");
    let mut outer = OuterPty::new(&mut session);

    outer
        .write_all(b"echo integration-test-marker-77\n")
        .expect("failed to write command to rustlogger's controlling terminal");
    outer
        .write_all(b"exit 4\n")
        .expect("failed to write exit to rustlogger's controlling terminal");

    // Draining to EOF blocks until rustlogger itself has exited and every
    // fd/handle pointing at its side of this outer pty has closed - the
    // same reasoning as `pty_session.rs`'s own test.
    let mut output = Vec::new();
    let _ = outer.read_to_end(&mut output);
    let displayed = String::from_utf8_lossy(&output);
    assert!(
        displayed.contains("integration-test-marker-77"),
        "expected the echoed command in what rustlogger displayed, got: {displayed:?}"
    );

    let exit_code = session
        .wait()
        .expect("failed to wait on rustlogger process");
    assert_eq!(
        exit_code,
        Some(4),
        "rustlogger's own exit code should mirror the wrapped shell's"
    );

    let log = read_the_log_file(&scratch.path);
    assert!(
        log.starts_with("=== rustlogger session started "),
        "log is missing its header: {log:?}"
    );
    assert!(log.contains("shell: /bin/sh"), "log: {log:?}");
    assert!(log.contains("tty: /dev/pts/"), "log: {log:?}");
    assert!(
        log.contains("integration-test-marker-77"),
        "log is missing the echoed command: {log:?}"
    );
    assert!(
        log.contains("=== rustlogger session ended "),
        "log is missing its footer: {log:?}"
    );
    assert!(log.contains("reason: process exited"), "log: {log:?}");
    assert!(log.contains("exit code: 4"), "log: {log:?}");
}

#[test]
fn stoplogger_ends_the_session_and_records_it_as_the_reason() {
    let scratch = ScratchDir::create("stoplogger");

    let mut command = Command::new(env!("CARGO_BIN_EXE_rustlogger"));
    command.env("SHELL", "/bin/sh").current_dir(&scratch.path);

    let mut session =
        PtySession::spawn_command(command).expect("failed to spawn rustlogger under a pty");
    let mut outer = OuterPty::new(&mut session);

    outer
        .write_all(b"stoplogger\n")
        .expect("failed to write stoplogger to rustlogger's controlling terminal");

    let mut output = Vec::new();
    let _ = outer.read_to_end(&mut output);

    let exit_code = session
        .wait()
        .expect("failed to wait on rustlogger process");
    assert_eq!(
        exit_code,
        Some(0),
        "stopping via the trigger phrase should be a clean exit"
    );

    let log = read_the_log_file(&scratch.path);
    assert!(log.contains("reason: stoplogger command"), "log: {log:?}");
}

#[test]
fn headless_mode_tracks_a_command_given_directly_on_argv() {
    let scratch = ScratchDir::create("headless");

    // Headless mode doesn't touch rustlogger's own controlling terminal
    // (see session::run_headless), so unlike the interactive-mode tests
    // above, it doesn't need PtySession::spawn_command to give rustlogger
    // itself a pty - a plain child process with piped output is enough.
    let output = Command::new(env!("CARGO_BIN_EXE_rustlogger"))
        .arg("echo")
        .arg("integration-headless-marker")
        .current_dir(&scratch.path)
        .output()
        .expect("failed to run rustlogger in headless mode");

    assert!(
        output.status.success(),
        "rustlogger exited with {:?}, stderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("tracking `echo integration-headless-marker`"),
        "stderr: {stderr:?}"
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("integration-headless-marker"),
        "expected the tracked command's output mirrored to stdout, got: {stdout:?}"
    );

    let log = read_the_log_file(&scratch.path);
    assert!(
        log.contains("shell: echo integration-headless-marker"),
        "log: {log:?}"
    );
    assert!(log.contains("tty: /dev/pts/"), "log: {log:?}");
    assert!(
        log.contains("integration-headless-marker"),
        "log is missing the tracked command's output: {log:?}"
    );
    assert!(log.contains("reason: process exited"), "log: {log:?}");
    assert!(log.contains("exit code: 0"), "log: {log:?}");
}

#[test]
fn log_dir_flag_places_the_log_in_the_given_directory_auto_created() {
    let scratch = ScratchDir::create("log-dir-flag");
    // Deliberately not pre-created - proves --log-dir's mkdir -p semantics,
    // not just "it worked because the dir already existed."
    let log_dir = scratch.path.join("logs");

    let output = Command::new(env!("CARGO_BIN_EXE_rustlogger"))
        .arg("--log-dir")
        .arg(&log_dir)
        .arg("echo")
        .arg("log-dir-marker")
        .current_dir(&scratch.path)
        .output()
        .expect("failed to run rustlogger with --log-dir");

    assert!(
        output.status.success(),
        "rustlogger exited with {:?}, stderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(&log_dir.display().to_string()),
        "expected the startup message to print the resolved directory, not just the bare filename: {stderr:?}"
    );

    assert!(log_dir.is_dir(), "expected --log-dir to auto-create {log_dir:?}");
    let log = read_the_log_file(&log_dir);
    assert!(
        log.contains("log-dir-marker"),
        "log is missing the tracked command's output: {log:?}"
    );

    let logs_in_cwd: Vec<_> = fs::read_dir(&scratch.path)
        .expect("failed to read scratch dir")
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            name.starts_with("rustlogger-") && name.ends_with(".log")
        })
        .collect();
    assert!(
        logs_in_cwd.is_empty(),
        "log should not also land in cwd when --log-dir is given, found: {logs_in_cwd:?}"
    );
}

#[test]
fn log_dir_env_var_is_used_when_the_flag_is_not_given() {
    let scratch = ScratchDir::create("log-dir-env");
    let log_dir = scratch.path.join("via-env");

    let output = Command::new(env!("CARGO_BIN_EXE_rustlogger"))
        .env("RUSTLOGGER_LOG_DIR", &log_dir)
        .arg("echo")
        .arg("log-dir-env-marker")
        .current_dir(&scratch.path)
        .output()
        .expect("failed to run rustlogger with RUSTLOGGER_LOG_DIR set");

    assert!(
        output.status.success(),
        "rustlogger exited with {:?}, stderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(log_dir.is_dir(), "expected the env var's dir to be auto-created");
    let log = read_the_log_file(&log_dir);
    assert!(
        log.contains("log-dir-env-marker"),
        "log is missing the tracked command's output: {log:?}"
    );
}

#[test]
fn log_dir_flag_wins_over_env_var_when_both_are_set() {
    let scratch = ScratchDir::create("log-dir-precedence");
    let flag_dir = scratch.path.join("flag-wins");
    let env_dir = scratch.path.join("env-loses");

    let output = Command::new(env!("CARGO_BIN_EXE_rustlogger"))
        .env("RUSTLOGGER_LOG_DIR", &env_dir)
        .arg("--log-dir")
        .arg(&flag_dir)
        .arg("echo")
        .arg("precedence-marker")
        .current_dir(&scratch.path)
        .output()
        .expect("failed to run rustlogger with both --log-dir and RUSTLOGGER_LOG_DIR set");

    assert!(
        output.status.success(),
        "rustlogger exited with {:?}, stderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(flag_dir.is_dir(), "expected the flag's directory to be used");
    assert!(
        !env_dir.exists(),
        "the env var's directory should never have been created when the flag also won"
    );
    let log = read_the_log_file(&flag_dir);
    assert!(
        log.contains("precedence-marker"),
        "log is missing the tracked command's output: {log:?}"
    );
}

#[test]
fn log_dir_flag_after_the_command_is_the_wrapped_commands_own_argument() {
    let scratch = ScratchDir::create("log-dir-passthrough");
    let log_dir = scratch.path.join("logs");

    // A second --log-dir appears *after* the command token (`echo`) here -
    // it belongs to echo's own argv and must never be reinterpreted by
    // rustlogger's own flag parsing. Checking the log's recorded "shell:"
    // line (built directly from the args Vec that was actually handed to
    // the child process) rather than echo's own stdout sidesteps any
    // ambiguity in exactly how a given `echo` implementation prints
    // leading-dash arguments.
    let output = Command::new(env!("CARGO_BIN_EXE_rustlogger"))
        .arg("--log-dir")
        .arg(&log_dir)
        .arg("echo")
        .arg("marker")
        .arg("--log-dir")
        .arg("nope")
        .current_dir(&scratch.path)
        .output()
        .expect("failed to run rustlogger");

    assert!(
        output.status.success(),
        "rustlogger exited with {:?}, stderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(
        log_dir.is_dir(),
        "the first --log-dir should still have taken effect for rustlogger itself"
    );
    let log = read_the_log_file(&log_dir);
    assert!(
        log.contains("shell: echo marker --log-dir nope"),
        "expected the second --log-dir to be recorded as part of the wrapped command's own argv, not consumed by rustlogger: {log:?}"
    );
    assert!(log.contains("exit code: 0"), "log: {log:?}");
}
