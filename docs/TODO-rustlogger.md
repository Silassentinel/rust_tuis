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
- [x] 5. Log file format (`timestamp.rs`, `logfile.rs`): session log named
      `rustlogger-<UTC timestamp>.log` in the directory rustlogger is
      launched from (confirmed with you 2026-07-23 rather than a fixed
      dotfile location or a CLI-arg path). Header records start time,
      shell, and tty; every line of the session's display output gets a
      `[UTC timestamp]` prefix; footer records the stop reason and the
      wrapped shell's exit code. Only the master→outer byte stream is
      logged (not raw outer keystrokes) - see `docs/rustlogger-design.md`
      for why that's actually the correct behavior, not a shortcut.
      Timestamp formatting is hand-rolled against `std::time::SystemTime`
      (no crate - see the doc comment in `timestamp.rs` and the checklist
      rule in `CLAUDE.md`), unit-tested against known reference dates.
      19/19 tests green (`cargo test -p rustlogger`); also smoke-tested
      end-to-end through a real pty via `script(1)` (see
      `docs/rustlogger-design.md`'s chunk 5 notes for the transcript).
- [x] 6. Integration tests + README. Split `main.rs` into a thin binary
      over a new `lib.rs` (Ch. 12 `minigrep` pattern, see
      `docs/rustlogger-design.md`'s chunk 6 notes) so
      `tests/session_end_to_end.rs` can spawn the *compiled `rustlogger`
      binary* under a real pty via a generalized
      `PtySession::spawn_command` and check both its exit code and the
      log file it produces, for both the shell-exits-on-its-own and
      `stoplogger` stop conditions. `README.md` added covering usage, the
      stop conditions, and the log format. 21/21 tests green (19 unit +
      2 integration, `cargo test -p rustlogger`), stable across repeated
      runs.
