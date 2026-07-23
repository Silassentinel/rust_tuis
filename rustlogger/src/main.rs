//! Thin binary entry point - the real logic lives in `lib.rs` and its
//! modules, so it can be exercised both by unit tests and by the
//! integration tests under `tests/`, which spawn this compiled binary
//! itself.

fn main() {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());

    match rustlogger::session::run(&shell) {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("rustlogger: {e}");
            std::process::exit(1);
        }
    }
}
