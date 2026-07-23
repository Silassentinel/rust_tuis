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
- [x] 7. Headless tracking mode: `rustlogger <command> [args...]` runs
      `command` (not `$SHELL`) attached to a pty with no outer terminal
      touched at all - no raw-moding, no `stoplogger` detection (there's
      no live keystroke stream to watch), just the command's pty output
      logged and mirrored to rustlogger's own stdout until it exits or
      rustlogger is signaled to stop (see `session::run_headless` and
      `docs/rustlogger-design.md`'s chunk 7 notes for why, and for the
      `PtySession::tty` field this needed - `ttyname()` on a pty *master*
      fd reports `/dev/ptmx`, not the slave's real path, so the slave's
      name has to be captured before it's dropped). Added to support the
      rustlogger MCP server (`rustlogger-mcp-server/`, own README) that
      lets Claude start/check/stop a tracked program in the background.
      Refactored `read_master`'s `EIO`-vs-`EOF` handling and the
      stop-reason-to-exit-code logic (`finish_session`) out of
      interactive mode so headless mode reuses both rather than
      duplicating them. 23/23 tests green (20 unit + 3 integration,
      `cargo test -p rustlogger`), stable across repeated runs.
- [ ] 8. Cross-platform, part 1 - Android via Termux (see
      `docs/rustlogger-android-termux.md`). `cargo check` confirmed clean
      for both `aarch64-linux-android` and `armv7-linux-androideabi` with
      no source changes - every `nix` API in use (pty/termios/signal/poll)
      type-checks for Android. **Not yet done: an actual on-device
      build+test run.** No Android device/emulator is available in this
      dev environment, so `cargo test -p rustlogger` has not actually been
      run and gone green on Android - don't check this box on the strength
      of the type-check alone. Whoever has device access next should build
      on-device via Termux's own toolchain (`pkg install rust`, not NDK
      cross-compilation - see the doc for why) and confirm the real test
      suite passes there, plus the specific runtime risks called out in
      that doc (signal delivery under Android's process lifecycle
      management, in particular).
- [ ] 9. Cross-platform, part 2 - native Windows console. Scoped as its own
      sub-project once chunk 8 is settled: rustlogger's pty/terminal/signal
      layer (`pty_session.rs`, `terminal.rs`, `signals.rs`) is built
      entirely on POSIX APIs via `nix` (`openpty`, `termios`, `sigaction`)
      that have no Windows equivalent. Native Windows needs a parallel
      backend using ConPTY (`CreatePseudoConsole`) instead of `openpty`,
      the console-mode APIs instead of `termios`, and console-control-event
      handlers instead of POSIX signals - not a port, a second
      implementation selected by `cfg(windows)` vs `cfg(unix)`. Needs its
      own design doc section (architecture per-platform, which pieces are
      genuinely shared vs. which need two implementations) and almost
      certainly a new crate for the Windows side (`windows` and/or a
      ConPTY-wrapping crate) - subject to the crate-checklist process in
      `docs/crate-checklist.md` before anything gets added to `Cargo.toml`.
      Not started.
