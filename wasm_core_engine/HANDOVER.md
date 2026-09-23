# Handover — shared Rust/WASM core engine

_Written 2026-08-05, by a Claude Code session working in a sibling project,
`ferment-tracker-app/`. That project's own docs (`data/PLAN.md`,
`data/SCHEMA.md`) called this "Phase 4."
Read this whole file before writing code — it has the decision this crate
still needs to make before porting starts._

## Why this exists

`ferment-tracker-app` is a local-only fermentation tracker: Vite + React +
TypeScript frontend, a Vite dev-server middleware standing in for a backend
(`server/fermentData.ts` — plain filesystem reads/writes, YAML + Markdown, no
database). Its original roadmap had four phases:

1. Cowork/scheduled-task + Markdown (done)
2. React web app (done — this is `ferment-tracker-app` today)
3. Rust desktop app, same feature set as Phase 2, native shell
4. Converge Phase 2 + Phase 3 on one shared Rust core (compiled to WASM for
   the web frontend, compiled natively for the desktop shell), instead of
   maintaining two separate implementations

**Phase 3 will not happen inside `ferment-tracker-app`.** The decision
(2026-08-05): skip straight to a standalone Rust/WASM core, built here as its
own project, not nested in `website/`. Two things follow from that:

- This crate is **not** "Phase 4 converging with Phase 3" anymore — there's
  no Phase 3 to converge with. It's a from-scratch extraction of Phase 2's
  logic into a reusable core, done with an eye toward reuse across *other*
  future web-app projects too, not just this one. Don't design it as
  ferment-tracker-specific if you can reasonably avoid it — the whole point
  of doing this now is future reuse, per the person who made this call.
- If a native desktop shell for the ferment tracker specifically ever gets
  built later, it would consume this crate the way a Tauri app consumes a
  shared Rust core — but that's speculative and out of scope for the first
  milestone below. Don't build a desktop shell as part of this handover.

## What to port (the concrete reference)

The logic to extract lives in one file, in `ferment-tracker-app`:
`server/fermentData.ts`. Its exported functions, as of this handover:

**Pure — no filesystem, straightforward first targets:**
- `parseFieldLabel(raw: string): ChecklistField` — infers an input kind
  (text / number / yes-no / select / **mix**) from a checklist label's
  trailing parenthetical. Has real edge-case behavior worth porting exactly:
  `"(e.g. ...)"` must not be misread as a unit (a real bug this app shipped
  and fixed), and `"(mix: ...)"` must be checked before the plain-select
  rule since both match on `" / "`.
- `parseBodyToValues(body: string, knownLabels: string[]): Record<string, string>`
  — parses a Markdown log entry back into field values. Has a real edge
  case: labels can contain a colon (`"Ratio used (e.g. 1:1:1)"`, any
  `"(mix: ...)"` field), so it matches against known labels rather than
  splitting on the first colon — this was a real bug (silently broke
  edit-prefill) found and fixed 2026-08-01, see that repo's git log.
- `buildChecklistBody(...)`, `formatCadence(...)`, `formatDue(...)`,
  `computeNextDue(...)`, `titleCase(...)`, `safeFilename(...)` — all pure,
  all have existing test coverage in `server/fermentData.test.ts` to port
  alongside them.
- `src/lib/mix.ts` (a separate file) — `parseMix`/`formatMix`/`orderMix`/
  `mixTotal`/`roundWeight` for the weighted-blend field kind (grams, 2
  decimals, `;`-separated so a comma decimal stays unambiguous). Fully pure,
  fully tested in `src/lib/mix.test.ts` — a good second target after
  `parseFieldLabel`.

**Filesystem-bound — port the logic, not the I/O:**
`listTypes`, `listTrackers`, `writeCheckin`, `updateCheckin`, `deleteCheckin`,
`createTracker`, `writeYaml`. These mix pure decision logic with
`node:fs` calls. Don't port the `fs` calls verbatim — Rust-side, filesystem
access differs completely between the native target (real `std::fs`) and
`wasm32-unknown-unknown` (no filesystem at all; a browser target would need
an injected storage abstraction — IndexedDB, an in-memory store, whatever the
consuming web app provides). Separate the decision logic (what to write, what
to refuse, what the resulting shape is) from the I/O itself, with the I/O
behind a trait the two targets implement differently. This is the actual
architecture work of this crate — the pure-function porting above is the
warm-up.

## The architecture question to resolve first

Before porting anything beyond the pure functions, decide and document (in
`docs/`, as a real design doc, not just a comment):

1. **Binding layer**: `wasm-bindgen` is the obvious default for the WASM
   side, but it needs a `docs/crate-checklist.md` entry and sign-off before
   `cargo add`, same as every dependency here (see `CLAUDE.md`).
2. **Storage abstraction**: a trait (`TrackerStore` or similar) that native
   and WASM each implement — native backed by real files, WASM backed by
   whatever the browser side actually needs (this is genuinely open; don't
   assume IndexedDB without checking what's practical to call from WASM).
3. **What "reusable beyond this project" actually means concretely**: is the
   ferment-tracker domain model (trackers/schedules/checklists/blends) itself
   meant to generalize, or is it specifically the *pattern* (pure-logic core
   + storage trait + WASM/native dual target) that's meant to be reused, with
   each new project getting its own domain model built the same way? This
   changes whether `core/` should have ferment-specific types at all. Ask the
   project owner if it's not obvious from context by the time you get here —
   this is a real fork in the design, not a detail.

## Known gaps in the source app (context, not this crate's job to fix)

Two things are open in `ferment-tracker-app` itself and were deliberately
**not** turned into tracked backlog items there — noted here instead, purely
as background:

- `checklist.overrides` is declared in that project's `SCHEMA.md` and its
  TypeScript types, but never implemented in `resolveChecklist` — silently
  ignored. If the domain model gets ported here, decide whether `overrides`
  is worth implementing for real or worth dropping from the schema instead.
- History search (browsing/filtering past check-ins) was never built in the
  web app. Not this crate's concern unless the ported core ends up owning
  query/filter logic that the current TypeScript doesn't have yet.

Also: `ferment-tracker-app`'s own live scheduled-task prompts (a separate,
non-code part of that system — a claude.ai Scheduled Tasks UI) were found
stale during that project's work and the decision was explicitly to leave
them as-is. Irrelevant to this crate; noted only so it isn't rediscovered and
treated as new information.

## First milestone (what "done" looks like)

1. `docs/` design doc answering the architecture question above.
2. `parseFieldLabel` and the `mix.ts` module ported with their full existing
   test suites passing (`cargo test -p core`), including the two
   already-fixed edge cases called out above — a regression there means the
   port is wrong, not that the test is outdated.
3. Confirm the crate actually builds for `wasm32-unknown-unknown`
   (`cargo build -p core --target wasm32-unknown-unknown`) as well as native,
   before calling anything done — a crate that only compiles natively hasn't
   proven the point of this project.
4. Update `docs/TODO.md` (create it) with the remaining functions from the
   list above as the next chunks, per `CLAUDE.md` standing rule 2.

Do not attempt the storage-trait / filesystem-bound functions before 1-4 are
solid — they're where the real design risk is, and the pure-function chunks
are what prove the toolchain and testing approach work at all first.
