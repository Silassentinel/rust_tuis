//! One-shot output renderers.
//!
//! These are what `rustmon --once` uses, and they must work in a
//! `--no-default-features` build — no external crates, no terminal control, no
//! assumptions about being attached to a tty.

pub mod json;
pub mod text;

use std::io::Write;

use crate::config::{Config, OutputFormat};
use crate::delta::Rates;
use crate::error::Result;
use crate::sample::Snapshot;

/// Render a snapshot (and optionally its rates) to `out` in the configured
/// format.
pub fn render(
    out: &mut dyn Write,
    snapshot: &Snapshot,
    rates: Option<&Rates>,
    config: &Config,
) -> Result<()> {
    match config.format {
        OutputFormat::Text => text::write(out, snapshot, rates, config.verbose),
        OutputFormat::Json => json::write(out, snapshot, rates, config.verbose),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispatches_to_text_by_default() {
        let mut config = Config::new();
        config.format = OutputFormat::Text;
        let snapshot = Snapshot::now();

        let mut buf = Vec::new();
        render(&mut buf, &snapshot, None, &config).expect("renders fine");
        let out = String::from_utf8(buf).unwrap();
        assert!(out.starts_with("rustmon —"), "expected text output, got {out:?}");
    }

    #[test]
    fn dispatches_to_json_when_configured() {
        let mut config = Config::new();
        config.format = OutputFormat::Json;
        let snapshot = Snapshot::now();

        let mut buf = Vec::new();
        render(&mut buf, &snapshot, None, &config).expect("renders fine");
        let out = String::from_utf8(buf).unwrap();
        assert!(out.trim_start().starts_with('{'), "expected JSON output, got {out:?}");
    }
}
