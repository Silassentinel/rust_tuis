//! Runtime configuration.
//!
//! Constructed from CLI arguments only — there is no config file in v1, and no
//! environment-variable overrides. Both are deliberate: a monitor that silently
//! changes what it reads based on ambient state is harder to reason about, and
//! `--sysfs-root` in particular is a security-relevant knob that should be
//! visible in the command line that ran.

use std::path::PathBuf;
use std::time::Duration;

use crate::error::{Error, Result};

/// Floor on the refresh interval.
///
/// A `--interval 0` would spin the CPU that the tool exists to measure, so the
/// value is clamped rather than accepted. Security model item 8.
pub const MIN_INTERVAL: Duration = Duration::from_millis(100);

/// Default refresh interval.
pub const DEFAULT_INTERVAL: Duration = Duration::from_millis(1000);

/// Default depth of the in-memory history ring buffer (fixed capacity — the
/// tool never grows without bound and never writes history to disk).
pub const DEFAULT_HISTORY_LEN: usize = 120;

/// Ceiling on the history ring buffer, regardless of what `--history` asks
/// for. At one sample per [`MIN_INTERVAL`] this is still bounded well under a
/// megabyte of `Snapshot`s — the point is a hard ceiling exists at all, not
/// the exact number. Security model item 8.
pub const MAX_HISTORY_LEN: usize = 3600;

#[derive(Debug, Clone)]
pub struct Config {
    /// Refresh interval, already clamped to at least [`MIN_INTERVAL`].
    pub interval: Duration,

    /// Root for all filesystem reads. `/` in production; a fixture tree in
    /// tests; anything the user passes to `--sysfs-root`.
    pub sysfs_root: PathBuf,

    /// If non-empty, run only these collectors (`--only cpu,memory`).
    pub only: Vec<String>,

    /// Never run these (`--skip gpu`). Applied after `only`.
    pub skip: Vec<String>,

    /// One snapshot to stdout, then exit — no TUI.
    pub once: bool,

    /// A short one-liner for a shell prompt, then exit — no TUI, and no
    /// `--once` snapshot either. See [`crate::summary`]. Mutually exclusive
    /// with `once` in practice (whichever the CLI parser sets last wins);
    /// `run()` checks this before `once`.
    pub summary: bool,

    /// Output format for the one-shot path.
    pub format: OutputFormat,

    /// Include the `errors` list in output and show collector diagnostics.
    pub verbose: bool,

    /// Samples retained for sparklines.
    pub history_len: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OutputFormat {
    /// Aligned columns for a human.
    #[default]
    Text,
    /// Documented fixed schema, for scripts and MCP consumers.
    Json,
}

impl Config {
    /// Defaults: 1 s interval, root `/`, TUI mode, all collectors.
    pub fn new() -> Self {
        Config {
            interval: DEFAULT_INTERVAL,
            sysfs_root: PathBuf::from("/"),
            only: Vec::new(),
            skip: Vec::new(),
            once: false,
            summary: false,
            format: OutputFormat::default(),
            verbose: false,
            history_len: DEFAULT_HISTORY_LEN,
        }
    }

    /// Apply the clamps and consistency rules. Call this once after parsing,
    /// before anything reads the config.
    ///
    /// - `interval` raised to [`MIN_INTERVAL`] if lower.
    /// - `history_len` capped so memory stays bounded.
    /// - `sysfs_root` canonicalised and verified to be a directory.
    /// - errors if `only` or `skip` names a collector that doesn't exist,
    ///   rather than silently ignoring a typo'd `--only cpuu`.
    pub fn validate(&mut self, known_collectors: &[&str]) -> Result<()> {
        if self.interval < MIN_INTERVAL {
            self.interval = MIN_INTERVAL;
        }
        if self.history_len > MAX_HISTORY_LEN {
            self.history_len = MAX_HISTORY_LEN;
        }

        // Canonicalised here, not left for `SysfsReader::with_root` to
        // discover later: a bad `--sysfs-root` should surface immediately as
        // a config error, before any collector setup has happened, not deep
        // inside registry construction.
        let canonical = std::fs::canonicalize(&self.sysfs_root).map_err(|source| Error::Io {
            path: self.sysfs_root.clone(),
            source,
        })?;
        if !canonical.is_dir() {
            return Err(Error::PathRejected {
                path: self.sysfs_root.clone(),
                reason: "sysfs root is not a directory".into(),
            });
        }
        self.sysfs_root = canonical;

        for name in self.only.iter().chain(self.skip.iter()) {
            if !known_collectors.contains(&name.as_str()) {
                return Err(unknown_collector_error(name, known_collectors));
            }
        }

        Ok(())
    }

    /// Should this collector run, given `only`/`skip`?
    ///
    /// An empty `only` means "no restriction" (everything runs unless
    /// skipped); a non-empty `only` is an allowlist. `skip` is always applied
    /// on top, so a name in both lists is skipped — the more restrictive
    /// instruction wins.
    pub fn wants(&self, collector: &str) -> bool {
        let allowed_by_only = self.only.is_empty() || self.only.iter().any(|c| c == collector);
        let not_skipped = !self.skip.iter().any(|c| c == collector);
        allowed_by_only && not_skipped
    }
}

impl Default for Config {
    fn default() -> Self {
        Self::new()
    }
}

/// Build the "unknown collector name" error shared by [`Config::validate`]
/// and [`crate::cli::parse_collector_list`].
///
/// There's no dedicated `Error` variant for "a config value is invalid" —
/// [`Error::parse`] is reused instead, with the offending CLI flag standing
/// in for the usual file path. That's a reasonable stretch of what the
/// variant means (a source of untrusted input and the bad value it produced)
/// without adding a variant for a single call site.
pub(crate) fn unknown_collector_error(name: &str, known: &[&str]) -> Error {
    Error::parse(
        std::path::Path::new("--only/--skip"),
        None,
        format!("unknown collector {name:?} — known collectors: {}", known.join(", ")),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const KNOWN: &[&str] = &["cpu", "memory", "thermal", "disk", "net", "gpu"];

    #[test]
    fn defaults_are_sane() {
        let c = Config::new();
        assert_eq!(c.interval, DEFAULT_INTERVAL);
        assert_eq!(c.sysfs_root, PathBuf::from("/"));
        assert!(c.only.is_empty());
        assert!(c.skip.is_empty());
        assert!(!c.once);
        assert!(!c.summary);
        assert_eq!(c.format, OutputFormat::Text);
        assert!(!c.verbose);
        assert_eq!(c.history_len, DEFAULT_HISTORY_LEN);
        assert_eq!(Config::default().interval, DEFAULT_INTERVAL);
    }

    /// Security model item 8: `--interval 0` must not be able to spin the CPU
    /// this tool exists to measure.
    #[test]
    fn validate_clamps_a_too_short_interval() {
        let mut c = Config::new();
        c.interval = Duration::ZERO;
        c.sysfs_root = PathBuf::from("/");
        c.validate(KNOWN).expect("validates");
        assert_eq!(c.interval, MIN_INTERVAL);
    }

    #[test]
    fn validate_caps_an_unbounded_history_length() {
        let mut c = Config::new();
        c.history_len = usize::MAX;
        c.sysfs_root = PathBuf::from("/");
        c.validate(KNOWN).expect("validates");
        assert_eq!(c.history_len, MAX_HISTORY_LEN);
    }

    #[test]
    fn validate_rejects_an_unknown_collector_in_only_or_skip() {
        let mut c = Config::new();
        c.sysfs_root = PathBuf::from("/");
        c.only = vec!["cpuu".to_string()];
        let err = c.validate(KNOWN).expect_err("typo must be rejected");
        assert!(matches!(err, Error::Parse { .. }), "got {err:?}");

        let mut c = Config::new();
        c.sysfs_root = PathBuf::from("/");
        c.skip = vec!["gpuu".to_string()];
        assert!(c.validate(KNOWN).is_err());
    }

    #[test]
    fn validate_rejects_a_sysfs_root_that_is_not_a_directory() {
        let mut c = Config::new();
        // Any file that reliably exists and is not a directory.
        c.sysfs_root = PathBuf::from("/etc/hostname");
        if c.sysfs_root.exists() {
            let err = c.validate(KNOWN).expect_err("a file is not a valid root");
            assert!(matches!(err, Error::PathRejected { .. }), "got {err:?}");
        }
    }

    #[test]
    fn validate_rejects_a_sysfs_root_that_does_not_exist() {
        let mut c = Config::new();
        c.sysfs_root = PathBuf::from("/this/path/does/not/exist/hopefully");
        let err = c.validate(KNOWN).expect_err("missing root must be rejected");
        assert!(matches!(err, Error::Io { .. }), "got {err:?}");
    }

    #[test]
    fn wants_with_empty_only_runs_everything_not_skipped() {
        let mut c = Config::new();
        assert!(c.wants("cpu"));
        c.skip = vec!["cpu".to_string()];
        assert!(!c.wants("cpu"));
        assert!(c.wants("memory"));
    }

    /// `skip` applies on top of `only` — the more restrictive instruction
    /// wins if a name manages to end up in both.
    #[test]
    fn skip_wins_over_only_for_the_same_name() {
        let mut c = Config::new();
        c.only = vec!["cpu".to_string(), "memory".to_string()];
        c.skip = vec!["cpu".to_string()];
        assert!(!c.wants("cpu"));
        assert!(c.wants("memory"));
        assert!(!c.wants("thermal"), "not in the only-list at all");
    }
}
