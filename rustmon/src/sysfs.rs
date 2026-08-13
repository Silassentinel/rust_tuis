//! The **only** filesystem entry point in the crate.
//!
//! Every read of `/proc` or `/sys` goes through here. Nothing else in `rustmon`
//! may call `std::fs` directly — that rule is what makes the security model
//! auditable, because all the confinement and size-capping lives in one file
//! that a reviewer can read end to end.
//!
//! What this module defends against (see `docs/rustmon-design.md`):
//!
//! - **Path traversal / symlink escape.** Device names come from directory
//!   listings, but a container mount or a user-supplied `--sysfs-root` can put
//!   anything in those listings. `/sys` is full of symlinks, so canonicalising
//!   and re-checking containment is not optional.
//! - **Unbounded reads.** Most `/sys` files report `st_size == 0` while
//!   returning data, so sizing a buffer from metadata is wrong. And a root
//!   accidentally pointed at `/proc` would make `kcore` an attempt to read all
//!   of physical memory. Every read is `Read::take(max_read_bytes)`.
//! - **Writes.** There is no write path here. Not "we don't call it" — the API
//!   does not offer one. Writing `/sys/class/hwmon/*/pwm1` can stop a fan and
//!   physically damage hardware.
//!
//! # What the containment check is and isn't worth
//!
//! Two honest caveats, stated here rather than discovered later:
//!
//! **With the production root of `/`, the containment check is vacuous.**
//! Every canonical path starts with `/`, so the containment step of the
//! private `SysfsReader::resolve`
//! can never fire. What actually protects the production path is the component
//! allowlist ([`is_safe_component`]) plus the rejection of `..` and absolute
//! paths — a collector cannot assemble a path to `/etc/shadow` out of
//! `[A-Za-z0-9_.:-]` components with no parent-directory escapes. The
//! containment check earns its place for fixture trees and for a user-supplied
//! `--sysfs-root`, where a symlink out of the root is a real possibility.
//!
//! **Resolve-then-open is a TOCTOU window.** Between `canonicalize` and
//! `File::open`, a component could in principle be swapped for a symlink.
//! Closing that properly needs `openat2(RESOLVE_BENEATH)`, which is not
//! reachable from `std` and would mean a `libc`/`nix` dependency (`CLAUDE.md`
//! rule 4). The window is accepted deliberately: `/proc` and `/sys` are
//! root-owned and not attacker-writable, and the threat model for
//! `--sysfs-root` is "the user points it at a fixture tree", not "an attacker
//! races us inside a world-writable directory". Anyone pointing a root at
//! untrusted, writable ground should know that is out of scope.
//!
//! # Which path an error carries
//!
//! - [`Error::Io`] carries the **resolved** path — it names what actually
//!   failed at the OS level.
//! - [`Error::Parse`] and [`Error::PathRejected`] carry the **relative** path
//!   as the caller wrote it. It is the form a reader recognises, and it has
//!   already been through [`is_safe_component`], so it cannot smuggle terminal
//!   escapes into an error message the way an arbitrary root could.

use std::fs::File;
use std::io::Read;
use std::iter::Peekable;
use std::path::{Component, Path, PathBuf};
use std::str::Chars;

use crate::error::{Absence, Error, Result};

/// Default cap on any single file read: 1 MiB.
pub const DEFAULT_MAX_READ_BYTES: usize = 1024 * 1024;

/// Default cap on lines parsed from one file.
pub const DEFAULT_MAX_LINES: usize = 65_536;

/// Default cap on entries enumerated from one directory.
pub const DEFAULT_MAX_ENTRIES: usize = 4_096;

/// A read-only, confined reader rooted at some directory.
///
/// In production the root is `/` (so `proc/stat` and `sys/class/hwmon` are the
/// relative paths used everywhere). In tests it's a fixture tree, which is why
/// every collector takes a `&SysfsReader` rather than reaching for `/proc`
/// itself — it makes the whole crate testable without root, without hardware,
/// and deterministically.
#[derive(Debug, Clone)]
pub struct SysfsReader {
    root: PathBuf,
    max_read_bytes: usize,
    max_lines: usize,
    max_entries: usize,
}

impl SysfsReader {
    /// Rooted at `/`, with default limits.
    pub fn new() -> Self {
        // `/` is already canonical, so unlike `with_root` this cannot fail and
        // the constructor doesn't need to return a `Result`. Constructing the
        // struct directly rather than calling `with_root(...).unwrap()` is what
        // keeps the no-panic rule intact.
        SysfsReader {
            root: PathBuf::from("/"),
            max_read_bytes: DEFAULT_MAX_READ_BYTES,
            max_lines: DEFAULT_MAX_LINES,
            max_entries: DEFAULT_MAX_ENTRIES,
        }
    }

    /// Rooted at an arbitrary directory — the fixture-test and
    /// `--sysfs-root` entry point. The root itself is canonicalised once here
    /// so later containment checks compare against a real path.
    pub fn with_root(root: PathBuf) -> Result<Self> {
        let canonical = std::fs::canonicalize(&root).map_err(|source| Error::Io {
            path: root.clone(),
            source,
        })?;

        if !canonical.is_dir() {
            return Err(Error::PathRejected {
                path: root,
                reason: "sysfs root is not a directory".into(),
            });
        }

        Ok(SysfsReader {
            root: canonical,
            max_read_bytes: DEFAULT_MAX_READ_BYTES,
            max_lines: DEFAULT_MAX_LINES,
            max_entries: DEFAULT_MAX_ENTRIES,
        })
    }

    pub fn with_max_read_bytes(mut self, max: usize) -> Self {
        self.max_read_bytes = max;
        self
    }

    pub fn with_max_entries(mut self, max: usize) -> Self {
        self.max_entries = max;
        self
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Cap on bytes taken from any single file.
    pub fn max_read_bytes(&self) -> usize {
        self.max_read_bytes
    }

    /// Cap on lines a parser should take from one file. Enforced by the
    /// collectors (chunk 3 onwards), not here — this module hands back whole
    /// file contents and has no opinion on their shape.
    pub fn max_lines(&self) -> usize {
        self.max_lines
    }

    /// Cap on entries enumerated from one directory.
    pub fn max_entries(&self) -> usize {
        self.max_entries
    }

    /// Resolve `rel` against the root and prove the result is still inside it.
    ///
    /// Rejects: absolute paths, any component failing [`is_safe_component`],
    /// and any canonicalised result that escapes the root. This is the
    /// security-critical function in the crate — chunk 2 tests it before
    /// anything is built on top of it.
    ///
    /// Returns a nested result because "the file isn't there" is not a
    /// failure: `canonicalize` needs the path to exist, so a missing or
    /// permission-denied path surfaces here first and must come back as an
    /// [`Absence`] for the fail-soft rule to hold. A hard `Err` means the path
    /// was *refused*, which is a different and much louder thing than absent.
    fn resolve(&self, rel: &Path) -> Result<std::result::Result<PathBuf, Absence>> {
        if rel.is_absolute() {
            return Err(Error::PathRejected {
                path: rel.to_path_buf(),
                reason: "absolute path; every read is relative to the root".into(),
            });
        }

        for component in rel.components() {
            // Only plain names survive. `..` is `ParentDir`, `.` is `CurDir`,
            // a leading `/` is `RootDir` — all rejected here, which is what
            // stops `a/../../b` before any filesystem call happens.
            let Component::Normal(raw) = component else {
                return Err(Error::PathRejected {
                    path: rel.to_path_buf(),
                    reason: "path may only contain plain names (no `/`, `.` or `..`)".into(),
                });
            };

            let Some(name) = raw.to_str() else {
                return Err(Error::PathRejected {
                    path: rel.to_path_buf(),
                    reason: "path component is not valid UTF-8".into(),
                });
            };

            if !is_safe_component(name) {
                return Err(Error::PathRejected {
                    path: rel.to_path_buf(),
                    reason: format!(
                        "unsafe path component {:?}",
                        sanitize_kernel_string(name, 64)
                    ),
                });
            }
        }

        let joined = self.root.join(rel);

        // Canonicalising is what makes the containment check meaningful:
        // `/sys` is largely symlinks, and a symlink out of a fixture root
        // would otherwise sail straight through a textual prefix test.
        let canonical = match std::fs::canonicalize(&joined) {
            Ok(canonical) => canonical,
            Err(source) => return Error::classify_io(&joined, source).map(Err),
        };

        if !canonical.starts_with(&self.root) {
            return Err(Error::PathRejected {
                path: rel.to_path_buf(),
                reason: "resolves outside the configured root".into(),
            });
        }

        Ok(Ok(canonical))
    }

    /// Read a whole file, capped at `max_read_bytes`, as UTF-8 (lossy).
    ///
    /// Lossy is correct here: kernel-supplied strings such as a USB device's
    /// self-reported name are not guaranteed valid UTF-8, and a monitor should
    /// show a replacement character rather than refuse to display the device.
    /// Callers rendering the result must still strip control bytes — see
    /// [`sanitize_kernel_string`].
    /// Truncation at `max_read_bytes` is silent — there is no "was it cut
    /// short?" signal. That is deliberate: no real `/sys` or `/proc` file this
    /// crate reads comes close to 1 MiB, so hitting the cap means the root is
    /// pointed somewhere it shouldn't be, and the parse that follows will fail
    /// on the truncated tail anyway. The cap exists to bound memory, not to
    /// be recovered from.
    pub fn read_to_string(&self, rel: &Path) -> Result<std::result::Result<String, Absence>> {
        let path = match self.resolve(rel)? {
            Ok(path) => path,
            Err(absence) => return Ok(Err(absence)),
        };

        let file = match File::open(&path) {
            Ok(file) => file,
            Err(source) => return Error::classify_io(&path, source).map(Err),
        };

        // `Read::take`, never `fs::read_to_string`: most `/sys` files report
        // `st_size == 0` while returning data, so a metadata-sized buffer is
        // both wrong and unbounded.
        let mut buf = Vec::new();
        if let Err(source) = file.take(self.max_read_bytes as u64).read_to_end(&mut buf) {
            return Error::classify_io(&path, source).map(Err);
        }

        // Lossy on purpose: a kernel-supplied string (a USB device naming
        // itself) is not guaranteed valid UTF-8, and a monitor should render a
        // replacement character rather than refuse to show the device.
        Ok(Ok(String::from_utf8_lossy(&buf).into_owned()))
    }

    /// First line only, trimmed. The shape of nearly every `/sys` file.
    ///
    /// An empty or whitespace-only file comes back as
    /// [`Absence::NotReported`] rather than an empty string: a driver that has
    /// nothing to say is the case that variant exists for, and it keeps every
    /// caller from having to re-check for `""`.
    pub fn read_first_line(&self, rel: &Path) -> Result<std::result::Result<String, Absence>> {
        let contents = match self.read_to_string(rel)? {
            Ok(contents) => contents,
            Err(absence) => return Ok(Err(absence)),
        };

        let line = contents.lines().next().unwrap_or("").trim();
        if line.is_empty() {
            return Ok(Err(Absence::NotReported));
        }

        Ok(Ok(line.to_string()))
    }

    /// Parse the first line as `u64`.
    pub fn read_u64(&self, rel: &Path) -> Result<std::result::Result<u64, Absence>> {
        let line = match self.read_first_line(rel)? {
            Ok(line) => line,
            Err(absence) => return Ok(Err(absence)),
        };

        match line.parse::<u64>() {
            Ok(value) => Ok(Ok(value)),
            Err(_) => Err(parse_error(rel, "unsigned integer", &line)),
        }
    }

    /// Parse the first line as `i64` (temperatures can be negative).
    pub fn read_i64(&self, rel: &Path) -> Result<std::result::Result<i64, Absence>> {
        let line = match self.read_first_line(rel)? {
            Ok(line) => line,
            Err(absence) => return Ok(Err(absence)),
        };

        match line.parse::<i64>() {
            Ok(value) => Ok(Ok(value)),
            Err(_) => Err(parse_error(rel, "signed integer", &line)),
        }
    }

    /// Directory entry *names* (not paths), filtered and capped.
    ///
    /// Returns names rather than `PathBuf`s on purpose: the caller must go back
    /// through [`SysfsReader`] to read anything, so there's no way to
    /// accidentally hold a path that bypassed confinement.
    pub fn list_dir(
        &self,
        rel: &Path,
        keep: &dyn Fn(&str) -> bool,
    ) -> Result<std::result::Result<Vec<String>, Absence>> {
        let path = match self.resolve(rel)? {
            Ok(path) => path,
            Err(absence) => return Ok(Err(absence)),
        };

        let entries = match std::fs::read_dir(&path) {
            Ok(entries) => entries,
            Err(source) => return Error::classify_io(&path, source).map(Err),
        };

        let mut names = Vec::new();

        // The cap counts entries *examined*, not entries kept: a directory
        // with a million entries that all fail `keep` must still cost a
        // bounded amount of work. Security model item 8.
        for entry in entries.take(self.max_entries) {
            // An entry vanishing mid-iteration is routine in `/sys` (a device
            // unbinding while we walk the class directory) and is not a
            // reason to fail the whole listing.
            let Ok(entry) = entry else { continue };

            let raw = entry.file_name();
            let Some(name) = raw.to_str() else { continue };

            if !is_safe_component(name) || !keep(name) {
                continue;
            }

            names.push(name.to_string());
        }

        // `read_dir` order is filesystem-dependent. Sorting makes output
        // stable across runs, which the golden-file tests in chunk 7 depend
        // on and which stops the TUI from reordering rows at random.
        names.sort();

        Ok(Ok(names))
    }

    /// Read a symlink's raw target string, without following it.
    ///
    /// Confinement here differs from every other method on purpose.
    /// `resolve` canonicalises the *entire* path, which means following the
    /// very symlink this method exists to inspect — wrong for the case it's
    /// actually for, `/proc/<pid>/fd/N`, whose target is a synthetic string
    /// like `socket:[12345]` that `canonicalize` cannot resolve as a real
    /// path at all (there is no such file).
    ///
    /// Instead, `rel`'s *parent* goes through the same full [`Self::resolve`]
    /// every other method uses — component allowlist, `..`/absolute
    /// rejection, and symlink containment for every directory along the way
    /// — and only the final component, the symlink itself, is left
    /// unresolved and read with [`std::fs::read_link`]. A symlink whose
    /// *target* happens to point outside the confined root is not rejected
    /// by this method: the target is never opened, listed, or resolved
    /// through this API, so it cannot be used to escape confinement — it's
    /// returned as a plain string, the same way file *contents* are.
    /// Nothing downstream may treat that string as a path back into this
    /// reader without going through `resolve` itself, same as every other
    /// string this module hands back.
    pub fn read_link(&self, rel: &Path) -> Result<std::result::Result<String, Absence>> {
        let file_name = match rel.file_name().and_then(|n| n.to_str()) {
            Some(name) if is_safe_component(name) => name,
            _ => {
                return Err(Error::PathRejected {
                    path: rel.to_path_buf(),
                    reason: "missing file name or unsafe final component".into(),
                })
            }
        };

        let parent = rel.parent().unwrap_or_else(|| Path::new(""));
        let canonical_parent = match self.resolve(parent)? {
            Ok(path) => path,
            Err(absence) => return Ok(Err(absence)),
        };

        let target_path = canonical_parent.join(file_name);
        match std::fs::read_link(&target_path) {
            Ok(target) => Ok(Ok(target.to_string_lossy().into_owned())),
            Err(source) => Error::classify_io(&target_path, source).map(Err),
        }
    }

    /// True if the path exists and is readable, without reading it.
    ///
    /// A refused path is also `false`: from a caller's point of view "you may
    /// not have this" and "it isn't there" both mean don't try.
    pub fn exists(&self, rel: &Path) -> bool {
        matches!(self.resolve(rel), Ok(Ok(_)))
    }
}

impl Default for SysfsReader {
    fn default() -> Self {
        Self::new()
    }
}

/// Build a [`Error::Parse`] for a value that didn't match its expected shape.
///
/// The offending text is sanitised before it goes anywhere near an error
/// message — this is the chunk 1 carry-forward. File contents are
/// kernel-supplied and attacker-influenced in some setups, and an error string
/// ends up on a terminal.
fn parse_error(rel: &Path, expected: &str, got: &str) -> Error {
    Error::parse(
        rel,
        Some(1),
        format!(
            "expected {expected}, got {:?}",
            sanitize_kernel_string(got, 64)
        ),
    )
}

/// Is this directory-entry name safe to use as a path component?
///
/// Rejects: empty, `.`, `..`, anything containing `/` or NUL, anything starting
/// with `.`, anything longer than 255 bytes, and anything outside
/// `[A-Za-z0-9_.:-]`. The allowlist is deliberate — every real device name we
/// care about (`sda`, `hwmon3`, `enp0s31f6`, `card0`, `nvme0n1p2`, `wwan0`)
/// fits inside it, so there's no reason to accept anything wider.
pub fn is_safe_component(name: &str) -> bool {
    // NAME_MAX. Checked in bytes, not chars, because that is what the kernel
    // limits.
    if name.is_empty() || name.len() > 255 {
        return false;
    }

    // Covers `.` and `..` as a side effect, and keeps us out of dotfiles
    // generally. No `/sys` or `/proc` name this crate reads begins with a dot.
    if name.starts_with('.') {
        return false;
    }

    // Allowlist, not denylist: `/` and NUL are excluded by not appearing in
    // it, rather than by being remembered. Every real name we care about
    // (`sda`, `hwmon3`, `enp0s31f6`, `nvme0n1p2`, `0000:00:02.0`, `dm-0`,
    // `eth0.100`) fits, so there is no reason to accept anything wider.
    name.bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b':' | b'-'))
}

/// Is this a character that must never reach a terminal or a JSON string?
///
/// C0 controls, DEL, and the C1 block. C1 matters because `0x9b` is a
/// single-byte CSI introducer on some terminals — stripping ESC alone is not
/// enough.
fn is_control_char(c: char) -> bool {
    let code = c as u32;
    code < 0x20 || code == 0x7f || (0x80..=0x9f).contains(&code)
}

/// Consume a CSI sequence's parameter bytes and its final byte.
///
/// ECMA-48: parameters and intermediates are `0x20..=0x3f`, the final byte is
/// `0x40..=0x7e`. Anything else means the sequence was malformed, and we stop
/// rather than keep eating — dropping one stray character is better than
/// swallowing the rest of a legitimate label.
fn consume_csi(chars: &mut Peekable<Chars<'_>>) {
    for c in chars.by_ref() {
        let code = c as u32;
        if (0x40..=0x7e).contains(&code) || !(0x20..=0x3f).contains(&code) {
            break;
        }
    }
}

/// Consume an OSC sequence up to its terminator (BEL, or ST in either the
/// `ESC \` or the single-byte `0x9c` form).
///
/// An unterminated OSC consumes the remainder of the string. That is the safe
/// direction: an OSC left open on a real terminal swallows subsequent input,
/// so emitting nothing beats emitting a fragment that reopens it.
fn consume_osc(chars: &mut Peekable<Chars<'_>>) {
    while let Some(c) = chars.next() {
        match c {
            '\u{7}' | '\u{9c}' => break,
            '\u{1b}' => {
                if chars.peek() == Some(&'\\') {
                    chars.next();
                }
                break;
            }
            _ => {}
        }
    }
}

/// Strip anything that could rewrite the terminal or break JSON out of a
/// kernel-supplied string.
///
/// Removes C0/C1 control characters and ESC-introduced sequences, then
/// truncates to `max_len` characters. Sensor labels and device names are
/// attacker-influenced on some systems (a USB device supplies its own name),
/// and both the TUI and the JSON writer render them. Security model item 7.
pub fn sanitize_kernel_string(s: &str, max_len: usize) -> String {
    let mut out = String::new();
    let mut kept = 0usize;
    let mut chars = s.chars().peekable();

    while kept < max_len {
        let Some(c) = chars.next() else { break };

        match c {
            // ESC introduces a sequence. Dropping the ESC alone would leave
            // the body behind — `\x1b[31m` would render as a visible `[31m` —
            // so the introducer decides how much more to discard.
            '\u{1b}' => match chars.peek() {
                Some('[') => {
                    chars.next();
                    consume_csi(&mut chars);
                }
                Some(']') => {
                    chars.next();
                    consume_osc(&mut chars);
                }
                // A two-character escape such as `ESC c` (full terminal
                // reset). Drop both.
                Some(_) => {
                    chars.next();
                }
                None => {}
            },
            // Single-byte C1 forms of the same two introducers.
            '\u{9b}' => consume_csi(&mut chars),
            '\u{9d}' => consume_osc(&mut chars),
            c if is_control_char(c) => {}
            c => {
                out.push(c);
                kept += 1;
            }
        }
    }

    out
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU32, Ordering};

    static TREE_SEQ: AtomicU32 = AtomicU32::new(0);

    /// Minimal `tempdir` stand-in, hand-rolled because chunk 2 is scoped to
    /// zero external crates (`CLAUDE.md` rule 4) and a dev-dependency would
    /// still be a dependency.
    ///
    /// The directory name mixes the process id with a process-wide counter:
    /// cargo runs tests in parallel by default, and a fixed name would make
    /// these tests collide with each other intermittently.
    ///
    /// `pub(crate)`: reused by other collectors' fixture-tree tests (chunk 5
    /// onwards) so they don't each duplicate this scaffolding.
    pub(crate) struct TempTree {
        root: PathBuf,
    }

    impl TempTree {
        pub(crate) fn new(tag: &str) -> Self {
            let seq = TREE_SEQ.fetch_add(1, Ordering::Relaxed);
            let mut root = std::env::temp_dir();
            root.push(format!("rustmon-{}-{}-{}", tag, std::process::id(), seq));

            fs::create_dir_all(&root).expect("create fixture root");

            // Canonicalise: the system temp dir is itself a symlink on some
            // platforms, and a containment assertion against an
            // uncanonicalised root would fail for the wrong reason.
            let root = fs::canonicalize(&root).expect("canonicalise fixture root");

            TempTree { root }
        }

        pub(crate) fn dir(&self, rel: &str) -> &Self {
            fs::create_dir_all(self.root.join(rel)).expect("create fixture dir");
            self
        }

        pub(crate) fn file(&self, rel: &str, contents: &str) -> &Self {
            let path = self.root.join(rel);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).expect("create fixture parent");
            }
            fs::write(&path, contents).expect("write fixture file");
            self
        }

        /// Remove a previously-created entry — used to simulate a device
        /// disappearing mid-session (a kernel module unloading, a device
        /// being unplugged) between two collector runs against the same tree.
        pub(crate) fn remove(&self, rel: &str) -> &Self {
            let path = self.root.join(rel);
            let _ = fs::remove_dir_all(&path);
            let _ = fs::remove_file(&path);
            self
        }

        #[cfg(unix)]
        pub(crate) fn symlink(&self, rel: &str, target: &Path) -> &Self {
            let path = self.root.join(rel);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).expect("create fixture parent");
            }
            std::os::unix::fs::symlink(target, &path).expect("create fixture symlink");
            self
        }

        pub(crate) fn reader(&self) -> SysfsReader {
            SysfsReader::with_root(self.root.clone()).expect("open fixture root")
        }

        pub(crate) fn path(&self) -> &Path {
            &self.root
        }
    }

    impl Drop for TempTree {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn rel(p: &str) -> &Path {
        Path::new(p)
    }

    /// Collapse the nested `Result<Result<T, Absence>, Error>` to the value,
    /// failing loudly on either of the other two outcomes.
    fn value<T>(got: Result<std::result::Result<T, Absence>>) -> T {
        match got {
            Ok(Ok(v)) => v,
            Ok(Err(absence)) => panic!("expected a value, got absence {absence:?}"),
            Err(e) => panic!("expected a value, got error: {e}"),
        }
    }

    fn absence<T: std::fmt::Debug>(got: Result<std::result::Result<T, Absence>>) -> Absence {
        match got {
            Ok(Err(absence)) => absence,
            Ok(Ok(v)) => panic!("expected an absence, got value {v:?}"),
            Err(e) => panic!("expected an absence, got error: {e}"),
        }
    }

    fn refusal<T: std::fmt::Debug>(got: Result<std::result::Result<T, Absence>>) -> Error {
        match got {
            Err(e) => e,
            Ok(other) => panic!("expected a refusal, got {other:?}"),
        }
    }

    // ---- is_safe_component -------------------------------------------------

    #[test]
    fn safe_components_cover_the_real_device_names() {
        for name in [
            "sda",
            "nvme0n1p2",
            "dm-0",
            "mmcblk0",
            "hwmon3",
            "temp1_input",
            "enp0s31f6",
            "wlp3s0",
            "eth0.100",
            "br-1a2b3c",
            "card0",
            "renderD128",
            "0000:00:02.0",
            "thermal_zone0",
        ] {
            assert!(is_safe_component(name), "{name} should be accepted");
        }
    }

    #[test]
    fn unsafe_components_are_rejected() {
        for name in [
            "",
            ".",
            "..",
            ".hidden",
            "a/b",
            "a\u{0}b",
            "a b",
            "a*b",
            "a$b",
            "a\nb",
            "a\u{1b}[31m",
            "~root",
            "a|b",
            "../etc",
        ] {
            assert!(!is_safe_component(name), "{name:?} should be rejected");
        }

        // NAME_MAX, checked in bytes.
        assert!(is_safe_component(&"a".repeat(255)));
        assert!(!is_safe_component(&"a".repeat(256)));
    }

    // ---- sanitize_kernel_string -------------------------------------------

    #[test]
    fn sanitize_strips_control_characters() {
        assert_eq!(sanitize_kernel_string("Core 0", 64), "Core 0");
        assert_eq!(sanitize_kernel_string("a\u{0}b\u{1}c", 64), "abc");
        assert_eq!(sanitize_kernel_string("a\u{7f}b", 64), "ab");
        assert_eq!(sanitize_kernel_string("a\u{9f}b", 64), "ab");
        assert_eq!(sanitize_kernel_string("line\nbreak\ttab", 64), "linebreaktab");
    }

    /// A device that names itself `ESC [ 2 J` would otherwise clear the screen
    /// the moment the TUI draws it, and stripping the ESC alone would leave a
    /// visible `[2J` behind. Security model item 7.
    #[test]
    fn sanitize_drops_whole_ansi_sequences_not_just_the_escape() {
        assert_eq!(sanitize_kernel_string("\u{1b}[31mRED\u{1b}[0m", 64), "RED");
        assert_eq!(sanitize_kernel_string("before\u{1b}[2Jafter", 64), "beforeafter");
        // Single-byte C1 CSI — no ESC involved at all.
        assert_eq!(sanitize_kernel_string("x\u{9b}31my", 64), "xy");
        // Two-character escape: ESC c is a full terminal reset.
        assert_eq!(sanitize_kernel_string("\u{1b}csafe", 64), "safe");
    }

    #[test]
    fn sanitize_drops_osc_sequences_including_unterminated_ones() {
        assert_eq!(sanitize_kernel_string("\u{1b}]0;title\u{7}rest", 64), "rest");
        assert_eq!(sanitize_kernel_string("\u{1b}]0;title\u{1b}\\rest", 64), "rest");
        // Unterminated: swallowing the remainder beats emitting a fragment
        // that leaves a real terminal waiting for a terminator.
        assert_eq!(sanitize_kernel_string("keep\u{1b}]0;forever", 64), "keep");
    }

    #[test]
    fn sanitize_truncates_by_characters_not_bytes() {
        assert_eq!(sanitize_kernel_string("abcdefgh", 3), "abc");
        assert_eq!(sanitize_kernel_string("abc", 0), "");
        // Must not split a multi-byte character mid-encoding.
        assert_eq!(sanitize_kernel_string("°°°°", 2), "°°");
        assert_eq!(sanitize_kernel_string("Package id 0 °C", 64), "Package id 0 °C");
    }

    // ---- reads -------------------------------------------------------------

    #[test]
    fn reads_and_parses_scalar_files() {
        let tree = TempTree::new("scalar");
        tree.file("sys/class/hwmon/hwmon0/name", "coretemp\n");
        tree.file("sys/class/hwmon/hwmon0/temp1_input", "45678\n");
        tree.file("sys/class/hwmon/hwmon0/temp2_input", "-4250\n");
        let r = tree.reader();

        assert_eq!(
            value(r.read_first_line(rel("sys/class/hwmon/hwmon0/name"))),
            "coretemp"
        );
        assert_eq!(
            value(r.read_u64(rel("sys/class/hwmon/hwmon0/temp1_input"))),
            45_678
        );
        // Signed, because sensors below freezing are legitimate.
        assert_eq!(
            value(r.read_i64(rel("sys/class/hwmon/hwmon0/temp2_input"))),
            -4_250
        );
    }

    #[test]
    fn garbage_in_a_numeric_file_is_a_parse_error_not_a_panic() {
        let tree = TempTree::new("garbage");
        tree.file("sys/bad", "not a number\n");
        tree.file("sys/negative", "-1\n");
        let r = tree.reader();

        let err = refusal(r.read_u64(rel("sys/bad")));
        assert!(matches!(err, Error::Parse { .. }), "got {err:?}");

        // A negative value in an unsigned field is a parse failure too, not a
        // silent wrap to a huge u64.
        let err = refusal(r.read_u64(rel("sys/negative")));
        assert!(matches!(err, Error::Parse { .. }), "got {err:?}");
        assert_eq!(value(r.read_i64(rel("sys/negative"))), -1);
    }

    /// The offending text reaches an error message, so it must be sanitised
    /// there — the chunk 1 carry-forward.
    #[test]
    fn a_parse_error_does_not_carry_terminal_escapes() {
        let tree = TempTree::new("escapes");
        tree.file("sys/hostile", "\u{1b}[2J\u{1b}[31mgotcha\n");
        let r = tree.reader();

        let rendered = refusal(r.read_u64(rel("sys/hostile"))).to_string();
        assert!(!rendered.contains('\u{1b}'), "escape survived: {rendered:?}");
        assert!(rendered.contains("gotcha"), "{rendered:?}");
    }

    #[test]
    fn an_empty_file_is_not_reported_rather_than_an_empty_string() {
        let tree = TempTree::new("empty");
        tree.file("sys/empty", "");
        tree.file("sys/blank", "   \n");
        let r = tree.reader();

        assert_eq!(absence(r.read_first_line(rel("sys/empty"))), Absence::NotReported);
        assert_eq!(absence(r.read_first_line(rel("sys/blank"))), Absence::NotReported);
        assert_eq!(absence(r.read_u64(rel("sys/empty"))), Absence::NotReported);
    }

    #[test]
    fn a_missing_file_is_absent_not_an_error() {
        let tree = TempTree::new("missing");
        tree.dir("sys");
        let r = tree.reader();

        assert_eq!(absence(r.read_first_line(rel("sys/nope"))), Absence::NotPresent);
        assert_eq!(absence(r.read_u64(rel("sys/nope"))), Absence::NotPresent);
        assert_eq!(
            absence(r.list_dir(rel("sys/nodir"), &|_| true)),
            Absence::NotPresent
        );
    }

    #[cfg(unix)]
    #[test]
    fn an_unreadable_file_is_absent_not_a_panic() {
        use std::os::unix::fs::PermissionsExt;

        let tree = TempTree::new("eacces");
        tree.file("sys/secret", "hidden\n");
        let path = tree.path().join("sys/secret");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).expect("chmod");

        // Root ignores mode bits, so the assertion below is only meaningful
        // unprivileged. Ask the question directly rather than looking up a
        // uid — it is the same question, answered honestly.
        if fs::read(&path).is_ok() {
            return;
        }

        assert_eq!(
            absence(tree.reader().read_first_line(rel("sys/secret"))),
            Absence::PermissionDenied
        );
    }

    // ---- confinement -------------------------------------------------------

    #[test]
    fn traversal_attempts_are_refused() {
        let tree = TempTree::new("traversal");
        tree.file("sys/present", "1\n");
        let r = tree.reader();

        for attempt in ["../../etc/passwd", "a/../../b", "..", "./sys/present"] {
            let err = refusal(r.read_first_line(rel(attempt)));
            assert!(
                matches!(err, Error::PathRejected { .. }),
                "{attempt:?} produced {err:?}"
            );
        }
    }

    #[test]
    fn absolute_paths_are_refused() {
        let tree = TempTree::new("absolute");
        tree.dir("sys");
        let r = tree.reader();

        let err = refusal(r.read_first_line(rel("/etc/passwd")));
        assert!(matches!(err, Error::PathRejected { .. }), "got {err:?}");
    }

    /// The test this whole module exists for. `/sys` is largely symlinks, so a
    /// textual prefix check would let a link straight out of the root.
    #[cfg(unix)]
    #[test]
    fn a_symlink_pointing_out_of_the_root_is_refused() {
        let outside = TempTree::new("outside");
        outside.file("passwd", "root:x:0:0\n");

        let tree = TempTree::new("escape");
        tree.dir("sys");
        tree.symlink("sys/escape", outside.path());
        let r = tree.reader();

        let err = refusal(r.read_first_line(rel("sys/escape/passwd")));
        assert!(matches!(err, Error::PathRejected { .. }), "got {err:?}");

        let err = refusal(r.list_dir(rel("sys/escape"), &|_| true));
        assert!(matches!(err, Error::PathRejected { .. }), "got {err:?}");

        assert!(!r.exists(rel("sys/escape/passwd")));
    }

    /// The counterpart: `/sys/class/hwmon/hwmon0` really is a symlink into
    /// `/sys/devices/...`, so following links *within* the root must work or
    /// the crate reads nothing on a real machine.
    #[cfg(unix)]
    #[test]
    fn a_symlink_inside_the_root_is_followed() {
        let tree = TempTree::new("inner");
        tree.file("sys/devices/chip/temp1_input", "42000\n");
        tree.dir("sys/class/hwmon");
        tree.symlink(
            "sys/class/hwmon/hwmon0",
            &tree.path().join("sys/devices/chip"),
        );
        let r = tree.reader();

        assert_eq!(
            value(r.read_u64(rel("sys/class/hwmon/hwmon0/temp1_input"))),
            42_000
        );
    }

    // ---- read_link -----------------------------------------------------------

    #[cfg(unix)]
    #[test]
    fn read_link_returns_the_raw_target_of_a_safe_symlink() {
        let tree = TempTree::new("readlink-safe");
        tree.file("real/target", "1\n");
        tree.symlink("proc/1/fd/3", Path::new("../../real/target"));
        let r = tree.reader();

        assert_eq!(
            value(r.read_link(rel("proc/1/fd/3"))),
            "../../real/target"
        );
    }

    /// The whole reason this method exists: `/proc/<pid>/fd/N` for a socket
    /// points at a string that is not a real path at all, and `read_link`
    /// must hand it back unchanged rather than trying (and failing) to
    /// resolve it as one.
    #[cfg(unix)]
    #[test]
    fn read_link_returns_a_synthetic_procfs_style_target() {
        let tree = TempTree::new("readlink-socket");
        tree.dir("proc/1/fd");
        tree.symlink("proc/1/fd/4", Path::new("socket:[12345]"));
        let r = tree.reader();

        assert_eq!(value(r.read_link(rel("proc/1/fd/4"))), "socket:[12345]");
    }

    /// The key property that makes this method safe despite reading past
    /// the confined root: a symlink's *target* pointing outside the root is
    /// fine and must not be rejected, because that target is only ever
    /// returned as a string here, never opened or resolved back through
    /// this reader. This is deliberately the opposite outcome from every
    /// other confinement test in this module, and that difference is the
    /// point.
    #[cfg(unix)]
    #[test]
    fn read_link_returns_the_target_string_even_when_it_points_outside_the_root() {
        let outside = TempTree::new("readlink-outside");
        let tree = TempTree::new("readlink-escaping-target");
        tree.dir("proc/1/fd");
        tree.symlink("proc/1/fd/5", &outside.path().join("secret"));
        let r = tree.reader();

        let got = value(r.read_link(rel("proc/1/fd/5")));
        assert_eq!(got, outside.path().join("secret").to_string_lossy());
    }

    /// Unlike the target, the symlink's own *path* still goes through full
    /// confinement — a traversal attempt in `rel` itself must be rejected
    /// exactly like every other method rejects one.
    #[test]
    fn read_link_rejects_a_traversal_attempt_in_its_own_path() {
        let tree = TempTree::new("readlink-traversal");
        let r = tree.reader();

        let err = refusal(r.read_link(rel("../../etc/passwd")));
        assert!(matches!(err, Error::PathRejected { .. }), "got {err:?}");

        let err = refusal(r.read_link(rel("proc/../../../etc/passwd")));
        assert!(matches!(err, Error::PathRejected { .. }), "got {err:?}");
    }

    /// If an *intermediate* directory in the symlink's own path escapes the
    /// root, that must still be caught — this is the parity check proving
    /// `read_link` inherits `resolve`'s full containment for everything
    /// except the final component.
    #[cfg(unix)]
    #[test]
    fn read_link_rejects_a_path_whose_parent_escapes_the_root() {
        let outside = TempTree::new("readlink-parent-outside");
        let tree = TempTree::new("readlink-parent-escape");
        tree.dir("proc/1");
        tree.symlink("proc/1/fd", outside.path());
        let r = tree.reader();

        let err = refusal(r.read_link(rel("proc/1/fd/3")));
        assert!(matches!(err, Error::PathRejected { .. }), "got {err:?}");
    }

    #[test]
    fn read_link_on_a_missing_path_is_absent_not_an_error() {
        let tree = TempTree::new("readlink-missing");
        tree.dir("proc/1/fd");
        let r = tree.reader();

        assert_eq!(
            absence(r.read_link(rel("proc/1/fd/99"))),
            Absence::NotPresent
        );
    }

    /// `read_link` on something that exists but isn't a symlink is a real,
    /// unexpected condition (a garbled fixture, or a caller bug) — not
    /// something to paper over as an absence.
    #[test]
    fn read_link_on_a_regular_file_is_a_hard_error_not_a_panic() {
        let tree = TempTree::new("readlink-not-a-symlink");
        tree.file("proc/1/fd/6", "not a symlink\n");
        let r = tree.reader();

        assert!(r.read_link(rel("proc/1/fd/6")).is_err());
    }

    // ---- bounds ------------------------------------------------------------

    #[test]
    fn reads_stop_at_the_size_cap() {
        let tree = TempTree::new("cap");
        tree.file("proc/huge", &"a".repeat(2 * 1024 * 1024));

        let capped = value(tree.reader().read_to_string(rel("proc/huge")));
        assert_eq!(capped.len(), DEFAULT_MAX_READ_BYTES);

        let tighter = tree.reader().with_max_read_bytes(10);
        assert_eq!(value(tighter.read_to_string(rel("proc/huge"))).len(), 10);
    }

    #[test]
    fn list_dir_filters_unsafe_names_sorts_and_caps() {
        let tree = TempTree::new("list");
        tree.dir("sys/class/net/eth0");
        tree.dir("sys/class/net/lo");
        tree.dir("sys/class/net/wlan0");
        // Rejected by the component allowlist, so it must never be listed.
        tree.dir("sys/class/net/.hidden");
        let r = tree.reader();

        assert_eq!(
            value(r.list_dir(rel("sys/class/net"), &|_| true)),
            vec!["eth0", "lo", "wlan0"]
        );

        assert_eq!(
            value(r.list_dir(rel("sys/class/net"), &|n| n.starts_with('w'))),
            vec!["wlan0"]
        );

        // The cap counts entries examined, so a hostile directory costs a
        // bounded amount of work regardless of what `keep` says.
        let capped = tree.reader().with_max_entries(2);
        assert_eq!(value(capped.list_dir(rel("sys/class/net"), &|_| true)).len(), 2);
    }

    // ---- construction ------------------------------------------------------

    #[test]
    fn with_root_rejects_a_file_and_a_missing_directory() {
        let tree = TempTree::new("ctor");
        tree.file("notadir", "x\n");

        assert!(matches!(
            SysfsReader::with_root(tree.path().join("notadir")),
            Err(Error::PathRejected { .. })
        ));
        assert!(matches!(
            SysfsReader::with_root(tree.path().join("nope")),
            Err(Error::Io { .. })
        ));
    }

    #[test]
    fn default_reader_is_rooted_at_slash() {
        assert_eq!(SysfsReader::default().root(), Path::new("/"));
        assert_eq!(SysfsReader::new().max_read_bytes(), DEFAULT_MAX_READ_BYTES);
        assert_eq!(SysfsReader::new().max_lines(), DEFAULT_MAX_LINES);
        assert_eq!(SysfsReader::new().max_entries(), DEFAULT_MAX_ENTRIES);
    }

    #[test]
    fn exists_is_false_for_missing_and_refused_paths() {
        let tree = TempTree::new("exists");
        tree.file("sys/present", "1\n");
        let r = tree.reader();

        assert!(r.exists(rel("sys/present")));
        assert!(!r.exists(rel("sys/absent")));
        assert!(!r.exists(rel("../../etc/passwd")));
    }
}
