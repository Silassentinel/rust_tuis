//! rustlogger: wraps the shell in the terminal it's launched from and
//! records the whole session (input + output) to a log file until the
//! session is stopped.
//!
//! Status: chunks 1-4 done (see `docs/TODO-rustlogger.md`) - the outer
//! terminal is raw-moded, bytes are proxied both ways, and all three stop
//! conditions are wired up. Log file output itself is chunk 5. See
//! `docs/rustlogger-design.md` for the architecture.

mod pty_session;
mod session;
mod signals;
mod stop_trigger;
mod terminal;

fn main() {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());

    match session::run(&shell) {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("rustlogger: {e}");
            std::process::exit(1);
        }
    }
}
