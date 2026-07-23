//! Detects the literal `stoplogger` command typed by the user in the
//! terminal, so the logger can end the session even though the wrapped
//! shell would otherwise just treat it as an unknown command.
//!
//! This operates on raw bytes as they arrive from the *outer* controlling
//! terminal, before they're forwarded to the child pty - once that
//! terminal is switched to raw mode (chunk 3) nothing does line editing
//! for us any more, so this module has to track lines itself.
//!
//! Known limitation: this is a simplified line model. Arrow-key history
//! recall, cursor movement within a line, or terminal escape sequences
//! while typing `stoplogger` are not specially handled - only a plain
//! backspace/delete and Ctrl+C are accounted for. Revisit if that turns
//! out to matter in practice (see docs/rustlogger-design.md).

const TRIGGER: &str = "stoplogger";

/// Feed it raw input bytes as they're typed; it reports when a full line
/// equal to `stoplogger` (surrounding whitespace ignored) has just been
/// completed with Enter.
#[derive(Debug, Default)]
pub struct StopTrigger {
    line: Vec<u8>,
}

impl StopTrigger {
    pub fn new() -> Self {
        Self { line: Vec::new() }
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
                    self.line.clear();
                }
                0x7f | 0x08 => {
                    // backspace / delete
                    self.line.pop();
                }
                0x03 => {
                    // Ctrl+C: the shell would abandon the current line, so we do too
                    self.line.clear();
                }
                _ => self.line.push(b),
            }
        }
        triggered
    }

    fn current_line_is_trigger(&self) -> bool {
        std::str::from_utf8(&self.line)
            .map(|s| s.trim() == TRIGGER)
            .unwrap_or(false)
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

    #[test]
    fn ctrl_c_clears_the_current_line() {
        let mut t = StopTrigger::new();
        assert!(!t.feed(b"stoplogger\x03\n"));
    }
}
