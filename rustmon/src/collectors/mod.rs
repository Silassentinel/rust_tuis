//! One module per hardware area, each implementing [`crate::collector::Collector`].
//!
//! House rules for every collector in here:
//!
//! - Reads go through [`crate::sysfs::SysfsReader`] only. No `std::fs`.
//! - No `unwrap`, `expect`, `panic!`, or indexing into parsed input.
//! - A missing or unreadable file leaves the field `None` and returns `Ok(())`.
//! - Parsers are iterator chains over `lines()` (Rust Book Ch. 13), which also
//!   keeps the no-index-slicing rule easy to hold to.
//! - Every parser gets a malformed-input test, not just a happy-path one.

pub mod connections;
pub mod cpu;
pub mod disk;
pub mod gpu;
pub mod memory;
pub mod net;
pub mod thermal;

/// Names of every collector this build knows about, in display order.
///
/// Used to validate `--only`/`--skip` so a typo is an error rather than a
/// silently-ignored flag.
pub const ALL: &[&str] = &["cpu", "memory", "thermal", "disk", "net", "gpu", "connections"];
