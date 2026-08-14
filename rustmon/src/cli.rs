//! Argument parsing, hand-rolled over `std::env::args`.
//!
//! No `clap`. The same reasoning as `rustlogger`'s hand-rolled timestamp
//! formatting: the flag set is small and fixed, so an argument-parsing
//! framework would be a dependency bought for convenience rather than
//! capability (`CLAUDE.md` rule 4).
//!
//! Security-relevant parsing rules:
//!
//! - `--sysfs-root` is validated by [`crate::config::Config::validate`], not
//!   here — the check belongs next to the confinement logic it feeds.
//! - `--interval` is clamped, not rejected, so `--interval 0` runs at the floor
//!   rather than spinning.
//! - An unknown flag is an error. Silently ignoring `--formt json` would send
//!   text to a script expecting JSON.
//! - `--only`/`--skip` names are checked against
//!   [`crate::collectors::ALL`], so a typo'd collector name is an error rather
//!   than a silently empty result.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::config::{unknown_collector_error, Config, OutputFormat};
use crate::error::{Error, Result};

/// What the process should do after parsing.
#[derive(Debug)]
pub enum Invocation {
    /// Run with this config.
    Run(Box<Config>),
    /// Print help and exit 0.
    Help,
    /// Print version and exit 0.
    Version,
}

/// Parse arguments (excluding `argv[0]`).
pub fn parse<I, S>(args: I) -> Result<Invocation>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut config = Config::new();
    let mut args = args.into_iter();

    while let Some(arg) = args.next() {
        let arg = arg.as_ref();

        // A closure borrowing `args` rather than a free function: it needs
        // the same "next token, or this flag's value is missing" behaviour
        // at every call site, and a free function would need the iterator
        // threaded through every call.
        let mut next_value = |flag: &'static str| -> Result<String> {
            args.next()
                .map(|v| v.as_ref().to_string())
                .ok_or_else(|| missing_value_error(flag))
        };

        match arg {
            "-h" | "--help" => return Ok(Invocation::Help),
            "-V" | "--version" => return Ok(Invocation::Version),
            "-1" | "--once" => config.once = true,
            "--summary" => config.summary = true,
            "-v" | "--verbose" => config.verbose = true,
            "-i" | "--interval" => {
                config.interval = parse_interval(&next_value("--interval")?)?;
            }
            "-f" | "--format" => {
                let value = next_value("--format")?;
                config.format = match value.as_str() {
                    "text" => OutputFormat::Text,
                    "json" => OutputFormat::Json,
                    _ => {
                        return Err(Error::parse(
                            Path::new("--format"),
                            None,
                            format!("expected \"text\" or \"json\", got {value:?}"),
                        ))
                    }
                };
            }
            "--only" => {
                config.only = parse_collector_list(&next_value("--only")?, crate::collectors::ALL)?;
            }
            "--skip" => {
                config.skip = parse_collector_list(&next_value("--skip")?, crate::collectors::ALL)?;
            }
            "--sysfs-root" => {
                config.sysfs_root = PathBuf::from(next_value("--sysfs-root")?);
            }
            "--history" => {
                let value = next_value("--history")?;
                config.history_len = value.parse::<usize>().map_err(|_| {
                    Error::parse(
                        Path::new("--history"),
                        None,
                        format!("expected a number, got {value:?}"),
                    )
                })?;
            }
            // Silently ignoring `--formt json` would send text to a script
            // expecting JSON — an unrecognised flag is always an error, never
            // a no-op.
            other => {
                return Err(Error::parse(
                    Path::new(other),
                    None,
                    format!("unknown flag {other:?}"),
                ))
            }
        }
    }

    config.validate(crate::collectors::ALL)?;
    Ok(Invocation::Run(Box::new(config)))
}

/// Parse `--interval`'s value. Accepts a bare number as milliseconds, or a
/// `500ms` / `2s` suffix.
///
/// `ms` is checked before `s`: `"500ms"` ends in `s` too, and stripping that
/// first would leave `"500m"`, which isn't a number.
pub fn parse_interval(raw: &str) -> Result<Duration> {
    let bad = || {
        Error::parse(
            Path::new("--interval"),
            None,
            format!("expected a number of milliseconds, or a value like \"500ms\"/\"2s\", got {raw:?}"),
        )
    };

    if let Some(ms) = raw.strip_suffix("ms") {
        return Ok(Duration::from_millis(ms.parse::<u64>().map_err(|_| bad())?));
    }
    if let Some(secs) = raw.strip_suffix('s') {
        return Ok(Duration::from_secs(secs.parse::<u64>().map_err(|_| bad())?));
    }
    Ok(Duration::from_millis(raw.parse::<u64>().map_err(|_| bad())?))
}

/// Parse a comma-separated collector list, validating every name.
///
/// A typo'd `--only cpuu` is rejected here, not silently treated as an empty
/// or all-inclusive list — see the module doc.
pub fn parse_collector_list(raw: &str, known: &[&str]) -> Result<Vec<String>> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|name| {
            if known.contains(&name) {
                Ok(name.to_string())
            } else {
                Err(unknown_collector_error(name, known))
            }
        })
        .collect()
}

/// Usage text.
pub fn help_text() -> &'static str {
    "\
rustmon — a read-only Linux hardware monitor

USAGE:
    rustmon [OPTIONS]

OPTIONS:
    -1, --once              One snapshot to stdout, then exit (no TUI)
        --summary           Shell-prompt one-liner, then exit — see
                             \"rustmon --summary\" in the man page
    -f, --format <FORMAT>   Output format for --once: \"text\" (default) or \"json\"
    -i, --interval <TIME>   Refresh interval: a bare number of milliseconds,
                             or a value like \"500ms\" / \"2s\" (default: 1s,
                             floor: 100ms)
        --only <LIST>       Run only these collectors, comma-separated
                             (e.g. \"cpu,memory\")
        --skip <LIST>       Never run these collectors, comma-separated;
                             applied after --only
        --sysfs-root <PATH> Root for all filesystem reads (default: /)
        --history <N>       Samples retained for sparklines (default: 120,
                             ceiling: 3600)
    -v, --verbose            Include the collector error list in output
    -h, --help               Print this help and exit
    -V, --version             Print the version and exit

Collectors: cpu, memory, thermal, disk, net, gpu, connections

The JSON schema (--format json) is documented in rustmon/README.md.

rustmon is read-only: it never writes to /proc or /sys, never spawns a
subprocess, and never requires root. Anything it can't read is reported as
absent rather than as an error.
"
}

/// `rustmon <version>` plus which optional features this build has compiled in
/// (`tui`, `gpu-nvidia`, `fs-capacity`, `traceroute`) — worth printing,
/// because "why is my NVIDIA GPU missing" / "why does route tracing say
/// unavailable" is answered by that line.
pub fn version_text() -> String {
    let mut features = Vec::new();
    if cfg!(feature = "tui") {
        features.push("tui");
    }
    if cfg!(feature = "gpu-nvidia") {
        features.push("gpu-nvidia");
    }
    if cfg!(feature = "fs-capacity") {
        features.push("fs-capacity");
    }
    if cfg!(feature = "traceroute") {
        features.push("traceroute");
    }

    let feature_list = if features.is_empty() {
        "none".to_string()
    } else {
        features.join(", ")
    };

    format!("rustmon {}\nfeatures: {feature_list}", crate::VERSION)
}

/// Shared "the value for this flag was never given" error, e.g. `rustmon
/// --interval` with nothing after it.
fn missing_value_error(flag: &'static str) -> Error {
    Error::parse(Path::new(flag), None, "missing value")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    fn run_config(a: &[&str]) -> Config {
        match parse(args(a)).expect("parses") {
            Invocation::Run(c) => *c,
            other => panic!("expected Run, got {other:?}"),
        }
    }

    #[test]
    fn no_arguments_yields_default_config() {
        let c = run_config(&[]);
        assert!(!c.once);
        assert_eq!(c.format, OutputFormat::Text);
        assert!(!c.verbose);
    }

    #[test]
    fn help_and_version_short_circuit_before_any_other_flag() {
        assert!(matches!(
            parse(args(&["--once", "--help"])).expect("parses"),
            Invocation::Help
        ));
        assert!(matches!(parse(args(&["-h"])).expect("parses"), Invocation::Help));
        assert!(matches!(
            parse(args(&["-V"])).expect("parses"),
            Invocation::Version
        ));
        assert!(matches!(
            parse(args(&["--version"])).expect("parses"),
            Invocation::Version
        ));
    }

    #[test]
    fn once_and_verbose_flags() {
        let c = run_config(&["--once", "-v"]);
        assert!(c.once);
        assert!(c.verbose);

        let c = run_config(&["-1"]);
        assert!(c.once);
    }

    #[test]
    fn format_accepts_text_and_json() {
        assert_eq!(run_config(&["-f", "text"]).format, OutputFormat::Text);
        assert_eq!(run_config(&["--format", "json"]).format, OutputFormat::Json);
    }

    #[test]
    fn an_invalid_format_value_is_an_error() {
        let err = parse(args(&["--format", "xml"])).expect_err("must fail");
        assert!(matches!(err, Error::Parse { .. }), "got {err:?}");
    }

    #[test]
    fn interval_flag_is_parsed_through_parse_interval() {
        let c = run_config(&["-i", "500"]);
        assert_eq!(c.interval, Duration::from_millis(500));
    }

    #[test]
    fn only_and_skip_are_validated_collector_lists() {
        let c = run_config(&["--only", "cpu,memory", "--skip", "gpu"]);
        assert_eq!(c.only, vec!["cpu", "memory"]);
        assert_eq!(c.skip, vec!["gpu"]);
    }

    #[test]
    fn only_with_a_typo_is_rejected_at_parse_time() {
        let err = parse(args(&["--only", "cpuu"])).expect_err("typo must fail");
        assert!(matches!(err, Error::Parse { .. }), "got {err:?}");
    }

    #[test]
    fn sysfs_root_flag_sets_the_path() {
        // Use the crate root, which is guaranteed to exist and be a
        // directory in every environment this test runs in.
        let root = env!("CARGO_MANIFEST_DIR");
        let c = run_config(&["--sysfs-root", root]);
        assert!(c.sysfs_root.ends_with("rustmon") || c.sysfs_root == std::path::Path::new(root));
    }

    #[test]
    fn history_flag_parses_a_number() {
        assert_eq!(run_config(&["--history", "60"]).history_len, 60);
    }

    #[test]
    fn history_flag_rejects_a_non_number() {
        let err = parse(args(&["--history", "sixty"])).expect_err("must fail");
        assert!(matches!(err, Error::Parse { .. }), "got {err:?}");
    }

    #[test]
    fn an_unknown_flag_is_an_error_not_a_silent_no_op() {
        // Silently ignoring "--formt json" would send text to a script
        // expecting JSON — this is the exact scenario the module doc warns
        // about.
        let err = parse(args(&["--formt", "json"])).expect_err("must fail");
        assert!(matches!(err, Error::Parse { .. }), "got {err:?}");
    }

    #[test]
    fn a_flag_missing_its_value_is_an_error() {
        for flag in ["-i", "--interval", "-f", "--format", "--only", "--skip", "--sysfs-root", "--history"] {
            let err = parse(args(&[flag])).unwrap_err();
            assert!(matches!(err, Error::Parse { .. }), "{flag} produced {err:?}");
        }
    }

    // ---- parse_interval ------------------------------------------------------

    #[test]
    fn parse_interval_accepts_a_bare_number_as_milliseconds() {
        assert_eq!(parse_interval("250").unwrap(), Duration::from_millis(250));
    }

    #[test]
    fn parse_interval_accepts_ms_and_s_suffixes() {
        assert_eq!(parse_interval("500ms").unwrap(), Duration::from_millis(500));
        assert_eq!(parse_interval("2s").unwrap(), Duration::from_secs(2));
    }

    /// The trap the doc comment calls out: "ms" must be checked before "s",
    /// or "500ms" gets the "s" stripped first and fails to parse "500m".
    #[test]
    fn parse_interval_checks_ms_before_s() {
        assert_eq!(parse_interval("1000ms").unwrap(), Duration::from_millis(1000));
    }

    #[test]
    fn parse_interval_rejects_garbage() {
        assert!(parse_interval("").is_err());
        assert!(parse_interval("fast").is_err());
        assert!(parse_interval("-5").is_err());
        assert!(parse_interval("5.5s").is_err());
    }

    // ---- parse_collector_list -------------------------------------------------

    const KNOWN: &[&str] = &["cpu", "memory", "thermal", "disk", "net", "gpu"];

    #[test]
    fn parse_collector_list_trims_and_drops_empty_entries() {
        assert_eq!(
            parse_collector_list(" cpu, memory ,", KNOWN).unwrap(),
            vec!["cpu", "memory"]
        );
    }

    #[test]
    fn parse_collector_list_rejects_an_unknown_name() {
        assert!(parse_collector_list("cpu,nope", KNOWN).is_err());
    }

    #[test]
    fn parse_collector_list_of_empty_string_is_an_empty_list() {
        assert_eq!(parse_collector_list("", KNOWN).unwrap(), Vec::<String>::new());
    }

    // ---- help / version --------------------------------------------------------

    #[test]
    fn help_text_documents_every_flag() {
        let help = help_text();
        for flag in [
            "--once", "--summary", "--format", "--interval", "--only", "--skip", "--sysfs-root", "--history", "--verbose", "--help",
        ] {
            assert!(help.contains(flag), "help text is missing {flag}");
        }
        assert!(help.to_lowercase().contains("read-only"));
    }

    #[test]
    fn version_text_includes_the_crate_version() {
        assert!(version_text().contains(crate::VERSION));
    }

    /// This crate builds with the `tui` feature by default (see
    /// `Cargo.toml`), so a default `cargo test` run must report it.
    #[test]
    fn version_text_lists_compiled_in_features() {
        let text = version_text();
        if cfg!(feature = "tui") {
            assert!(text.contains("tui"), "{text}");
        }
        if cfg!(feature = "traceroute") {
            assert!(text.contains("traceroute"), "{text}");
        }
        if !cfg!(feature = "gpu-nvidia") {
            assert!(!text.contains("gpu-nvidia"), "{text}");
        }
    }
}
