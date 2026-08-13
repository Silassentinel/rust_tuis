//! Block devices and mounted filesystems.
//!
//! Sources:
//! - `proc/diskstats` — per-device counters. **Sectors here are always 512
//!   bytes**, a fixed kernel convention independent of the device's real
//!   logical or physical sector size. Using a 4096-byte device's actual sector
//!   size gives throughput 8x too high; it's the classic bug in this file, and
//!   [`crate::units::Bytes::from_sectors_512`] exists to make it hard to write.
//! - `proc/self/mounts` — mounted filesystems. Preferred over `/etc/mtab`
//!   (which can be a stale regular file) and over `/proc/mounts` (which shows
//!   the host namespace, not ours — using `self/` means a container reports its
//!   own mounts, which is what a user in that container expects).
//!
//! Capacity/free numbers need `statvfs(3)`, which `std` does not expose —
//! that's the `fs-capacity` feature (crate proposal "rustmon A", approved
//! 2026-08-12), backed by `nix::sys::statvfs`. With the feature off (the
//! default until a caller opts in), `read_capacity` is a no-op stub and the
//! `total`/`available` fields stay `None`; the throughput half is unaffected
//! either way.

use std::path::Path;

use crate::collector::Collector;
use crate::error::{Error, Result};
use crate::sample::{DiskDevice, DiskSample, MountPoint, Snapshot};
use crate::sysfs::{is_safe_component, sanitize_kernel_string, SysfsReader, DEFAULT_MAX_LINES};

/// Number of counter fields read from each `/proc/diskstats` line, starting
/// right after `major minor name`. Kernels have added fields over time (11 on
/// old kernels, up to 20 on new ones for discard/flush stats) — only the
/// first 11 are guaranteed to exist on every kernel this crate might run
/// against, and they're all this crate needs.
const DISKSTATS_FIELDS: usize = 11;

/// Cap on a mount `source`/`mount_point`/`fs_type` string before it reaches
/// a terminal or JSON output. Generous relative to the label caps elsewhere
/// in this crate (64 for a sensor label) because real filesystem paths are
/// legitimately much longer than a hwmon label ever is — `PATH_MAX` on Linux
/// is 4096.
const MAX_MOUNT_FIELD_LEN: usize = 4096;

pub const NAME: &str = "disk";

const PROC_DISKSTATS: &str = "proc/diskstats";
const PROC_MOUNTS: &str = "proc/self/mounts";

/// Device-name prefixes that are never real hardware. Filtering these out is
/// cosmetic, not security — but a list of 40 `loop` devices makes the real ones
/// unfindable, which is a usability failure in a monitor.
pub const VIRTUAL_DEVICE_PREFIXES: &[&str] = &["loop", "ram", "zram", "dm-", "md"];

/// Filesystem types that don't represent storage and would clutter the mount
/// list with entries whose capacity is meaningless.
pub const PSEUDO_FILESYSTEMS: &[&str] = &[
    "proc", "sysfs", "devtmpfs", "devpts", "tmpfs", "cgroup", "cgroup2", "securityfs", "debugfs",
    "tracefs", "pstore", "bpf", "configfs", "fusectl", "mqueue", "hugetlbfs", "autofs", "overlay",
    "squashfs", "ramfs", "binfmt_misc", "efivarfs", "nsfs",
];

#[derive(Debug, Default)]
pub struct DiskCollector;

impl DiskCollector {
    pub fn new() -> Self {
        // Unit struct — there is nothing to cache between refreshes.
        DiskCollector
    }
}

impl Collector for DiskCollector {
    fn name(&self) -> &'static str {
        NAME
    }

    fn probe(&self, reader: &SysfsReader) -> bool {
        reader.exists(Path::new(PROC_DISKSTATS))
    }

    fn collect(&mut self, reader: &SysfsReader, snapshot: &mut Snapshot) -> Result<()> {
        let devices = match reader.read_to_string(Path::new(PROC_DISKSTATS))? {
            Ok(contents) => parse_diskstats(Path::new(PROC_DISKSTATS), &contents)?,
            Err(_) => Vec::new(),
        };

        let mut mounts = match reader.read_to_string(Path::new(PROC_MOUNTS))? {
            Ok(contents) => parse_mounts(Path::new(PROC_MOUNTS), &contents)?,
            Err(_) => Vec::new(),
        };

        // Capacity is a separate I/O step from parsing on purpose: it keeps
        // `parse_mounts` a pure function of a string, testable against a
        // fixture with no filesystem involved. `read_capacity` itself is a
        // no-op stub until the `fs-capacity` crate proposal is signed off.
        for mount in &mut mounts {
            let (total, available) = read_capacity(&mount.mount_point)?.unzip();
            mount.total = total;
            mount.available = available;
        }

        snapshot.disks = if devices.is_empty() && mounts.is_empty() {
            None
        } else {
            Some(DiskSample { devices, mounts })
        };

        Ok(())
    }
}

/// Parse `/proc/diskstats`.
///
/// Format: `major minor name` then 11 (older kernels) to 20 (newer) counters.
/// Reading by field index past the 11th is therefore unsafe across kernel
/// versions — only the first 11 are relied on here, and a short line is an
/// `Error::Parse`, not a silent zero.
pub fn parse_diskstats(path: &Path, contents: &str) -> Result<Vec<DiskDevice>> {
    let mut devices = Vec::new();

    for (idx, line) in contents.lines().take(DEFAULT_MAX_LINES).enumerate() {
        if let Some(device) = parse_diskstats_line(path, idx + 1, line)? {
            devices.push(device);
        }
    }

    // The partition/virtual-device filter needs every name that was actually
    // present, not just the ones already kept — "is `sda1` a partition of
    // `sda`" has to be answered against the full listing.
    let all_names: Vec<String> = devices.iter().map(|d| d.name.clone()).collect();
    devices.retain(|d| is_interesting_device(&d.name, &all_names));

    Ok(devices)
}

/// One diskstats line.
///
/// `/proc/diskstats` has no header lines — unlike `/proc/net/dev` — so every
/// non-blank line is expected to be well-formed data. A line with too few
/// fields, a non-numeric counter, or an unsafe device name is `Error::Parse`,
/// not a silently-dropped device: something is wrong with the source itself,
/// not just one device's optional decoration.
pub fn parse_diskstats_line(path: &Path, line_no: usize, line: &str) -> Result<Option<DiskDevice>> {
    if line.trim().is_empty() {
        return Ok(None);
    }

    let mut fields = line.split_whitespace();

    // major, minor: present in every line but not stored — the device name
    // is the identity used everywhere else in this crate (rate matching,
    // display), and duplicating major:minor as a second identity would just
    // be one more thing that could disagree with it.
    let _major = fields.next();
    let _minor = fields.next();

    let name = fields.next().ok_or_else(|| {
        Error::parse(path, Some(line_no), "line has no device name")
    })?;

    if !is_safe_component(name) {
        return Err(Error::parse(
            path,
            Some(line_no),
            format!(
                "unsafe device name {:?}",
                crate::sysfs::sanitize_kernel_string(name, 32)
            ),
        ));
    }

    let mut counters = [0u64; DISKSTATS_FIELDS];
    let mut seen = 0usize;

    for (idx, (slot, field)) in counters.iter_mut().zip(fields).enumerate() {
        *slot = field.parse::<u64>().map_err(|_| {
            Error::parse(
                path,
                Some(line_no),
                format!(
                    "counter field {} is not an unsigned integer: {:?}",
                    idx + 1,
                    field
                ),
            )
        })?;
        seen = idx + 1;
    }

    if seen < DISKSTATS_FIELDS {
        return Err(Error::parse(
            path,
            Some(line_no),
            format!("expected {DISKSTATS_FIELDS} counter fields, got {seen}"),
        ));
    }

    Ok(Some(DiskDevice {
        name: name.to_string(),
        reads_completed: counters[0],
        // counters[1] is reads merged — not tracked.
        sectors_read: counters[2],
        // counters[3] is time spent reading — not tracked.
        writes_completed: counters[4],
        // counters[5] is writes merged — not tracked.
        sectors_written: counters[6],
        // counters[7] is time spent writing, counters[8] is I/Os in
        // progress — not tracked.
        io_ticks_ms: counters[9],
        // counters[10] is weighted I/O time — not tracked.
    }))
}

/// Should this device be shown?
///
/// Filters [`VIRTUAL_DEVICE_PREFIXES`] and partitions of a device already in
/// the list (`sda1` when `sda` is present) — showing both double-counts the
/// same physical I/O.
///
/// Two partition-naming conventions exist on real hardware: `sda` → `sda1`
/// (bare digit suffix) and `nvme0n1` → `nvme0n1p1` (`p`-then-digit suffix,
/// also used by `mmcblk0`). Both are checked.
pub fn is_interesting_device(name: &str, all_names: &[String]) -> bool {
    if VIRTUAL_DEVICE_PREFIXES
        .iter()
        .any(|prefix| name.starts_with(prefix))
    {
        return false;
    }

    let is_partition_suffix = |suffix: &str| {
        !suffix.is_empty()
            && (suffix.bytes().all(|b| b.is_ascii_digit())
                || (suffix.starts_with('p')
                    && suffix.len() > 1
                    && suffix[1..].bytes().all(|b| b.is_ascii_digit())))
    };

    let is_partition_of_something_listed = all_names.iter().any(|other| {
        other != name
            && name
                .strip_prefix(other.as_str())
                .is_some_and(is_partition_suffix)
    });

    !is_partition_of_something_listed
}

/// Parse `/proc/self/mounts`.
///
/// Fields are space-separated with **octal escapes** (`\040` for space, `\011`
/// for tab, `\012` for newline, `\134` for backslash) in the source and
/// mount-point fields. Not decoding those mangles any path with a space in it.
pub fn parse_mounts(path: &Path, contents: &str) -> Result<Vec<MountPoint>> {
    let mut mounts = Vec::new();

    for (idx, line) in contents.lines().take(DEFAULT_MAX_LINES).enumerate() {
        if line.trim().is_empty() {
            continue;
        }

        // Splitting on raw whitespace is safe here specifically because
        // every raw space *inside* a field is escaped in the source (that's
        // the whole reason the octal-escape scheme exists) — so an
        // unescaped space always is a real field separator.
        let mut fields = line.split_whitespace();

        let source = fields.next().ok_or_else(|| {
            Error::parse(path, Some(idx + 1), "line has no source field")
        })?;
        let mount_point = fields.next().ok_or_else(|| {
            Error::parse(path, Some(idx + 1), "line has no mount point field")
        })?;
        let fs_type = fields.next().ok_or_else(|| {
            Error::parse(path, Some(idx + 1), "line has no filesystem type field")
        })?;
        // Remaining fields (options, dump frequency, pass number) aren't
        // stored — nothing in `MountPoint` uses them.

        if PSEUDO_FILESYSTEMS.contains(&fs_type) {
            continue;
        }

        // `unescape_mount_field` only decodes the four documented octal
        // escapes — it does not strip control characters, so a hostile mount
        // source (a crafted FUSE filesystem, an LVM volume name) could still
        // carry a raw ESC sequence through untouched. Unlike device and
        // interface names elsewhere in this crate, these fields aren't
        // charset-restricted by `is_safe_component` (a real path can contain
        // almost anything), so sanitizing here — at the collector boundary,
        // same as every other kernel-supplied string in this crate — is not
        // optional the way it would be for an already-charset-safe name.
        mounts.push(MountPoint {
            source: sanitize_kernel_string(&unescape_mount_field(source), MAX_MOUNT_FIELD_LEN),
            mount_point: sanitize_kernel_string(&unescape_mount_field(mount_point), MAX_MOUNT_FIELD_LEN),
            fs_type: sanitize_kernel_string(fs_type, MAX_MOUNT_FIELD_LEN),
            total: None,
            available: None,
        });
    }

    Ok(mounts)
}

/// Decode the octal escapes used in `/proc/*/mounts`.
///
/// Exactly four sequences appear in practice — `\040` (space), `\011` (tab),
/// `\012` (newline), `\134` (backslash) — because those are the only bytes
/// that could otherwise be mistaken for a field separator or a line ending.
/// A backslash that isn't the start of one of these four is passed through
/// unchanged rather than treated as an error: it's not this crate's job to
/// validate the kernel's own escaping, only to decode the documented cases.
pub fn unescape_mount_field(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();

    // Walking `chars()` rather than indexing bytes means a mount point
    // containing a real multi-byte UTF-8 character (a non-ASCII volume
    // label, say) is copied through intact — the escape sequences below are
    // always plain ASCII, so recognising them and falling back to whole
    // *characters* elsewhere can't split one.
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }

        // Look ahead for exactly three ASCII-digit characters without
        // consuming them from `chars` unless they form a known escape —
        // `clone` is the cheap way to "peek three ahead" on a char iterator.
        let mut lookahead = chars.clone();
        let digits: Option<String> = (0..3)
            .map(|_| lookahead.next().filter(char::is_ascii_digit))
            .collect();

        let decoded = digits.and_then(|d| match d.as_str() {
            "040" => Some(' '),
            "011" => Some('\t'),
            "012" => Some('\n'),
            "134" => Some('\\'),
            _ => None,
        });

        match decoded {
            Some(unescaped) => {
                out.push(unescaped);
                chars = lookahead; // commit: actually consume the 3 digits
            }
            // Not a recognised escape — the backslash was already consumed
            // by the outer `next()`; just push it through as-is and let the
            // following characters be handled on their own turns.
            None => out.push('\\'),
        }
    }

    out
}

/// How long to wait for `statvfs(3)` before giving up on one mount.
///
/// `statvfs` takes no timeout parameter, and on a stale or unreachable NFS
/// mount it can block for a long time (bounded by the mount's own RPC
/// timeout/retry settings, or not at all on a `hard` mount) — precisely the
/// hang this module's own placeholder used to warn about. [`read_capacity`]
/// runs the call on a helper thread and stops waiting after this long; the
/// syscall itself keeps running (there is no safe way to cancel a thread
/// blocked inside a syscall), so a genuinely hung mount leaks one thread
/// rather than hanging the collector. That's a bounded, honestly-documented
/// cost — `--once` still finishes and exits — not a fix for the underlying
/// hang, which no purely synchronous `statvfs` call can avoid.
#[cfg(feature = "fs-capacity")]
const STATVFS_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(500);

/// Filesystem capacity via `statvfs(3)`.
///
/// Capacity is decoration on top of the mount listing (same rule as
/// `read_link_state` in `net.rs`): any failure — permission denied, the mount
/// vanishing mid-refresh, overflow multiplying block size by block count, or
/// the timeout above — degrades to `Ok(None)` for this one mount rather than
/// failing the whole collector.
#[cfg(feature = "fs-capacity")]
pub fn read_capacity(mount_point: &str) -> Result<Option<(crate::units::Bytes, crate::units::Bytes)>> {
    use std::sync::mpsc;
    use std::thread;

    let mount_point = mount_point.to_string();
    let (tx, rx) = mpsc::channel();
    // Detached: on timeout this thread is abandoned, not joined. Its result
    // is discarded by the closed receiver whenever the syscall does return.
    thread::spawn(move || {
        let _ = tx.send(nix::sys::statvfs::statvfs(mount_point.as_str()));
    });

    let stat = match rx.recv_timeout(STATVFS_TIMEOUT) {
        Ok(Ok(stat)) => stat,
        Ok(Err(_)) | Err(_) => return Ok(None),
    };

    // `f_frsize` (fragment size) is the POSIX-preferred block-size unit for
    // `f_blocks`/`f_bavail`, over `f_bsize` — the same choice `df` makes.
    // `f_bavail` (not `f_bfree`) is "available to an unprivileged user",
    // matching what a user actually cares about: space reserved for root is
    // not space they can write into.
    //
    // `c_ulong`/`fsblkcnt_t` happen to already be `u64` on this crate's only
    // target (x86_64 Linux), which is exactly why clippy calls the cast
    // below redundant — but they're nominally distinct types from `u64`
    // regardless, so a conversion is needed to `checked_mul` them together at
    // all. Silenced, not removed: removing it would just be swapping one
    // platform-specific assumption (this cast is a no-op) for a stronger,
    // unstated one (these two libc type aliases will always match `u64`
    // exactly, on any target this ever runs on).
    #[allow(clippy::unnecessary_cast)]
    Ok(capacity_from_raw(
        stat.fragment_size() as u64,
        stat.blocks() as u64,
        stat.blocks_available() as u64,
    ))
}

/// Multiply block counts by fragment size into byte counts. Split out from
/// [`read_capacity`] so the arithmetic — the part that can actually go wrong
/// (overflow on a pathological or corrupted report) — is testable without a
/// real filesystem. Compiled whenever `fs-capacity` is on (production use) or
/// under `cfg(test)` regardless of feature (so `cargo test
/// --no-default-features` still exercises this arithmetic); outside both,
/// there is no caller and no reason for it to exist in the binary.
#[cfg(any(feature = "fs-capacity", test))]
fn capacity_from_raw(fragment_size: u64, blocks: u64, blocks_available: u64) -> Option<(crate::units::Bytes, crate::units::Bytes)> {
    let total = fragment_size.checked_mul(blocks)?;
    let available = fragment_size.checked_mul(blocks_available)?;
    Some((crate::units::Bytes::from_bytes(total), crate::units::Bytes::from_bytes(available)))
}

/// Stub used when the `fs-capacity` feature is off (the default, and the only
/// state until the crate proposal is signed off).
#[cfg(not(feature = "fs-capacity"))]
pub fn read_capacity(_mount_point: &str) -> Result<Option<(crate::units::Bytes, crate::units::Bytes)>> {
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p() -> &'static Path {
        Path::new("proc/diskstats")
    }

    /// Captured from this machine on 2026-08-07: real whole disks (`sda`,
    /// `sdb`, `sdc`, `sdd`) each with one partition, an NVMe drive with three
    /// partitions, two LVM `dm-` volumes, and a wall of `loop*` devices — the
    /// exact mix `is_interesting_device` exists to cut down to something a
    /// human can read.
    const REAL_DISKSTATS: &str = "\
   7       0 loop0 173 0 12588 27 0 0 0 0 0 23 27 0 0 0 0 0 0
   8       0 sda 158 0 9120 52 0 0 0 0 0 35 52 0 0 0 0 0 0
   8       1 sda1 57 0 4416 30 0 0 0 0 0 25 30 0 0 0 0 0 0
   8      16 sdb 127 0 7112 336 0 0 0 0 0 304 336 0 0 0 0 0 0
   8      17 sdb1 53 0 4480 297 0 0 0 0 0 270 297 0 0 0 0 0 0
 259       0 nvme0n1 141322 50019 6128821 21898 7630 5355 218050 5487 0 11573 27682 0 0 0 0 466 296
 259       1 nvme0n1p1 576 1730 14634 213 2 0 2 3 0 25 216 0 0 0 0 0 0
 259       2 nvme0n1p2 144 22 8978 25 19 13 232 19 0 35 45 0 0 0 0 0 0
 259       3 nvme0n1p3 140501 48267 6100649 21637 7609 5342 217816 5464 0 12716 27102 0 0 0 0 0 0
 252       0 dm-0 188728 0 6099514 41211 12947 0 217816 37016 0 13140 78227 0 0 0 0 0 0
 252       1 dm-1 188666 0 6097530 41263 12284 0 217816 34703 0 13171 75966 0 0 0 0 0 0
";

    #[test]
    fn parses_a_real_diskstats_and_filters_down_to_real_disks() {
        let devices = parse_diskstats(p(), REAL_DISKSTATS).expect("real fixture parses");
        let names: Vec<&str> = devices.iter().map(|d| d.name.as_str()).collect();

        // loop0, sda1, sdb1, nvme0n1p1/p2/p3, dm-0, dm-1 all filtered —
        // every partition-of-a-listed-disk and every virtual-prefix device.
        assert_eq!(names, vec!["sda", "sdb", "nvme0n1"]);
    }

    #[test]
    fn diskstats_counters_use_the_right_field_offsets() {
        let devices = parse_diskstats(p(), REAL_DISKSTATS).expect("parses");
        let sda = devices.iter().find(|d| d.name == "sda").expect("sda present");

        assert_eq!(sda.reads_completed, 158);
        assert_eq!(sda.sectors_read, 9_120);
        assert_eq!(sda.writes_completed, 0);
        assert_eq!(sda.sectors_written, 0);
        assert_eq!(sda.io_ticks_ms, 35);
    }

    #[test]
    fn is_interesting_device_recognises_both_partition_conventions() {
        let names: Vec<String> = ["sda", "sda1", "nvme0n1", "nvme0n1p1", "mmcblk0", "mmcblk0p1"]
            .iter()
            .map(|s| s.to_string())
            .collect();

        assert!(is_interesting_device("sda", &names));
        assert!(!is_interesting_device("sda1", &names), "bare-digit partition");
        assert!(is_interesting_device("nvme0n1", &names));
        assert!(!is_interesting_device("nvme0n1p1", &names), "p-then-digit partition");
        assert!(is_interesting_device("mmcblk0", &names));
        assert!(!is_interesting_device("mmcblk0p1", &names), "mmcblk uses the same convention");
    }

    #[test]
    fn is_interesting_device_filters_every_virtual_prefix() {
        let names: Vec<String> = ["loop0", "ram0", "zram0", "dm-0", "md0"]
            .iter()
            .map(|s| s.to_string())
            .collect();

        for name in &names {
            assert!(!is_interesting_device(name, &names), "{name} should be filtered");
        }
    }

    #[test]
    fn a_truncated_diskstats_line_is_a_parse_error_not_a_panic() {
        // Fewer than the 11 required counter fields.
        let stat = "   8       0 sda 158 0 9120\n";
        let err = parse_diskstats(p(), stat).expect_err("truncated line must fail");
        assert!(matches!(err, Error::Parse { line: Some(1), .. }), "got {err:?}");
    }

    #[test]
    fn a_non_numeric_counter_is_a_parse_error() {
        let stat = "   8       0 sda banana 0 9120 52 0 0 0 0 0 35 52\n";
        let err = parse_diskstats(p(), stat).expect_err("garbage counter must fail");
        assert!(matches!(err, Error::Parse { line: Some(1), .. }), "got {err:?}");
    }

    #[test]
    fn an_unsafe_device_name_is_a_parse_error() {
        let stat = "   8       0 ../../etc 158 0 9120 52 0 0 0 0 0 35 52\n";
        let err = parse_diskstats(p(), stat).expect_err("unsafe name must fail");
        assert!(matches!(err, Error::Parse { .. }), "got {err:?}");
    }

    #[test]
    fn blank_lines_are_skipped() {
        let stat = "   8       0 sda 158 0 9120 52 0 0 0 0 0 35 52 0 0 0 0 0 0\n\n\n";
        let devices = parse_diskstats(p(), stat).expect("blank lines are fine");
        assert_eq!(devices.len(), 1);
    }

    // ---- mounts --------------------------------------------------------------

    fn mp() -> &'static Path {
        Path::new("proc/self/mounts")
    }

    /// A trimmed real capture: pseudo-filesystems that must be filtered, plus
    /// the one real storage mount (an LVM logical volume — note the `--`,
    /// which is LVM's own naming convention, not an escape sequence).
    const REAL_MOUNTS: &str = "\
sysfs /sys sysfs rw,nosuid,nodev,noexec,relatime 0 0
proc /proc proc rw,nosuid,nodev,noexec,relatime 0 0
udev /dev devtmpfs rw,nosuid,relatime,size=29068628k,nr_inodes=7267157,mode=755,inode64 0 0
tmpfs /run tmpfs rw,nosuid,nodev,noexec,relatime,size=6331024k,mode=755,inode64 0 0
/dev/mapper/ubuntu--vg-ubuntu--lv / ext4 rw,relatime 0 0
cgroup2 /sys/fs/cgroup cgroup2 rw,nosuid,nodev,noexec,relatime,nsdelegate 0 0
";

    #[test]
    fn parses_real_mounts_and_filters_pseudo_filesystems() {
        let mounts = parse_mounts(mp(), REAL_MOUNTS).expect("real fixture parses");
        assert_eq!(mounts.len(), 1);
        assert_eq!(mounts[0].source, "/dev/mapper/ubuntu--vg-ubuntu--lv");
        assert_eq!(mounts[0].mount_point, "/");
        assert_eq!(mounts[0].fs_type, "ext4");
        assert_eq!(
            mounts[0].total, None,
            "parse_mounts is a pure string parser and never populates capacity \
             itself — that's read_capacity's job, called separately by collect()"
        );
    }

    #[test]
    fn mount_fields_with_escaped_spaces_are_decoded() {
        let line = "/dev/sdb1 /mnt/My\\040Backup\\040Drive ext4 rw,relatime 0 0\n";
        let mounts = parse_mounts(mp(), line).expect("parses");
        assert_eq!(mounts[0].mount_point, "/mnt/My Backup Drive");
    }

    /// The gap this test exists to close: `unescape_mount_field` only
    /// decodes the four documented octal escapes, it doesn't strip control
    /// characters — so a hostile mount source (a crafted FUSE daemon, an
    /// LVM volume name) needs `parse_mounts` itself to sanitize, the same
    /// boundary guarantee every other kernel-supplied string in this crate
    /// gets. An ESC byte is not one of the four escapes, so it survives
    /// `unescape_mount_field` unchanged and must be caught here instead.
    #[test]
    fn a_hostile_mount_source_is_sanitised() {
        let line = "\u{1b}[2Jevil-fuse /mnt/x fuse.evil rw 0 0\n";
        let mounts = parse_mounts(mp(), line).expect("parses");
        assert!(!mounts[0].source.contains('\u{1b}'), "{:?}", mounts[0].source);
        assert!(mounts[0].source.contains("evil-fuse"), "{:?}", mounts[0].source);
    }

    // ---- capacity (chunk 8) ---------------------------------------------------

    #[test]
    fn capacity_from_raw_multiplies_blocks_by_fragment_size() {
        let (total, available) = capacity_from_raw(4096, 1_000_000, 400_000).expect("no overflow");
        assert_eq!(total.as_u64(), 4096 * 1_000_000);
        assert_eq!(available.as_u64(), 4096 * 400_000);
    }

    #[test]
    fn capacity_from_raw_overflow_is_none_not_a_panic() {
        assert_eq!(capacity_from_raw(u64::MAX, 2, 1), None);
        assert_eq!(capacity_from_raw(4096, u64::MAX, 1), None);
    }

    #[test]
    fn capacity_from_raw_zero_fragment_size_is_zero_not_a_panic() {
        // Seen on some pseudo-filesystems; must not divide-by-zero or
        // overflow, and reporting 0 bytes is the honest answer here.
        assert_eq!(
            capacity_from_raw(0, 1_000, 500).map(|(t, a)| (t.as_u64(), a.as_u64())),
            Some((0, 0))
        );
    }

    /// Real-hardware check, same standard as every other collector in this
    /// crate: `/` always exists and is always a real (non-pseudo) filesystem
    /// on every machine this crate targets.
    #[cfg(feature = "fs-capacity")]
    #[test]
    fn read_capacity_against_root_returns_plausible_numbers() {
        let (total, available) = read_capacity("/")
            .expect("statvfs on / must not error")
            .expect("/ must report capacity");
        assert!(total.as_u64() > 0, "root filesystem reporting 0 total bytes");
        assert!(
            available.as_u64() <= total.as_u64(),
            "available ({}) exceeds total ({})",
            available.as_u64(),
            total.as_u64()
        );
    }

    /// A mount point that no longer exists (vanished between `parse_mounts`
    /// and the capacity read, or a bad `--sysfs-root`-adjacent path) must
    /// degrade this one field to absent, not fail the whole collector.
    #[cfg(feature = "fs-capacity")]
    #[test]
    fn read_capacity_on_a_nonexistent_path_is_fail_soft() {
        let result = read_capacity("/this/path/almost-certainly-does-not-exist/rustmon-test")
            .expect("must not return an Err — capacity failures are fail-soft");
        assert_eq!(result, None);
    }

    #[test]
    fn unescape_handles_all_four_documented_sequences() {
        assert_eq!(unescape_mount_field("a\\040b"), "a b");
        assert_eq!(unescape_mount_field("a\\011b"), "a\tb");
        assert_eq!(unescape_mount_field("a\\012b"), "a\nb");
        assert_eq!(unescape_mount_field("a\\134b"), "a\\b");
        assert_eq!(unescape_mount_field("plain"), "plain");
    }

    /// A backslash that isn't one of the four documented escapes must pass
    /// through unchanged rather than erroring or eating the following chars.
    #[test]
    fn unescape_passes_through_an_unrecognised_backslash() {
        assert_eq!(unescape_mount_field("a\\999b"), "a\\999b");
        assert_eq!(unescape_mount_field("trailing\\"), "trailing\\");
        assert_eq!(unescape_mount_field("a\\0b"), "a\\0b");
    }

    /// The reason this function walks `chars()` and not bytes: a raw byte
    /// index could split a multi-byte UTF-8 character in half.
    #[test]
    fn unescape_does_not_corrupt_multi_byte_utf8() {
        assert_eq!(unescape_mount_field("caf\u{e9}"), "caf\u{e9}");
        assert_eq!(unescape_mount_field("日本語\\040ラベル"), "日本語 ラベル");
    }

    #[test]
    fn a_line_with_too_few_fields_is_a_parse_error() {
        let err = parse_mounts(mp(), "onlyonefield\n").expect_err("must fail");
        assert!(matches!(err, Error::Parse { line: Some(1), .. }), "got {err:?}");
    }

    #[test]
    fn blank_lines_in_mounts_are_skipped() {
        let mounts = parse_mounts(mp(), "\n\n").expect("blank lines are fine");
        assert!(mounts.is_empty());
    }
}
