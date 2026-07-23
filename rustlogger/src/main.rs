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

use std::io;

fn main() {
    let mut args = std::env::args().skip(1);

    let result: io::Result<i32> = match args.next() {
        None => {
            let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
            rustlogger::session::run(&shell)
        }
        Some(command) => {
            let rest: Vec<String> = args.collect();
            rustlogger::session::run_headless(&command, &rest)
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
