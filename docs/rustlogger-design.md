# rustlogger — design

## What it does

You run `rustlogger` inside a terminal. From that point on, that terminal behaves
exactly as before (interactive programs, colors, `sudo` prompts, etc. all work) but
everything that happens in it — commands typed and all output — is written to a log
file. Logging stops, and the log file is closed cleanly, when any of these happen:

- the wrapped shell exits, for any exit code (`0` or otherwise)
- you type the literal command `stoplogger`
- the session ends any other way: Ctrl+C, the terminal window/tab is closed, the
  connection drops (SIGHUP), or the process is killed (SIGTERM)

## Non-goals (for now)

- Not a replacement for full asciinema-style timing/playback — plain text log first,
  playback format is a later chunk if wanted.
- Not cross-platform — Linux only, matching where it'll actually run.

## Architecture

```
 real terminal (outer)                    child shell (inner)
 ┌───────────────────┐    raw bytes    ┌───────────────────┐
 │ rustlogger process │ ───────────────▶│  pty slave + your  │
 │ (raw mode, reads    │◀─────────────── │  $SHELL running    │
 │  stdin/stdout)      │   raw bytes    │  as normal          │
 └─────────┬───────────┘                └───────────────────┘
           │  tee
           ▼
      logfile.txt
```

`rustlogger` puts the *outer* terminal into raw mode, opens a new pty, spawns the
user's shell attached to the pty's slave side, and then copies bytes in both
directions between the outer terminal and the pty master — while also writing
everything that flows through to the log file. This is the same fundamental
approach the classic `script` command uses.

## Chunks (build order)

Tracked with checkboxes in `docs/TODO-rustlogger.md`. Each chunk gets tests +
docs updated before moving to the next, per project convention.

1. **Scaffolding** — workspace + crate skeleton, docs. (this chunk, no crates)
2. **Stop-phrase detector** — pure-`std`, unit-tested module that watches raw
   input bytes for a `stoplogger` line, independent of any terminal/pty code.
3. **Pty-backed shell wrapper** — spawn `$SHELL` in a real pty, raw-mode the
   outer terminal, proxy bytes both ways. *Blocked on crate decision, see
   `docs/crate-checklist.md`.*
4. **Stop conditions wiring** — child exit code (any code), the stop-phrase
   detector from chunk 2, and signal handling (SIGINT/SIGHUP/SIGTERM) for
   Ctrl+C / terminal close / kill. Also same crate decision as chunk 3.
5. **Log file format** — session header (start time, shell, tty), footer
   (stop reason + exit code), timestamps per line.
6. **Integration tests + README** — end-to-end test spawning a short-lived
   shell command, docs finalized.

## Rust Book references

- Ch. 12 (`minigrep`) — CLI project structure / argument handling, for chunk 1.
- Ch. 9 (error handling) — `Result`/`?` for all I/O in chunks 3–5; no `.unwrap()`
  outside of tests.
- Ch. 16 (concurrency) — the two-directional byte-copy proxy in chunk 3 needs a
  reader/writer thread pair (or an async runtime, decision deferred until chunk 3).
- Ch. 20 (final project, a bit) — the request/response-style loop is structurally
  similar to the proxy loop here.

## Known limitations to revisit later

- The stop-phrase detector works on a simplified line model (see its doc comments
  in `rustlogger/src/stop_trigger.rs`) — arrow-key history recall or complex line
  editing while typing `stoplogger` is not specially handled yet.
- Window resize (SIGWINCH) propagation to the inner pty isn't in scope until a
  later chunk — the wrapped shell just won't resize until then.

## Chunk 4 notes: ending a session doesn't mean leaving the shell running

All three ways a session can end other than the wrapped shell exiting on its own
(the `stoplogger` phrase, the outer terminal's input closing, rustlogger catching
SIGINT/HUP/TERM) send the wrapped shell `SIGHUP` before reaping it
(`session::terminate_child`). Simply stopping the proxy loop and exiting
rustlogger without this would leave the shell alive, attached to a pty nobody is
copying bytes to/from any more — effectively orphaned rather than actually
stopped. `SIGHUP` mirrors what the shell would receive if a real terminal had
hung up, which is the closest real-world equivalent to what rustlogger detaching
represents.

## Chunk 4 notes: pty EOF shows up as `EIO`, not a clean 0-byte read

Once every fd referring to the pty's slave side is closed (normally because the
wrapped shell exited), Linux fails the *next* `read()` on the master side with
`EIO` rather than returning `Ok(0)` the way a pipe would at EOF. This is a
long-standing BSD-pty behavior Linux kept for compatibility, not a bug — do not
gate "the child exited" detection on `Ok(0)` alone; check for `EIO` too. See the
comment at the `master.read()` call in `rustlogger/src/session.rs`.

## Chunk 5 notes: only the display stream gets logged, not raw keystrokes

`logfile.rs` only ever sees the bytes flowing from the pty master back out to
the real terminal (the same bytes `proxy_loop` writes to `outer_out`), never the
raw bytes read from the outer terminal. This is deliberate, not a shortcut:

- The inner pty's own terminal driver already echoes typed input back through
  the master (see `pty_session.rs`'s doc comment and its test), so the master
  stream is already a complete "what appeared on screen" transcript — the same
  thing `script`(1) records.
- It also means a prompt that turns terminal echo off for the duration of a
  sensitive input, most commonly `sudo` asking for a password, is *never*
  written to the log, because nothing was ever echoed to capture. Logging the
  display stream rather than raw keystrokes gets this for free, without
  special-casing "don't log passwords" — there's no reasonable way to
  special-case that safely if raw input were logged instead.

Log file name: `rustlogger-<UTC compact timestamp>.log`, written to whatever
directory rustlogger is launched from (confirmed with the user 2026-07-23,
over a fixed `~/.rustlogger/` location or a CLI-supplied path — either can be
revisited later without changing the log format itself).

Timestamp formatting (`timestamp.rs`) is hand-rolled against
`std::time::SystemTime` — the date math is Howard Hinnant's well-known
`civil_from_days` algorithm — rather than pulling in a time-formatting crate,
per the crate-checklist rule in `CLAUDE.md`; a log timestamp doesn't need
anything a chrono/time crate would offer beyond what a few lines of integer
math already provides.

## Chunk 6 notes: split into `lib.rs` + a thin `main.rs`

All the modules moved from `main.rs` into a new `lib.rs`, leaving `main.rs` as
a few lines calling `rustlogger::session::run`. This is the same
extract-binary-logic-into-a-library-crate pattern Ch. 12 (`minigrep`) teaches,
already cited as the reason for this project's structure back in chunk 1 — it
just took until chunk 6 to actually need it: the end-to-end integration test
(`tests/session_end_to_end.rs`) spawns the *compiled `rustlogger` binary*
itself attached to a real pty, and does so by reusing
`pty_session::PtySession::spawn_command` (a small generalization of the
existing `spawn`, taking a pre-configured `Command` instead of just a shell
path) rather than duplicating the `setsid`/`TIOCSCTTY`/`dup2` dance a second
time in the test. That reuse is only possible because `pty_session` (and
everything else) is now part of a library crate integration tests can `use`.
