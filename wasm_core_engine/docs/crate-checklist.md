# Crate checklist

Same discipline as `rust_tuis/docs/crate-checklist.md` (see `../CLAUDE.md`
standing rule 1) — nothing gets added to any `Cargo.toml` in this workspace
without a filled-in entry here and explicit sign-off first.

For each proposed crate, fill in:

- **Problem statement** — what's missing that this crate solves
- **Alternatives considered** — including "write it ourselves"
- **Maintenance health** — last release, open issues/PRs, maintainer activity
- **Adoption** — downloads, reverse-dependency count, notable users
- **License**
- **Security / RUSTSEC** — any advisories, current or historical
- **API stability** — 1.0+? breaking-change frequency?
- **Dependency footprint** — what it pulls in transitively
- **Platform support** — does it work on every target this crate compiles for (native + `wasm32-unknown-unknown`, at minimum)
- **MSRV**

## Open proposals

_(none yet)_

## Approved

_(none yet)_
