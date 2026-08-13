# wasm_core_engine

Shared Rust business-logic core, intended to compile to both WASM (for web
frontends) and a native library (for a Rust desktop shell), so future
projects don't need to maintain the same logic twice across a web app and a
native app.

Scaffolded 2026-08-05 as a handover from `ferment-tracker-app`'s Phase 4 —
see [`HANDOVER.md`](HANDOVER.md) for the full context, what to port first,
and the architecture decision to make before real work starts. Read that
before `core/`.

Moved into the `rust_tuis` workspace 2026-08-07 (subtree-merged, full history
preserved) — `core/` is now a direct member of `rust_tuis`'s root
`Cargo.toml`, not its own standalone workspace; the `Cargo.toml` that used to
live at this level is gone, superseded by `../Cargo.toml`. Consumers outside
this workspace (e.g. `ferment_tracker_core`) depend on it via a relative path
into here.

## Layout

```
wasm_core_engine/            (inside the rust_tuis workspace)
  core/                 the crate itself (cdylib + rlib)
    src/
      lib.rs
      field_label.rs    parse_field_label — input-kind inference from a label
      mix.rs            weighted-blend math (parse/format/order/total)
      text.rs           title_case
      datetime.rs       dependency-free civil-calendar DateTime
      schedule.rs        format_cadence / compute_next_due / format_due
      filename.rs        safe_filename — path-traversal + dated-.md guard
      field_log.rs        build_log_body / parse_log_body (Markdown log format)
      storage/            Store trait + native std::fs-backed FsStore
  docs/
    architecture.md     design doc: binding layer, storage abstraction, domain scope
    crate-checklist.md  same dependency-approval process as the rest of rust_tuis
    TODO.md              remaining chunks, one at a time
  HANDOVER.md            read first
  CLAUDE.md              standing rules for working in this repo
```

## Status

All domain-agnostic infrastructure scoped in `HANDOVER.md`'s "What to port"
(pure functions) is ported and proven against the ferment-tracker source:

- `docs/architecture.md` answers the architecture questions — `core/` is
  domain-agnostic by design; the ferment-tracker domain model is a separate
  layer, to be built later, on top of this crate.
- Every pure function from the source (`parseFieldLabel`, the `mix.ts`
  module, `titleCase`, `computeNextDue`, `formatDue`, `formatCadence`,
  `safeFilename`, `buildChecklistBody`, `parseBodyToValues`) is ported with
  its full existing test suite translated 1:1, including every named edge
  case.
- A generic, domain-agnostic `Store` trait exists, with a native
  `std::fs`-backed implementation (`FsStore`), path-traversal-safe and
  target-gated out of the WASM build.
- Builds and tests clean on both `cargo test -p core` (72 tests) and
  `cargo build -p core --target wasm32-unknown-unknown`, with zero
  dependencies.

Two things are deliberately not done yet, tracked with the reasons in
`docs/TODO.md`: a WASM-side `Store` implementation (blocked on a
`wasm-bindgen`-or-equivalent decision, which needs `docs/crate-checklist.md`
sign-off first), and porting the ferment-tracker-specific decision logic
(`listTypes`, `writeCheckin`, etc.) — that belongs in a separate
ferment-tracker domain layer on top of `core/`, not inside it.
