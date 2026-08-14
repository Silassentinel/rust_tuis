//! Real network I/O — DNS resolution today, traceroute once chunk 17 lands.
//!
//! Deliberately **not** under [`crate::collectors`], and not accessed
//! through [`crate::sysfs::SysfsReader`] beyond the one config file it
//! reads. `collectors::*` is a hard "reads through the confined filesystem
//! boundary only" contract; everything in this module opens real sockets
//! and talks to other machines, a different resource and a different trust
//! tier, and keeping that boundary structurally visible (a different
//! top-level module, not just a different file under `collectors/`) is the
//! point.
//!
//! Both this module's entry points are best-effort and infallible from the
//! caller's point of view — they return `Option`/an empty result on any
//! failure (no nameserver configured, a timeout, a malformed response)
//! rather than propagating [`crate::error::Error`]. Nothing in this module
//! is required for rustmon's core purpose; it exists purely as opt-in,
//! user-triggered enrichment (see `ui::app`'s checkbox/cursor UX), so a
//! failure here must never be louder than "this one field stays absent."

pub mod dns;
