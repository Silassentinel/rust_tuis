//! Thin binary entry point - the real logic lives in `lib.rs` and its
//! modules, so it can be exercised both by unit tests and by the
//! integration tests under `tests/`, which spawn this compiled binary
//! itself.
//!
//! No args: today's interactive mode, wrapping `$SHELL`. One or more
//! args: headless tracking mode, running that command instead (see
//! `session::run_headless` and `docs/rustlogger-design.md`'s chunk 7
//! notes). Ch. 12 of the Rust Book (`minigrep`) is the reference for this
//! project's argument handling generally - this is about as simple as
//! that gets, so no argument-parsing crate is warranted here either.

use std::iter::Peekable;
use std::io;
use std::path::PathBuf;

fn main() {
    let mut args = std::env::args().skip(1).peekable();

    let log_dir = resolve_log_dir(&mut args);

    let result: io::Result<i32> = match args.next() {
        None => {
            let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
            rustlogger::session::run(&shell, log_dir.as_deref())
        }
        Some(command) => {
            let rest: Vec<String> = args.collect();
            rustlogger::session::run_headless(&command, &rest, log_dir.as_deref())
        }
    };

    match result {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("rustlogger: {e}");
            std::process::exit(1);
        }
    }
}

/// Consumes a leading `--log-dir <path>` pair from `args` if present,
/// otherwise falls back to `RUSTLOGGER_LOG_DIR` - same env-default,
/// explicit-flag-wins precedent as `SHELL` above. Must only ever be
/// called once, before the command token is read: anything from the
/// command onward belongs to the wrapped command's own argv and must
/// never be reinterpreted (`rustlogger --log-dir /x mycmd --log-dir /y` -
/// the second `--log-dir` is `mycmd`'s own argument, untouched). That
/// invariant is why this takes a `Peekable` rather than consuming
/// eagerly: it only ever consumes tokens when the very next one is
/// literally `--log-dir`, and does nothing at all - not even a peek's
/// worth of consumption - otherwise, leaving the command token (or lack
/// of one, in interactive mode) exactly as `main`'s own `args.next()`
/// call afterward expects to find it.
///
/// `--log-dir` given with nothing after it (end of argv) is treated the
/// same as not giving it at all, rather than a hard error - consistent
/// with this project's minimal, non-`clap` argument handling elsewhere
/// (see the module doc comment).
fn resolve_log_dir(args: &mut Peekable<impl Iterator<Item = String>>) -> Option<PathBuf> {
    if args.peek().map(String::as_str) == Some("--log-dir") {
        args.next();
        return args.next().map(PathBuf::from);
    }
    std::env::var("RUSTLOGGER_LOG_DIR").ok().map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Consolidated into one test because every scenario reads/writes the
    // same process-wide RUSTLOGGER_LOG_DIR env var - cargo's default
    // parallel test threading would otherwise race separate #[test] fns
    // against each other (see the flaky-test-triage lesson).
    #[test]
    fn log_dir_resolution_scenarios() {
        // Scenario 1: neither the flag nor the env var is set.
        std::env::remove_var("RUSTLOGGER_LOG_DIR");
        let mut args = vec!["echo".to_string(), "hi".to_string()]
            .into_iter()
            .peekable();
        assert_eq!(resolve_log_dir(&mut args), None);
        assert_eq!(
            args.next().as_deref(),
            Some("echo"),
            "the command token must be untouched when nothing was resolved"
        );

        // Scenario 2: env var set, no flag.
        std::env::set_var("RUSTLOGGER_LOG_DIR", "/tmp/from-env");
        let mut args = vec!["echo".to_string()].into_iter().peekable();
        assert_eq!(
            resolve_log_dir(&mut args),
            Some(PathBuf::from("/tmp/from-env"))
        );
        assert_eq!(args.next().as_deref(), Some("echo"));

        // Scenario 3: flag given, no env var.
        std::env::remove_var("RUSTLOGGER_LOG_DIR");
        let mut args = vec![
            "--log-dir".to_string(),
            "/tmp/from-flag".to_string(),
            "echo".to_string(),
        ]
        .into_iter()
        .peekable();
        assert_eq!(
            resolve_log_dir(&mut args),
            Some(PathBuf::from("/tmp/from-flag"))
        );
        assert_eq!(
            args.next().as_deref(),
            Some("echo"),
            "the command token must immediately follow, with the flag pair fully consumed"
        );

        // Scenario 4: both set - the flag wins.
        std::env::set_var("RUSTLOGGER_LOG_DIR", "/tmp/from-env");
        let mut args = vec![
            "--log-dir".to_string(),
            "/tmp/from-flag".to_string(),
            "echo".to_string(),
        ]
        .into_iter()
        .peekable();
        assert_eq!(
            resolve_log_dir(&mut args),
            Some(PathBuf::from("/tmp/from-flag"))
        );

        // Scenario 5: a --log-dir that isn't the very next token (e.g.
        // one that belongs to the wrapped command, appearing after it)
        // must be left completely alone - this function only ever looks
        // at the immediate next token, never scans ahead.
        std::env::remove_var("RUSTLOGGER_LOG_DIR");
        let mut args = vec![
            "echo".to_string(),
            "--log-dir".to_string(),
            "nope".to_string(),
        ]
        .into_iter()
        .peekable();
        assert_eq!(resolve_log_dir(&mut args), None);
        let rest: Vec<String> = args.collect();
        assert_eq!(
            rest,
            vec![
                "echo".to_string(),
                "--log-dir".to_string(),
                "nope".to_string()
            ],
            "a --log-dir belonging to the wrapped command must pass through untouched"
        );

        // Scenario 6: the flag given with nothing after it degrades to
        // "not given," rather than panicking or erroring.
        std::env::remove_var("RUSTLOGGER_LOG_DIR");
        let mut args = vec!["--log-dir".to_string()].into_iter().peekable();
        assert_eq!(resolve_log_dir(&mut args), None);

        std::env::remove_var("RUSTLOGGER_LOG_DIR"); // leave clean for any other test
    }
}
