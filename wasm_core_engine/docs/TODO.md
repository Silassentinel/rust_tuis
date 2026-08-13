# TODO

Chunks tracked here per `CLAUDE.md` standing rule 2 (one function/module at a
time, tests before moving on). Source for all of these: source for all
ported logic: `website/features/ferment-tracker-app/server/fermentData.ts`
and `src/lib/mix.ts`, and their test files, unless noted otherwise. See
[`architecture.md`](architecture.md) for why these land as domain-agnostic
utilities rather than ferment-tracker-specific types.

## Done — domain-agnostic infrastructure, proven against the ferment-tracker source

- [x] Design doc (`architecture.md`) answering the binding-layer /
      storage-abstraction / domain-scope questions.
- [x] `parseFieldLabel` → [`core/src/field_label.rs`](../core/src/field_label.rs)
- [x] `mix.ts` (`parseMix`/`formatMix`/`orderMix`/`mixTotal`/`roundWeight`) →
      [`core/src/mix.rs`](../core/src/mix.rs)
- [x] `titleCase` → [`core/src/text.rs`](../core/src/text.rs)
- [x] `computeNextDue` / `formatDue` / `formatCadence` →
      [`core/src/datetime.rs`](../core/src/datetime.rs) +
      [`core/src/schedule.rs`](../core/src/schedule.rs). Needed a
      dependency-free civil-calendar type (Howard Hinnant's
      `days_from_civil`/`civil_from_days` algorithm) since no date/time
      crate is approved and none was actually needed — both source
      functions only ever operate on explicit `Date` values the caller
      supplies, never the system clock.
- [x] `safeFilename` → [`core/src/filename.rs`](../core/src/filename.rs)
- [x] `buildChecklistBody` / `parseBodyToValues` →
      [`core/src/field_log.rs`](../core/src/field_log.rs) as
      `build_log_body`/`parse_log_body`, operating on a generic
      `FieldSection` (not the source's `ChecklistSection` — kept
      domain-neutral per `architecture.md` §1). Includes the full
      colon-in-label regression coverage.
- [x] Generic `Store` trait + native `std::fs`-backed `FsStore` →
      [`core/src/storage/`](../core/src/storage/). Path-traversal-safe
      (rejects `..` components and absolute keys), compiles on both targets
      — `FsStore` itself is `#[cfg(not(target_arch = "wasm32"))]`-gated out
      of the WASM build entirely, since `std::fs` doesn't mean the same
      thing there.
- [x] `DateTime::parse_ymd` — a `"YYYY-MM-DD"` date-only string parser,
      added 2026-08-08 at `ferment_tracker_core`'s request for its
      `listTrackers` port (`tracker.created` and check-in dates are stored
      as this shape). Not a port of an existing TS function — the source
      just hands these strings to the platform `Date` constructor — but
      pure parsing with no new dependency, same posture as the rest of this
      file. `None` on malformed input rather than a panic, same as
      `field_label.rs`/`mix.rs`'s parse-and-skip functions.

All pure-function and native-storage chunks scoped in HANDOVER.md's "What to
port" (pure section) are ported, with 1:1-translated test coverage —
`cargo test -p core` is green (72 tests) and `cargo build -p core --target
wasm32-unknown-unknown` compiles clean.

- [x] **Security-parity fix (2026-08-12): `build_log_body` now sanitizes
      embedded newlines in field answers**, via a new `sanitize_answer`
      helper. The source's `buildChecklistBody` gained this in a
      2026-08-12 security pass (`website/features/ferment-tracker-app`'s
      `.security/findings.md` RT-2026-08-07-03) — this port had been
      faithfully translated from the *pre-fix* source, so it had the same
      gap: an embedded newline in a checklist answer terminated its
      `- Label: value` line early, letting the rest of the value inject
      new Markdown lines into the log body (a forged `## Section` heading
      re-parsed as real data by `parse_log_body` on the next edit, or
      arbitrary content). Found while checking `ferment_tracker_core`'s
      latest-source-changes catch-up for anything beyond the "completed
      tracker" feature that prompted it — not something this specific pass
      set out to look for. Regression tests cover the injection payload and
      CRLF-collapses-to-one-space (matching the source's `/\r\n?|\n/g`).
      70 → 72 tests.

## Deliberately not done — two real blockers, not oversights

- [ ] **WASM-side `Store` implementation.** `architecture.md` §3 always
      flagged this as "approach TBD" — not assumed to be IndexedDB, possibly
      an implementation injected by the consuming web app instead. Any of
      those approaches needs a way to call into JS from WASM at all, which
      means `wasm-bindgen` (or an alternative) — genuinely blocked on the
      `docs/crate-checklist.md` sign-off `CLAUDE.md` requires before that
      `cargo add` happens. Propose it there when this is picked back up.
- [ ] **Porting `listTypes`/`listTrackers`/`writeCheckin`/`updateCheckin`/
      `deleteCheckin`/`createTracker`/`writeYaml`'s decision logic into
      `core/`.** Deliberately *not* just a remaining chunk of the same kind
      as the rest of this list: these functions are inherently
      ferment-tracker-specific (tracker.yml/type.yml schema, the
      `Kombucha-Log-{date}.md` filename convention, tracker frontmatter
      shape). Per the domain-agnostic-core decision in `architecture.md`
      §1, this is exactly the kind of logic that belongs in the
      ferment-tracker *domain layer*, built as a separate consumer of
      `core/` — not inside `core/` itself. Revisit when that layer is
      started; it will call through `Store` (once a WASM implementation
      exists) rather than touching `fs` directly, using
      `build_log_body`/`parse_log_body`/`safe_filename` from `core/` for the
      log-entry mechanics.
