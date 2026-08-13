//! Detects the literal `stoplogger` command typed by the user in the
//! terminal, so the logger can end the session even though the wrapped
//! shell would otherwise just treat it as an unknown command.
//!
//! This operates on raw bytes as they arrive from the *outer* controlling
//! terminal, before they're forwarded to the child pty - once that
//! terminal is switched to raw mode (chunk 3) nothing does line editing
//! for us any more, so this module has to track lines itself.
//!
//! Known limitation 1 - **the trigger fires on data, not just commands.**
//! This sees every byte from the outer terminal with no notion of what is
//! consuming it on the other end, so any line that trims to `stoplogger`
//! ends the session: typed into an editor, a pager's search prompt, a
//! heredoc, an interactive database client, or pasted as part of a script
//! that happens to contain that word on its own line. The wrapped shell
//! is then sent SIGHUP, discarding whatever unsaved work that program
//! held. This is inherent to "just type the phrase" as an interface -
//! every alternative either changes that documented interface or relies
//! on a heuristic about the inner program's tty state - so it is
//! documented rather than fixed (see `.security/findings.md`
//! RT-core-2026-07-30-09 and the chunk 6 decision in
//! `.security/mitigation-plan.md`).
//!
//! Known limitation 2: this is a simplified line model. Arrow-key history
//! recall, cursor movement within a line, or terminal escape sequences
//! while typing `stoplogger` are not specially handled - only a plain
//! backspace/delete and Ctrl+C are accounted for. Revisit if that turns
//! out to matter in practice (see docs/rustlogger-design.md).

const TRIGGER: &str = "stoplogger";

/// Hard cap on how much of the current line is buffered.
///
/// The only thing this buffer exists to recognise is the literal
/// `stoplogger` (10 bytes), so a few hundred bytes is already far more
/// than any line that could possibly match. Without a cap this grew 1:1
/// with input (`.security/findings.md` RT-core-2026-07-30-08): whenever
/// the inner program has the tty in raw mode and is draining input - any
/// TUI, `less`, `ssh`, `stty raw; cat` - there is no canonical-mode
/// backpressure and no newline ever arrives to clear it, so 400 MiB of
/// newline-free input took rustlogger's RSS from 2.4 MB to 412 MB and
/// kept climbing until the OOM killer took the session (and with it the
/// terminal's raw-mode restore).
const MAX_LINE_LEN: usize = 512;

/// Feed it raw input bytes as they're typed; it reports when a full line
/// equal to `stoplogger` (surrounding whitespace ignored) has just been
/// completed with Enter.
#[derive(Debug, Default)]
pub struct StopTrigger {
    line: Vec<u8>,
    /// Set once the current line has exceeded [`MAX_LINE_LEN`], and
    /// cleared at the next line boundary. While set, further bytes on this
    /// line are discarded rather than buffered, and the line can no longer
    /// match - which is correct, since a line already longer than the cap
    /// is by definition not `stoplogger`.
    overflowed: bool,
}

impl StopTrigger {
    pub fn new() -> Self {
        Self {
            line: Vec::new(),
            overflowed: false,
        }
    }

    /// Feed one chunk of raw input bytes. Returns `true` the moment a
    /// completed line matches the trigger phrase.
    pub fn feed(&mut self, bytes: &[u8]) -> bool {
        let mut triggered = false;
        for &b in bytes {
            match b {
                b'\r' | b'\n' => {
                    if self.current_line_is_trigger() {
                        triggered = true;
                    }
                    self.reset_line();
                }
                0x7f | 0x08 => {
                    // backspace / delete
                    self.line.pop();
                }
                0x03 => {
                    // Ctrl+C: the shell would abandon the current line, so we do too
                    self.reset_line();
                }
                _ => {
                    if self.line.len() < MAX_LINE_LEN {
                        self.line.push(b);
                    } else {
                        // Past the cap: drop the byte and remember that
                        // this line is unmatchable. Dropping (rather than
                        // clearing) means a `stoplogger` typed later on the
                        // *same* over-long line still won't match, which is
                        // the intended reading - it was never a line equal
                        // to the trigger phrase.
                        self.overflowed = true;
                    }
                }
            }
        }
        triggered
    }

    fn reset_line(&mut self) {
        self.line.clear();
        self.overflowed = false;
    }

    fn current_line_is_trigger(&self) -> bool {
        if self.overflowed {
            return false;
        }
        std::str::from_utf8(&self.line)
            .map(|s| s.trim() == TRIGGER)
            .unwrap_or(false)
    }

    /// How many bytes of the current line are buffered right now. Exposed
    /// for the regression test that pins the cap down; not part of the
    /// module's real interface.
    #[cfg(test)]
    fn buffered_len(&self) -> usize {
        self.line.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_trigger_on_newline() {
        let mut t = StopTrigger::new();
        assert!(!t.feed(b"stoplogger"));
        assert!(t.feed(b"\n"));
    }

    #[test]
    fn detects_trigger_on_carriage_return() {
        let mut t = StopTrigger::new();
        assert!(t.feed(b"stoplogger\r"));
    }

    #[test]
    fn ignores_surrounding_whitespace() {
        let mut t = StopTrigger::new();
        assert!(t.feed(b"  stoplogger  \n"));
    }

    #[test]
    fn does_not_trigger_on_partial_word() {
        let mut t = StopTrigger::new();
        assert!(!t.feed(b"stoplog\n"));
    }

    #[test]
    fn does_not_trigger_on_unrelated_command() {
        let mut t = StopTrigger::new();
        assert!(!t.feed(b"sudo apt update\n"));
    }

    #[test]
    fn resets_after_each_line() {
        let mut t = StopTrigger::new();
        assert!(!t.feed(b"ls -la\n"));
        assert!(!t.feed(b"stoplog"));
        assert!(t.feed(b"ger\n"));
    }

    #[test]
    fn backspace_edits_are_respected() {
        let mut t = StopTrigger::new();
        // user typed "stoploggerX" then backspaced the X
        assert!(t.feed(b"stoploggerX\x7f\n"));
    }

    // Regression guard for RT-core-2026-07-30-08: newline-free input used
    // to grow the buffer 1:1 with everything received (measured 400 MiB in
    // -> 412 MB RSS, climbing until the OOM killer intervened).
    #[test]
    fn newline_free_input_does_not_grow_the_buffer_without_bound() {
        let mut t = StopTrigger::new();
        let chunk = vec![b'A'; 64 * 1024];

        // 8 MiB of input with not a single line terminator in it. Small
        // enough to stay fast, many orders of magnitude past the cap.
        for _ in 0..128 {
            assert!(!t.feed(&chunk));
        }

        assert!(
            t.buffered_len() <= MAX_LINE_LEN,
            "buffer grew to {} bytes after 8 MiB of newline-free input; cap is {MAX_LINE_LEN}",
            t.buffered_len()
        );
    }

    #[test]
    fn an_over_long_line_cannot_match_even_if_it_ends_with_the_trigger() {
        let mut t = StopTrigger::new();
        t.feed(&vec![b'A'; MAX_LINE_LEN + 1]);
        // The line is already unmatchable; appending the phrase to it must
        // not resurrect a match, because this was never a line *equal* to
        // the trigger.
        assert!(!t.feed(b"stoplogger\n"));
    }

    #[test]
    fn the_line_after_an_overflow_can_still_trigger_normally() {
        let mut t = StopTrigger::new();
        t.feed(&vec![b'A'; MAX_LINE_LEN * 2]);
        assert!(!t.feed(b"\n"), "the over-long line itself must not match");
        // Overflow state must reset at the line boundary, or one long
        // paste would permanently disable the feature.
        assert!(t.feed(b"stoplogger\n"));
    }

    #[test]
    fn ctrl_c_clears_the_current_line() {
        let mut t = StopTrigger::new();
        assert!(!t.feed(b"stoplogger\x03\n"));
    }
}
