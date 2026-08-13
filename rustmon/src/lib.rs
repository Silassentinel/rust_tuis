//! `rustmon` — a read-only Linux hardware monitor.
//!
//! Everything lives in the library; `main.rs` is a thin binary over it (Rust
//! Book Ch. 12's `minigrep` split, the same shape `rustlogger` uses), so
//! integration tests can drive the whole program without a terminal.
//!
//! # Layout
//!
//! - [`sysfs`] — the only filesystem entry point. Path confinement, size caps,
//!   no write path.
//! - [`collectors`] — one module per hardware area, each a [`collector::Collector`].
//! - [`sample`] — the plain-data model. No I/O, no rates.
//! - [`delta`] — counters to rates; owns all the interval arithmetic.
//! - [`render`] — one-shot text and JSON output. Dependency-free.
//! - [`summary`] — `--summary`'s fast, fixed shell-prompt one-liner.
//! - [`ui`] — the live TUI, behind the `tui` feature.
//!
//! # The guarantee this crate makes
//!
//! rustmon never writes to `/sys` or `/proc`, never spawns a subprocess, and
//! never requires root. Anything it can't read is reported as absent. The full
//! reasoning is in `docs/rustmon-design.md`; the short version is that a
//! monitoring tool should be the least dangerous thing on the machine.
//!
//! # Status
//!
//! Both `rustmon` (the live TUI) and `rustmon --once` are real, complete,
//! documented in `rustmon/README.md`, and covered end-to-end —  `--once` by
//! `rustmon/tests/integration.rs` driving the compiled binary, the TUI by a
//! real interactive pty session against this machine (see chunk 9's
//! `docs/TODO-rustmon.md` entry). Every chunk in the build plan is done
//! except the NVIDIA half of chunk 10, which is declined rather than
//! blocked: mount capacity (chunk 8, `nix`'s `fs` feature) and the live TUI
//! ([`ui`], chunk 9, `ratatui` + `crossterm`) were both approved and
//! implemented 2026-08-12, and both are in `default`. NVIDIA GPU support has
//! no crate proposal in flight — there's no NVIDIA hardware on the target
//! machine to justify pulling NVML (a `dlopen` of a proprietary library)
//! into the process. A `--no-default-features` build never compiles [`ui`]
//! in at all, so running rustmon without `--once` on that build returns a
//! clean [`Error::Unsupported`] rather than panicking or doing nothing.

pub mod cli;
pub mod collector;
pub mod collectors;
pub mod config;
pub mod delta;
pub mod error;
pub mod render;
pub mod sample;
pub mod summary;
pub mod sysfs;
pub mod units;

#[cfg(feature = "tui")]
pub mod ui;

pub use config::Config;
pub use error::{Error, Result};
pub use sample::Snapshot;

/// Crate version, from Cargo.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Run rustmon with an already-parsed config. Returns the process exit code.
///
/// Dispatches to the one-shot renderer or the TUI. This is the function
/// integration tests call.
pub fn run(config: Config) -> Result<i32> {
    // Checked before `once`: `--summary` is its own fixed, fast collection
    // (see `summary`'s module doc), not a variant of `--once`.
    if config.summary {
        return summary::run(&config);
    }
    if config.once {
        return run_once(&config);
    }

    #[cfg(feature = "tui")]
    {
        ui::run(&config)
    }

    #[cfg(not(feature = "tui"))]
    {
        // The default binary is built with the `tui` feature (see
        // `Cargo.toml`), so reaching here means someone built
        // `--no-default-features` and ran rustmon without `--once` — there's
        // no live view to fall back to, and silently doing nothing would be
        // worse than saying so.
        Err(Error::Unsupported {
            what: "the live TUI (built without the `tui` feature — pass --once for a single snapshot)",
        })
    }
}

/// Collect one snapshot and render it to stdout, then return.
///
/// Note there are no rates on a single snapshot — throughput and CPU
/// utilisation need two readings. `--once` therefore reports counters and
/// gauges only. Taking two snapshots an interval apart to synthesise rates is
/// a deliberate non-feature for v1: it would make `--once` block, which is
/// exactly what scripts calling it don't expect.
pub fn run_once(config: &Config) -> Result<i32> {
    let reader = sysfs::SysfsReader::with_root(config.sysfs_root.clone())?;
    let mut registry = collector::Registry::from_config(config, reader)?;
    let snapshot = registry.collect_all()?;

    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    render::render(&mut handle, &snapshot, None, config)?;

    Ok(0)
}
