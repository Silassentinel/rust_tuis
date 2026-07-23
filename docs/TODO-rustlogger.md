# rustlogger — build TODO

One chunk at a time. Don't start the next box until the current one has tests
passing and docs updated (see `docs/rustlogger-design.md`).

- [x] 1. Scaffolding: workspace `Cargo.toml`, `rustlogger` crate skeleton, design
      doc, this TODO file. No external crates.
- [x] 2. Stop-phrase detector (`stop_trigger.rs`): pure-`std`, unit tested,
      detects a `stoplogger` line in raw input bytes.
- [x] 3. Pty-backed shell wrapper (`pty_session.rs`): `nix` approved (see
      `docs/crate-checklist.md`), spawns `$SHELL` attached to a real pty via
      `openpty` + `Command::pre_exec`, exposes the master side as a
      `std::fs::File`, integration-tested by spawning `/bin/sh` and checking
      output + exit code. Does not yet touch the outer/real terminal.
- [ ] 4. Stop conditions wiring: raw-mode the outer terminal, proxy bytes
      both ways between it and `PtySession`, child exit code (any), the
      stop-phrase trigger from chunk 2, and signal handling
      (SIGINT/SIGHUP/SIGTERM) for Ctrl+C / terminal close / kill. Needs the
      `process`, `signal`, and `poll` features of `nix` added to `Cargo.toml`.
- [ ] 5. Log file format: session header/footer, per-line timestamps.
- [ ] 6. Integration tests + README for the `rustlogger` crate.
