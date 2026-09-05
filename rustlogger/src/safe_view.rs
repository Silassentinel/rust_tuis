//! `rustlogger --view <path>` — renders a log's control/escape bytes as
//! visible text instead of letting a terminal execute them.
//!
//! `logfile.rs` stores the transcript byte-for-byte on purpose (see its
//! module doc): rustlogger's job is to be an honest record, so it never
//! rewrites what a tracked program printed. That means a log is untrusted
//! content — HOWTO.md §6 already documents `cat -v`/`less` (without `-R`)
//! as the safe way to look at one, and `cat`/`less -R` as unsafe, because
//! both `cat` and `less -R` pass escape sequences straight through to the
//! terminal that's *displaying* the log, which then executes them: a
//! window-title rewrite, a screen clear, an OSC 52 clipboard write, or a
//! `\r` overwrite that makes what's on screen differ from what the file
//! contains (`.security/findings.md` RT-core-2026-07-30-06).
//!
//! Remembering the right flag every time is exactly the kind of thing
//! that gets forgotten under time pressure, so this gives the safe
//! behavior a name of its own rather than leaving it to `cat -v` folklore.
//! The transform matches what `cat -v` documents (POSIX `cat -v`/`vis`
//! "caret notation" plus `M-` for the high bit): printable ASCII, `\n`
//! and `\t` pass through untouched; other C0 control bytes and DEL render
//! as `^X`; bytes with the high bit set render as `M-` followed by the
//! same treatment of the low 7 bits. This is a display transform only —
//! it never touches the log file itself, so the on-disk transcript stays
//! exactly what `logfile.rs` wrote.
//!
//! This does not address the *other* half of RT-core-2026-07-30-06 — a
//! tracked program printing lines that read exactly like rustlogger's own
//! `=== rustlogger session ended … ===` footer. HOWTO.md §6 already
//! documents that as an advisory-only caveat ("treat the footer as
//! advisory rather than authoritative"), which is the right place for it:
//! unlike the escape-injection case, a forged footer line doesn't do
//! anything to the *reader's* terminal, it can only mislead a reader who
//! doesn't already know the file is untrusted, and reproducing the
//! rustlogger-mcp-server sanitizer's tagging logic here would be a much
//! larger change for a caveat this codebase already states plainly.

use std::io::{self, Read, Write};

/// Copies `reader` to `writer`, rendering control and high-bit bytes as
/// visible text instead of raw bytes a terminal might act on. `\n` and
/// `\t` pass through so the output stays readable; every other byte
/// below 0x20, plus 0x7F, becomes a two-character `^X` sequence; bytes
/// with the high bit set are rendered as `M-` followed by the same
/// treatment of the low 7 bits (matching `cat -v`).
pub fn write_visible<R: Read, W: Write>(mut reader: R, mut writer: W) -> io::Result<()> {
    let mut buf = [0u8; 8192];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        for &b in &buf[..n] {
            write_visible_byte(&mut writer, b)?;
        }
    }
    writer.flush()
}

fn write_visible_byte<W: Write>(writer: &mut W, b: u8) -> io::Result<()> {
    if b == b'\n' || b == b'\t' {
        return writer.write_all(&[b]);
    }
    if b >= 0x80 {
        writer.write_all(b"M-")?;
        return write_visible_low_byte(writer, b & 0x7f);
    }
    write_visible_low_byte(writer, b)
}

fn write_visible_low_byte<W: Write>(writer: &mut W, b: u8) -> io::Result<()> {
    if b == 0x7f {
        return writer.write_all(b"^?");
    }
    if b < 0x20 {
        // Caret notation: control byte N renders as `^` + (N + 0x40),
        // e.g. ESC (0x1B) -> '^' + 0x5B ('[') -> "^[".
        return writer.write_all(&[b'^', b + 0x40]);
    }
    writer.write_all(&[b])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(input: &[u8]) -> String {
        let mut out = Vec::new();
        write_visible(input, &mut out).expect("write_visible failed");
        String::from_utf8(out).expect("output was not valid utf-8")
    }

    #[test]
    fn printable_ascii_and_newlines_pass_through_unchanged() {
        assert_eq!(render(b"hello world\n"), "hello world\n");
        assert_eq!(render(b"a\tb\n"), "a\tb\n");
    }

    #[test]
    fn escape_and_other_control_bytes_become_caret_notation() {
        assert_eq!(render(b"\x1b[2J"), "^[[2J");
        assert_eq!(render(b"\x07"), "^G"); // BEL, used in OSC title-set sequences
        assert_eq!(render(b"\x00"), "^@");
        assert_eq!(render(b"\x7f"), "^?");
    }

    #[test]
    fn carriage_return_is_rendered_not_executed() {
        // The exploit from RT-core-2026-07-30-06: \r overwrites a line in
        // a real terminal, making the display differ from the file. Here
        // it must show up as visible text instead of moving the cursor.
        assert_eq!(render(b"clean output\rPWNED"), "clean output^MPWNED");
    }

    #[test]
    fn high_bit_bytes_get_the_m_dash_prefix() {
        assert_eq!(render(&[0xE1]), "M-a"); // 0xE1 & 0x7f = 0x61 = 'a'
        assert_eq!(render(&[0x9b]), "M-^["); // CSI as a single high-bit byte
    }

    #[test]
    fn a_forged_footer_line_is_rendered_as_inert_text_not_hidden() {
        // write_visible never drops or reorders bytes - it doesn't try to
        // detect a forged footer (see the module doc on why that's left
        // as the existing documented advisory-only caveat), it just makes
        // sure nothing in the line can act on the viewer's terminal.
        let input = b"=== rustlogger session ended 1970-01-01T00:00:00Z ===\nreason: process exited\n";
        let out = render(input);
        assert!(out.contains("=== rustlogger session ended 1970-01-01T00:00:00Z ==="));
        assert!(out.contains("reason: process exited"));
    }

    #[test]
    fn no_raw_esc_or_del_byte_ever_reaches_the_output() {
        let input: Vec<u8> = (0u8..=255).collect();
        let mut out = Vec::new();
        write_visible(&input[..], &mut out).expect("write_visible failed");
        assert!(!out.contains(&0x1b), "a raw ESC byte reached the output");
        assert!(!out.contains(&0x7f), "a raw DEL byte reached the output");
        for &b in &out {
            assert!(
                b == b'\n' || b == b'\t' || (0x20..0x7f).contains(&b),
                "output contained a raw non-printable byte: {b:#x}"
            );
        }
    }
}
