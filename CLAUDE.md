# rust_tuis — project instructions

Collection of terminal UI / terminal-adjacent apps built in Rust. See `INDEX.md`
for repo layout.

## Standing rules (apply to every sub-project in this repo)

1. **Reference the Rust Book** (<https://doc.rust-lang.org/book/>) when working
   through a non-trivial problem — cite the relevant chapter in comments/docs
   where it shaped a design choice.
2. **Work in chunks.** Break any non-trivial task into small chunks and track
   them as a checklist (see each sub-project's `docs/TODO-*.md`). Do one chunk
   at a time — don't start the next until the current one is done.
3. **Tests + docs before moving on.** A chunk isn't done until it has passing
   tests and its docs (design doc / TODO checklist) are updated to match. Run
   `cargo test` yourself and confirm it's green before checking a box or
   starting the next chunk — don't just assert it should work.
4. **Crates.io additions are blocked by default.** Before running `cargo add`
   for anything, fill in the checklist in `docs/crate-checklist.md` (problem
   statement, alternatives considered, maintenance health, adoption, license,
   security/RUSTSEC, API stability, dependency footprint, platform support,
   MSRV) as a new "Open proposal" entry, and get explicit sign-off from the
   user before adding the dependency. This is a hard blocker, not a
   suggestion — do not add a crate speculatively "to see if it helps."

## Sub-projects

### `rustlogger/`

Wraps the shell in whatever terminal it's launched from and logs the entire
session (input + output) to a file until: the wrapped shell exits (any exit
code), the user types the literal command `stoplogger`, or the session ends
any other way (Ctrl+C, terminal closed → SIGHUP, killed → SIGTERM).

- Architecture, non-goals, and the Rust Book references behind each part:
  `docs/rustlogger-design.md`
- Build plan / current status: `docs/TODO-rustlogger.md`
- Crate decisions and their justification: `docs/crate-checklist.md`

**Handoff note:** chunks 1–3 (workspace scaffold, `stop_trigger.rs`,
`pty_session.rs`) were written in a sandbox with no Rust toolchain available,
so they were never compiled or test-run before landing here. Treat that code
as unverified until you've run `cargo test -p rustlogger` yourself and it's
actually green — fix whatever doesn't compile first, don't assume it's
correct because it's already in the tree.
