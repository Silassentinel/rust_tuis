//! Crate-wide error type.
//!
//! Rust Book Ch. 9: one error enum for the library, `?` everywhere, and no
//! `panic!`/`unwrap`/`expect` in library code at all. That last part is a
//! security property, not just style — see `docs/rustmon-design.md` security
//! model item 6. A malformed `/proc` line must produce a `Parse` error, never
//! an abort.
//!
//! Every variant carries the offending path, because "failed to parse an
//! integer" is useless when you're reading forty files a second.

use std::fmt;
use std::io;
use std::path::PathBuf;

/// Convenience alias used throughout the crate.
pub type Result<T> = std::result::Result<T, Error>;

/// Everything that can go wrong while collecting.
#[derive(Debug)]
pub enum Error {
    /// An I/O failure that is *not* "absent" — see [`Absence`] for the cases
    /// that are expected and non-fatal.
    Io { path: PathBuf, source: io::Error },

    /// A file was read fine but its contents didn't match what the kernel
    /// documents. `line` is 1-based; `None` means the whole file.
    Parse {
        path: PathBuf,
        line: Option<usize>,
        reason: String,
    },

    /// A path failed the confinement checks in [`crate::sysfs`] — traversal
    /// attempt, unsafe component, or a symlink resolving outside the root.
    /// Security model item 4.
    PathRejected { path: PathBuf, reason: String },

    /// The running kernel/machine doesn't expose this at all (no hwmon chips,
    /// no GPU, container with `/sys` masked). Distinct from a failure.
    Unsupported { what: &'static str },

    /// A numeric conversion would have overflowed or gone negative. Kept
    /// separate so the "counter went backwards" case in [`crate::delta`] is
    /// greppable.
    Arithmetic { what: &'static str },
}

/// Why a value isn't present, for the fail-soft path.
///
/// Collectors return these instead of hard errors when the kernel simply
/// doesn't offer something, or offers it only to root. Rendering an "n/a
/// (needs root)" cell is the correct behaviour; erroring out is not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Absence {
    /// File or directory doesn't exist on this machine.
    NotPresent,
    /// Exists but we lack permission (`EACCES`). Usually means "needs root".
    PermissionDenied,
    /// Exists, readable, but empty or explicitly disabled by the driver.
    NotReported,
}

/// Linux errno values that mean "this sensor exists in the tree but has
/// nothing to report right now", as opposed to a genuine I/O failure.
///
/// hwmon drivers return these routinely: a `temp*_input` for a probe that
/// isn't physically connected answers `EIO`, a device that has been unbound
/// answers `ENODEV`, and a hot-unplugged one answers `ENXIO`. Treating those
/// as hard errors would make the thermal collector fail on ordinary hardware,
/// which is exactly the fail-soft rule the design forbids breaking.
///
/// These are Linux's numbers specifically (`ENODATA` is 61 here and 96 on
/// macOS), hence the `target_os` gate — v1 is Linux-only by design.
#[cfg(target_os = "linux")]
const ERRNO_NOT_REPORTED: [i32; 5] = [
    5,  // EIO
    6,  // ENXIO
    19, // ENODEV
    61, // ENODATA
    95, // EOPNOTSUPP
];

impl Error {
    /// The path this error concerns, if it has one.
    pub fn path(&self) -> Option<&PathBuf> {
        match self {
            Error::Io { path, .. }
            | Error::Parse { path, .. }
            | Error::PathRejected { path, .. } => Some(path),
            Error::Unsupported { .. } | Error::Arithmetic { .. } => None,
        }
    }

    /// Classify an [`io::Error`] into either an expected [`Absence`] or a real
    /// [`Error`]. The single place `ENOENT`/`EACCES` get their meaning.
    ///
    /// Note there is deliberately no `From<io::Error> for Error`: an
    /// `io::Error` on its own has no path, and a variant that can be built
    /// without one would defeat the whole point of this enum. Every I/O
    /// failure in the crate comes through here, where the path is in scope.
    pub fn classify_io(
        path: &std::path::Path,
        source: io::Error,
    ) -> std::result::Result<Absence, Error> {
        match source.kind() {
            io::ErrorKind::NotFound => return Ok(Absence::NotPresent),
            io::ErrorKind::PermissionDenied => return Ok(Absence::PermissionDenied),
            _ => {}
        }

        #[cfg(target_os = "linux")]
        if let Some(errno) = source.raw_os_error() {
            if ERRNO_NOT_REPORTED.contains(&errno) {
                return Ok(Absence::NotReported);
            }
        }

        Err(Error::Io {
            path: path.to_path_buf(),
            source,
        })
    }

    /// Build a [`Error::Parse`] without repeating the boilerplate at 30 call sites.
    ///
    /// `reason` is frequently built from file contents, which are
    /// kernel-supplied and in some setups attacker-influenced (a USB device
    /// names itself). Callers must pass it through
    /// [`crate::sysfs::sanitize_kernel_string`] before it reaches here —
    /// see the chunk 2 note in `docs/TODO-rustmon.md`.
    pub fn parse(
        path: &std::path::Path,
        line: Option<usize>,
        reason: impl Into<String>,
    ) -> Error {
        Error::Parse {
            path: path.to_path_buf(),
            line,
            reason: reason.into(),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io { path, source } => {
                write!(f, "reading {}: {source}", path.display())
            }
            Error::Parse {
                path,
                line: Some(line),
                reason,
            } => write!(f, "parsing {}:{line}: {reason}", path.display()),
            Error::Parse {
                path,
                line: None,
                reason,
            } => write!(f, "parsing {}: {reason}", path.display()),
            Error::PathRejected { path, reason } => {
                write!(f, "refusing path {}: {reason}", path.display())
            }
            Error::Unsupported { what } => {
                write!(f, "not available on this system: {what}")
            }
            Error::Arithmetic { what } => write!(f, "arithmetic error: {what}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io { source, .. } => Some(source),
            Error::Parse { .. }
            | Error::PathRejected { .. }
            | Error::Unsupported { .. }
            | Error::Arithmetic { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error as _;
    use std::path::Path;

    fn p() -> &'static Path {
        Path::new("sys/class/hwmon/hwmon0/temp1_input")
    }

    #[test]
    fn path_is_carried_by_the_variants_that_have_one() {
        assert!(Error::parse(p(), Some(3), "nope").path().is_some());
        assert!(Error::Io {
            path: p().to_path_buf(),
            source: io::Error::from(io::ErrorKind::Other),
        }
        .path()
        .is_some());
        assert!(Error::PathRejected {
            path: p().to_path_buf(),
            reason: "escapes root".into(),
        }
        .path()
        .is_some());

        assert!(Error::Unsupported { what: "gpu" }.path().is_none());
        assert!(Error::Arithmetic { what: "overflow" }.path().is_none());
    }

    /// `Error` can't derive `PartialEq` (`io::Error` doesn't implement it), so
    /// absence assertions go through the `Ok` value rather than `assert_eq!`
    /// on the whole `Result`.
    fn absence_of(err: io::Error) -> Absence {
        match Error::classify_io(p(), err) {
            Ok(absence) => absence,
            Err(e) => panic!("expected an absence, got error: {e}"),
        }
    }

    #[test]
    fn enoent_and_eacces_are_absences_not_errors() {
        assert_eq!(
            absence_of(io::Error::from(io::ErrorKind::NotFound)),
            Absence::NotPresent
        );
        assert_eq!(
            absence_of(io::Error::from(io::ErrorKind::PermissionDenied)),
            Absence::PermissionDenied
        );
    }

    /// A sensor that is present but not currently reporting must degrade to
    /// absent, not fail the collector. See `ERRNO_NOT_REPORTED`.
    #[cfg(target_os = "linux")]
    #[test]
    fn eio_from_a_disconnected_probe_is_not_reported() {
        for errno in ERRNO_NOT_REPORTED {
            assert_eq!(
                absence_of(io::Error::from_raw_os_error(errno)),
                Absence::NotReported,
                "errno {errno} should classify as NotReported"
            );
        }
    }

    #[test]
    fn a_genuine_io_failure_stays_an_error() {
        let err = Error::classify_io(p(), io::Error::other("boom"))
            .expect_err("ErrorKind::Other must not be swallowed as an absence");
        assert!(matches!(err, Error::Io { .. }));
        assert_eq!(err.path().map(|p| p.as_path()), Some(p()));

        // ENOMEM is a real failure and must not be in the not-reported list.
        #[cfg(target_os = "linux")]
        assert!(Error::classify_io(p(), io::Error::from_raw_os_error(12)).is_err());
    }

    #[test]
    fn display_always_names_the_path() {
        let with_line = Error::parse(p(), Some(7), "expected a u64");
        let rendered = with_line.to_string();
        assert!(rendered.contains("temp1_input"), "{rendered}");
        assert!(rendered.contains(":7:"), "{rendered}");
        assert!(rendered.contains("expected a u64"), "{rendered}");

        let whole_file = Error::parse(p(), None, "empty");
        assert!(!whole_file.to_string().contains(":7:"));
        assert!(whole_file.to_string().contains("temp1_input"));
    }

    #[test]
    fn only_io_exposes_an_inner_source() {
        let io_err = Error::Io {
            path: p().to_path_buf(),
            source: io::Error::from(io::ErrorKind::Other),
        };
        assert!(io_err.source().is_some());
        assert!(Error::parse(p(), None, "x").source().is_none());
        assert!(Error::Unsupported { what: "gpu" }.source().is_none());
    }
}
