//! Thin binary over the `rustmon` library.
//!
//! Rust Book Ch. 12 (`minigrep`): `main` parses arguments, calls into the
//! library, and turns errors into an exit code. All logic lives in `lib.rs` so
//! integration tests can exercise it directly.
//!
//! `main` is the one place a non-zero exit and a message on stderr are the
//! right response to an error — the library itself never prints and never
//! exits.

use std::process::ExitCode;

use rustmon::cli::Invocation;

fn main() -> ExitCode {
    let invocation = match rustmon::cli::parse(std::env::args().skip(1)) {
        Ok(invocation) => invocation,
        Err(e) => return report_error(&e),
    };

    match invocation {
        Invocation::Help => {
            print!("{}", rustmon::cli::help_text());
            ExitCode::SUCCESS
        }
        Invocation::Version => {
            println!("{}", rustmon::cli::version_text());
            ExitCode::SUCCESS
        }
        Invocation::Run(config) => match rustmon::run(*config) {
            Ok(code) => exit_code_from(code),
            Err(e) => report_error(&e),
        },
    }
}

/// Print the full error chain to stderr (the offending file, and any
/// underlying `io::Error` via [`std::error::Error::source`]) and return the
/// exit code for it. This is the one place in the crate a non-zero exit and
/// a printed message are the right response — `lib.rs` itself never does
/// either, per this module's own doc comment.
fn report_error(e: &dyn std::error::Error) -> ExitCode {
    eprintln!("rustmon: error: {e}");

    let mut source = e.source();
    while let Some(cause) = source {
        eprintln!("  caused by: {cause}");
        source = cause.source();
    }

    ExitCode::FAILURE
}

/// Convert `run`'s `i32` (a POSIX-style exit status) to an [`ExitCode`].
/// `ExitCode` only accepts `u8`, so anything `run` might someday return
/// outside that range is clamped rather than silently wrapped — not that any
/// path returns one today (`run_once`'s only success value is `0`).
fn exit_code_from(code: i32) -> ExitCode {
    ExitCode::from(code.clamp(0, u8::MAX as i32) as u8)
}
