//! A minimal, domain-agnostic keyed storage abstraction. Native and
//! (eventually) WASM targets each implement `Store` differently — see
//! `docs/architecture.md` §3.
//!
//! Only the native filesystem-backed implementation exists so far. The
//! WASM-side implementation is deliberately not built yet: it's blocked on
//! a binding-layer decision (exposing anything to JS needs `wasm-bindgen`
//! or similar, which needs a `docs/crate-checklist.md` entry and sign-off
//! first, per `CLAUDE.md` standing rule 1) and on knowing what the first
//! real browser consumer actually needs storage to look like.

use std::error::Error;
use std::fmt;

#[cfg(not(target_arch = "wasm32"))]
mod native_fs;
#[cfg(not(target_arch = "wasm32"))]
pub use native_fs::FsStore;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreError(pub String);

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Error for StoreError {}

/// Generic keyed byte storage. Keys are `/`-separated relative paths (no
/// leading `/`, no `..` components) — deliberately not filesystem-specific
/// vocabulary, so a browser-side implementation (IndexedDB, an adapter
/// injected by the consuming web app, or something else — still an open
/// question, see `docs/architecture.md`) can implement this the same
/// trait.
pub trait Store {
    fn get(&self, key: &str) -> Result<Option<Vec<u8>>, StoreError>;
    fn put(&self, key: &str, value: &[u8]) -> Result<(), StoreError>;
    fn delete(&self, key: &str) -> Result<(), StoreError>;
    /// Keys directly under `prefix` (not recursive), sorted. `""` lists the
    /// root.
    fn list(&self, prefix: &str) -> Result<Vec<String>, StoreError>;
}
