# wasm_core_engine — project instructions

**Read [`HANDOVER.md`](HANDOVER.md) before writing any code.** It has the
context this file doesn't repeat: why this project exists, what to port
first, the architecture decision to make before real work starts, and what
"done" looks like for the first milestone.

## Standing rules

1. **Crates.io additions are blocked by default.** Before running `cargo add`
   for anything, fill in the checklist in `docs/crate-checklist.md` (problem
   statement, alternatives considered, maintenance health, adoption, license,
   security/RUSTSEC, API stability, dependency footprint, platform support —
   including `wasm32-unknown-unknown`, MSRV) as a new "Open proposal" entry,
   and get explicit sign-off from the user before adding the dependency. Same
   rule as `../CLAUDE.md` (this crate lives inside the `rust_tuis` workspace
   as of 2026-08-07) — this is a hard blocker, not a suggestion.
2. **Work in chunks, tracked as a checklist.** Break the porting work into
   small pieces (one function or one module at a time) and track them in
   `docs/TODO.md`. Do one chunk at a time.
3. **Tests before moving on.** A chunk isn't done until it has passing tests.
   Run `cargo test -p core` yourself and confirm it's green — don't assert it
   should work without running it.
4. **Every ported function needs a test that would fail against the original
   TypeScript's behavior if the port got it wrong.** The point of this crate
   is to be a faithful, reusable core — a port that silently changes behavior
   defeats the purpose. Where the source has a test file
   (`server/fermentData.test.ts` etc.), port the test cases too, not just the
   implementation.
5. **This crate targets two runtimes: native and `wasm32-unknown-unknown`.**
   Before adding any dependency or writing anything platform-specific, check
   it actually compiles for both. `std::fs`, `std::time::SystemTime`, and
   anything else assuming a real filesystem/clock do not exist the same way
   in a browser — see HANDOVER.md's "What has to change" section.
