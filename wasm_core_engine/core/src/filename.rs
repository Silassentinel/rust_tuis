//! Filename safety checks. Ported from `safeFilename` in
//! `website/features/ferment-tracker-app/server/fermentData.ts` — guards
//! against path traversal from untrusted input, and against touching a
//! non-dated `.md` file (documentation, not a log entry). Pure string
//! validation, no filesystem access, despite living next to the
//! filesystem-bound functions in the source.

use std::error::Error;
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidFilename(pub String);

impl fmt::Display for InvalidFilename {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Invalid log filename: {}", self.0)
    }
}

impl Error for InvalidFilename {}

fn is_filename_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-'
}

// /^[\w.-]+\.md$/ — the whole filename, made only of word/dot/hyphen chars,
// ending in a literal (lowercase) ".md".
fn has_valid_md_filename_syntax(filename: &str) -> bool {
    match filename.strip_suffix(".md") {
        Some(prefix) if !prefix.is_empty() => prefix.chars().all(is_filename_char),
        _ => false,
    }
}

// /(\d{4}-\d{2}-\d{2})/ — a YYYY-MM-DD run anywhere in the string.
fn contains_date_pattern(s: &str) -> bool {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() < 10 {
        return false;
    }
    for start in 0..=(chars.len() - 10) {
        let w = &chars[start..start + 10];
        let digits_ok = w[0].is_ascii_digit()
            && w[1].is_ascii_digit()
            && w[2].is_ascii_digit()
            && w[3].is_ascii_digit()
            && w[5].is_ascii_digit()
            && w[6].is_ascii_digit()
            && w[8].is_ascii_digit()
            && w[9].is_ascii_digit();
        if digits_ok && w[4] == '-' && w[7] == '-' {
            return true;
        }
    }
    false
}

/// A check-in is always a *dated* Markdown file with no path separators —
/// only a bare filename, no directory components, and it must carry a
/// `YYYY-MM-DD` somewhere in the name. Guards against both path traversal
/// and accidentally deleting/overwriting undated documentation files
/// (`README.md`, `INDEX.md`) sitting alongside real log entries.
pub fn safe_filename(filename: &str) -> Result<String, InvalidFilename> {
    if !has_valid_md_filename_syntax(filename) || !contains_date_pattern(filename) {
        return Err(InvalidFilename(filename.to_string()));
    }
    Ok(filename.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_path_traversal_and_non_bare_filenames() {
        for name in [
            "../../etc/passwd",
            "../tracker.yml",
            "sub/dir/file.md",
            "/absolute/path.md",
            "file.md/../../escape.md",
        ] {
            assert!(safe_filename(name).is_err(), "{name}");
        }
    }

    #[test]
    fn rejects_non_markdown_files_including_the_tracker_config_itself() {
        assert!(safe_filename("tracker.yml").is_err());
    }

    // Second layer of the guard: a log filename must carry a date. Without
    // this, a crafted request could delete a tracker's README.md/INDEX.md —
    // those are documentation, not check-ins.
    #[test]
    fn refuses_to_touch_undated_md_files() {
        for name in ["README.md", "INDEX.md", "1ATEMPLATE.MD", "notes.md"] {
            assert!(safe_filename(name).is_err(), "{name}");
        }
    }

    #[test]
    fn accepts_a_plain_dated_log_filename() {
        assert_eq!(
            safe_filename("Kombucha-Log-2026-07-23.md").unwrap(),
            "Kombucha-Log-2026-07-23.md"
        );
        assert_eq!(
            safe_filename("Pickled-Radishes-Log-2026-07-23.md").unwrap(),
            "Pickled-Radishes-Log-2026-07-23.md"
        );
    }
}
