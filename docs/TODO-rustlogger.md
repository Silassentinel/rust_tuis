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
- [x] 4. Stop conditions wiring: `terminal.rs` (`RawGuard`, raw-modes the
      outer terminal via termios, restores on drop), `signals.rs`
      (installs SIGINT/SIGHUP/SIGTERM handlers, exposes which one fired
      through a process-global flag), `session.rs` (`proxy_loop` copies
      bytes both ways via `poll()`, feeds outer input through the chunk-2
      `StopTrigger`, and `run()` wires it all up: on `stoplogger`, the
      outer input closing, or a caught signal, sends `SIGHUP` to the
      wrapped shell before reaping it; on the shell exiting on its own,
      reaps it and uses its real exit code). `process`/`signal`/`poll`
      features added to `Cargo.toml` per the follow-up crate-checklist
      entry (approved 2026-07-23). 14/14 tests green
      (`cargo test -p rustlogger`); see `docs/rustlogger-design.md` for
      the two behaviors that needed documenting along the way (the
      Linux pty EIO-vs-EOF quirk, and why ending the session sends the
      child SIGHUP instead of just detaching).
- [ ] 5. Log file format: session header/footer, per-line timestamps.
- [ ] 6. Integration tests + README for the `rustlogger` crate.
