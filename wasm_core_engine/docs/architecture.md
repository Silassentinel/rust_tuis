# Architecture

Design doc required by [`HANDOVER.md`](../HANDOVER.md) before porting anything
beyond the pure functions. Answers the three open questions there.

## 1. What "reusable beyond this project" means here

**Decision (owner sign-off, 2026-08-05): `core/` is domain-agnostic. The
ferment-tracker domain model is a separate layer, built later, on top of
this crate — not inside it.**

Concretely:

- `core/` holds only generic, non-ferment-specific logic: parsing utilities,
  math/formatting helpers, the storage-trait *pattern*, WASM/native binding
  conventions. Nothing here should require knowing what a "tracker" or
  "checklist" is.
- The two functions scoped for this milestone —
  [`parseFieldLabel`](../HANDOVER.md) and `mix.ts` — fit this cleanly as-is:
  `parseFieldLabel` is a generic label-syntax parser (infers a field kind
  from parenthetical syntax; nothing fermentation-specific about the syntax
  itself), and `mix.rs` is generic weighted-blend math (grams, ratios,
  rounding). Neither needs renaming or reshaping to stay domain-agnostic.
- Once `core/` proves out (this milestone + the storage trait), the
  ferment-tracker domain model (trackers, checklists, schedules, blends)
  becomes a *consumer* of `core/` — either a separate crate in this
  workspace or back in the web app, decided when that work starts. Future
  unrelated projects get the same treatment: their own domain layer, built
  on the same pattern, not shoehorned into this crate's types.
- Practical effect on naming: avoid ferment-tracker vocabulary in `core/`
  public APIs (no `Tracker`, `Checklist`, `Blend` types here). Where the
  source TypeScript uses domain-flavored names for genuinely generic
  concepts, prefer the generic name in the port (e.g. a "mix" stays a
  weighted blend of named components with weights — that's already
  domain-neutral in the source, so no change needed).

This is revisited once the ferment-tracker domain layer is actually being
built — at that point we'll know whether a second workspace member is the
right shape, or a separate crate/repo entirely.

## 2. Binding layer (WASM)

`wasm-bindgen` is the obvious choice for exposing a JS-callable API, but per
`CLAUDE.md` standing rule 1 it needs a `docs/crate-checklist.md` entry and
explicit sign-off before `cargo add` — not assumed just because it's the
obvious default.

**Decision: defer adding `wasm-bindgen` until there's an actual JS-callable
surface to bind.** For this milestone, "compiles for `wasm32-unknown-unknown`"
is proven the plain way: the crate builds as an ordinary `rlib`/`cdylib` for
that target with zero dependencies (`cargo build -p core --target
wasm32-unknown-unknown`, confirmed passing). That proves portability of the
logic itself — no `std::fs`, no platform-assumed clock, nothing that only
exists on native. It does **not** yet prove the crate is callable from
JavaScript, which is a separate, later concern.

When a real consumer needs to call into this crate from JS (i.e. once the
storage trait and a first real WASM target exist), propose `wasm-bindgen` as
an "Open proposal" in `docs/crate-checklist.md` at that point, with the usual
maintenance/adoption/footprint/MSRV writeup, and get sign-off before adding
it.

## 3. Storage abstraction

Not implemented yet — HANDOVER.md explicitly scopes the filesystem-bound
functions and the storage trait itself as later work, after the pure-function
chunks and this doc are solid. This section records the *shape* of the
decision so later work has a target, without building it now.

Sketch (subject to revision when it's actually built):

- A trait — working name `Store` — with byte-oriented, `Result`-returning
  methods (`get`, `put`, `delete`, `list`) over a simple key type (likely
  `&str` path-like keys, matching the source app's file-per-entity layout).
  Keep it synchronous for now: native `std::fs` is naturally sync, and a sync
  trait is the simpler default until a concrete WASM-side implementation
  proves async is actually required.
- **Native impl**: backed by real `std::fs`, straightforward port of the
  current file-per-entity layout.
- **WASM impl**: explicitly *not* assumed to be IndexedDB — HANDOVER.md flags
  this as open. The right answer depends on what's practical to call from
  `wasm32-unknown-unknown` without a heavy dependency, and probably on what
  the consuming web app already has available (it may prefer to own storage
  itself and have `core/` just call an injected implementation). Decide this
  when the storage trait work actually starts, informed by whichever web app
  is the first real consumer.
- Because the trait lives in `core/` and is domain-agnostic (per §1), it
  should be phrased in terms of generic keyed storage, not tracker-specific
  operations (`writeCheckin` etc. become domain-layer logic that calls
  through a generic `Store`, not methods on the trait itself).

## Status

This doc satisfies HANDOVER.md's "First milestone" item 1. Items 2–4
(pure-function ports, WASM build confirmation, `docs/TODO.md`) are tracked
separately.
