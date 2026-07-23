//! The session log: a header (start time, shell, tty), the session's
//! output stream with a timestamp prefix on every line, and a footer (stop
//! reason + exit code).
//!
//! Only bytes flowing from the wrapped shell's pty back out to the real
//! terminal are logged, not the raw keystrokes read from the outer
//! terminal. In a pty, whatever the user types is echoed back by the
//! *inner* pty's own terminal driver (see `pty_session.rs`) as part of
//! that same output stream, so this already captures a full "what
//! appeared on screen" transcript - the same thing `script`(1) records.
//! It also means a password prompt (e.g. `sudo`) that disables echo on
//! the pty for the duration of the prompt is, correctly, never written to
//! the log: nothing was ever echoed, so there's nothing to capture. That
//! is a deliberate property of logging the display stream rather than raw
//! input, not an oversight.
//!
//! Generic over `Write` (rather than hardcoded to `File`) so tests can
//! check the exact bytes produced against an in-memory buffer instead of
//! reading a real file back.

use std::io::{self, Write};
use std::time::SystemTime;

use crate::timestamp::format_utc;

pub struct LogFile<W: Write> {
    writer: W,
    at_line_start: bool,
}

impl<W: Write> LogFile<W> {
    /// Opens the log with its header already written.
    pub fn new(mut writer: W, shell: &str, tty: &str, started_at: SystemTime) -> io::Result<Self> {
        writeln!(
            writer,
            "=== rustlogger session started {} ===",
            format_utc(started_at)
        )?;
        writeln!(writer, "shell: {shell}")?;
        writeln!(writer, "tty: {tty}")?;
        Ok(Self {
            writer,
            at_line_start: true,
        })
    }

    /// Appends a chunk of the session's display output, stamping the
    /// current time at the start of every line within it, and flushes
    /// before returning. The flush matters: `W` is normally a
    /// `BufWriter<File>` (see `session.rs`), and without it, nothing
    /// written here would actually reach disk until the session ends and
    /// `finish` flushes on its way out - fine for a log nobody reads
    /// until the session is over, but headless tracking mode exists
    /// specifically so something *else* (the rustlogger MCP server) can
    /// check a still-running session's progress, which needs the log
    /// current on disk while the session is still live, not just at the
    /// end.
    pub fn write_output(&mut self, bytes: &[u8]) -> io::Result<()> {
        for &b in bytes {
            if self.at_line_start {
                write!(self.writer, "[{}] ", format_utc(SystemTime::now()))?;
                self.at_line_start = false;
            }
            self.writer.write_all(&[b])?;
            if b == b'\n' {
                self.at_line_start = true;
            }
        }
        self.writer.flush()
    }

    /// Unwraps the underlying writer - only meant for tests that need to
    /// inspect what was written to an in-memory buffer.
    #[cfg(test)]
    pub fn into_writer(self) -> W {
        self.writer
    }

    /// Writes the footer and flushes. Nothing should be logged after this
    /// (the session is over), but it doesn't consume `self` - there's no
    /// meaningful invariant to enforce here beyond what the caller in
    /// `session.rs` already guarantees by construction (it's the last
    /// thing done with the log).
    pub fn finish(
        &mut self,
        reason: &str,
        exit_code: Option<i32>,
        ended_at: SystemTime,
    ) -> io::Result<()> {
        if !self.at_line_start {
            writeln!(self.writer)?;
        }
        writeln!(
            self.writer,
            "=== rustlogger session ended {} ===",
            format_utc(ended_at)
        )?;
        writeln!(self.writer, "reason: {reason}")?;
        match exit_code {
            Some(code) => writeln!(self.writer, "exit code: {code}")?,
            None => writeln!(self.writer, "exit code: (none)")?,
        }
        self.writer.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::{BufWriter, Read};
    use std::time::{Duration, UNIX_EPOCH};

    #[test]
    fn header_records_start_time_shell_and_tty() {
        let started_at = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let log = LogFile::new(Vec::new(), "/bin/bash", "/dev/pts/4", started_at)
            .expect("failed to build log header");

        let text = String::from_utf8(log.writer).unwrap();
        assert_eq!(
            text,
            "=== rustlogger session started 2023-11-14T22:13:20Z ===\nshell: /bin/bash\ntty: /dev/pts/4\n"
        );
    }

    #[test]
    fn each_line_gets_its_own_timestamp_prefix() {
        let mut log = LogFile::new(Vec::new(), "/bin/sh", "/dev/pts/0", UNIX_EPOCH)
            .expect("failed to build log header");

        log.write_output(b"hello\nworld").unwrap();

        let text = String::from_utf8(log.writer).unwrap();
        let body: Vec<&str> = text.lines().skip(3).collect(); // past the header
        assert_eq!(body.len(), 2);
        assert!(body[0].ends_with("] hello"), "got: {body:?}");
        assert!(body[1].ends_with("] world"), "got: {body:?}");
        // Both lines carry a `[...]` timestamp prefix of the same shape.
        for line in &body {
            assert!(line.starts_with('['), "line missing timestamp: {line:?}");
        }
    }

    #[test]
    fn footer_records_reason_and_exit_code_and_closes_a_dangling_line() {
        let mut log = LogFile::new(Vec::new(), "/bin/sh", "/dev/pts/0", UNIX_EPOCH)
            .expect("failed to build log header");
        log.write_output(b"still typing").unwrap(); // no trailing newline yet

        let ended_at = UNIX_EPOCH + Duration::from_secs(1_700_000_100);
        log.finish("shell exited", Some(7), ended_at)
            .expect("failed to write footer");

        let text = String::from_utf8(log.writer).unwrap();

        assert!(text.contains("still typing\n"), "got: {text:?}");
        assert!(text.contains("=== rustlogger session ended 2023-11-14T22:15:00Z ==="));
        assert!(text.contains("reason: shell exited"));
        assert!(text.contains("exit code: 7"));
    }

    #[test]
    fn write_output_flushes_so_a_live_session_is_readable_before_finish() {
        // A `Vec<u8>`-backed `LogFile` (as the other tests use) would not
        // catch a missing flush - `Vec`'s `Write::flush` is a no-op, since
        // there's no OS-level buffering to push through. This needs a
        // real `BufWriter<File>`, and a *second*, independent handle on
        // the same file to read back through, to actually exercise the
        // thing that matters here: does the data reach disk without
        // going through this same `LogFile` (e.g. without calling
        // `finish`), the way `rustlogger-mcp-server` reads a still-running
        // session's log from an entirely separate process.
        let path = std::env::temp_dir().join(format!(
            "rustlogger-logfile-flush-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let file = File::create(&path).expect("failed to create temp file");

        let mut log = LogFile::new(BufWriter::new(file), "/bin/sh", "/dev/pts/0", UNIX_EPOCH)
            .expect("failed to build log header");
        log.write_output(b"still running\n")
            .expect("write_output failed");

        let mut independent_read = String::new();
        File::open(&path)
            .expect("failed to open temp file for independent read")
            .read_to_string(&mut independent_read)
            .expect("failed to read temp file");

        let _ = std::fs::remove_file(&path);

        assert!(
            independent_read.contains("still running"),
            "expected write_output's bytes to already be on disk without calling finish, got: {independent_read:?}"
        );
    }
}
