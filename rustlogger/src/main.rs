//! rustlogger: wraps the shell in the terminal it's launched from and
//! records the whole session (input + output) to a log file until the
//! session is stopped.
//!
//! Status: chunks 1-3 done (see `docs/TODO-rustlogger.md`). The pieces
//! aren't wired together into a running session yet - that's chunk 4
//! (raw-mode the outer terminal, proxy bytes both ways, wire up the
//! stop conditions). See `docs/rustlogger-design.md` for the architecture.

mod pty_session;
mod stop_trigger;

fn main() {
    eprintln!("rustlogger: not implemented yet (scaffolding stage).");
    eprintln!("See docs/rustlogger-design.md and docs/TODO-rustlogger.md.");
    std::process::exit(1);
}
