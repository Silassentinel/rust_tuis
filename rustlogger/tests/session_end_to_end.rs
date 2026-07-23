//! End-to-end test: spawns the actual compiled `rustlogger` binary
//! attached to a real pty (reusing `PtySession::spawn_command`, the same
//! way `pty_session.rs`'s own unit test spawns a plain shell - see
//! `docs/rustlogger-design.md`'s note on why `lib.rs` exists), feeds it a
//! short-lived shell command through that pty as if a user had typed it,
//! and checks both rustlogger's own exit code and the log file it wrote.

use std::fs;
use std::io::{Read, Write};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use rustlogger::pty_session::PtySession;

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

    session
        .master
        .write_all(b"echo integration-test-marker-77\n")
        .expect("failed to write command to rustlogger's controlling terminal");
    session
        .master
        .write_all(b"exit 4\n")
        .expect("failed to write exit to rustlogger's controlling terminal");

    // Draining to EOF blocks until rustlogger itself has exited and every
    // fd pointing at its side of this outer pty has closed - the same
    // reasoning as `pty_session.rs`'s own test.
    let mut output = Vec::new();
    let _ = session.master.read_to_end(&mut output);
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
    assert!(log.contains("reason: wrapped shell exited"), "log: {log:?}");
    assert!(log.contains("exit code: 4"), "log: {log:?}");
}

#[test]
fn stoplogger_ends_the_session_and_records_it_as_the_reason() {
    let scratch = ScratchDir::create("stoplogger");

    let mut command = Command::new(env!("CARGO_BIN_EXE_rustlogger"));
    command.env("SHELL", "/bin/sh").current_dir(&scratch.path);

    let mut session =
        PtySession::spawn_command(command).expect("failed to spawn rustlogger under a pty");

    session
        .master
        .write_all(b"stoplogger\n")
        .expect("failed to write stoplogger to rustlogger's controlling terminal");

    let mut output = Vec::new();
    let _ = session.master.read_to_end(&mut output);

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
