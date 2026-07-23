# Kickoff prompt for Claude Code

Paste this as your first message in a Claude Code session started in this repo
(`cd ~/code/Rust/rust_tuis && claude`). `CLAUDE.md` loads automatically and
carries the standing project rules; this prompt just gives the immediate task.

---

Continue building `rustlogger`. Read `CLAUDE.md`, `docs/rustlogger-design.md`,
`docs/TODO-rustlogger.md`, and `docs/crate-checklist.md` first for full context.

Chunks 1–3 (workspace scaffold, `stop_trigger.rs`, `pty_session.rs`) were
written without a local Rust toolchain to check them against, so they've
never actually been compiled or run. Before doing anything else:

1. Run `cargo test -p rustlogger` and fix whatever doesn't compile or fails.
   Don't assume the existing code is correct just because it's already in the
   tree — verify it, chunk by chunk (stop_trigger's unit tests first, then
   pty_session's integration test).
2. Once that's actually green, move on to chunk 4 as scoped in
   `docs/TODO-rustlogger.md`: raw-mode the outer/real terminal, proxy bytes
   both ways between it and `PtySession`, and wire in the stop conditions —
   the `stoplogger` phrase (`stop_trigger.rs`), the wrapped shell's exit code
   (any code), and SIGINT/SIGHUP/SIGTERM handling for Ctrl+C, terminal close,
   and kill.
3. Chunk 4 needs `nix`'s `process`, `signal`, and `poll` features added to
   `rustlogger/Cargo.toml` (currently only `term` is enabled). Per the crate
   checklist rule in `CLAUDE.md`: propose that addition (why each feature is
   needed) and wait for my go-ahead before running `cargo add` / editing the
   feature list — don't add it silently even though `nix` itself is already
   approved.

Keep going one chunk at a time, tests + docs updated before each next step.
