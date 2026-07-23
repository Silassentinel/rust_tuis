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

There's also a headless mode, `rustlogger <command> [args...]`, for when nothing
is sitting at a live terminal to type `stoplogger` or hit Ctrl+C — see chunk 7's
notes below. It's what the rustlogger MCP server (`rustlogger-mcp-server/`) uses
to let Claude track a program in the background and check its log later.

## Non-goals (for now)

- Not a replacement for full asciinema-style timing/playback — plain text log first,
  playback format is a later chunk if wanted.
- Cross-platform support is in progress (chunks 8-9) but not there yet: Android via
  Termux type-checks but hasn't been run on a real device, and native Windows (via
  ConPTY) hasn't been started. Today it only actually runs on desktop Linux.

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
7. **Headless tracking mode** — run a specific command instead of `$SHELL`,
   with no outer terminal involved, for the MCP server to drive.
8. **Cross-platform, part 1: Android via Termux** — verify (ideally on a real
   device) that today's POSIX-based code actually works under Termux, not
   just that it type-checks for the target. See
   `docs/rustlogger-android-termux.md`.
9. **Cross-platform, part 2: native Windows console** — a second
   pty/terminal/signal backend using ConPTY, selected by `cfg(windows)`.
   Its own crate decision, subject to `docs/crate-checklist.md`.

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

## Chunk 7 notes: headless tracking mode is a different shape, not just interactive-mode-minus-a-terminal

Interactive mode (`session::run`) exists to sit between a live human and a
shell: it raw-modes a *real* terminal, watches for the `stoplogger` phrase in
what that human types, and forwards their keystrokes. Headless mode
(`session::run_headless`) has none of that, because there's no human on the
other end — it's meant to be started by something else (the rustlogger MCP
server, `rustlogger-mcp-server/`) that wants to run one specific command in
the background and check its log later. Concretely:

- No outer terminal is touched at all — no `RawGuard`, no raw-moding of
  rustlogger's own stdin. There isn't an "outer" side in this mode; there's
  just the tracked command's own pty.
- No `stoplogger` detection — there's no live keystroke stream to feed a
  `StopTrigger`. "Stop tracking" instead means sending rustlogger itself a
  signal (`SIGTERM`, typically), which it already handles the same way
  interactive mode does: send the tracked command `SIGHUP`, reap it, write
  the log's footer, exit. The MCP server's "stop tracking" tool is just
  `kill <pid>`.
- The command being tracked is still run attached to a pty (via the same
  `PtySession::spawn_command`), not a plain pipe — programs that check
  `isatty()` to decide whether to show progress bars or colored output
  behave the same way they would run directly in a terminal.
- Output is both logged and mirrored to rustlogger's own stdout, so running
  `rustlogger some-command` directly in a terminal (rather than through the
  MCP server) still shows you what's happening live, in addition to logging
  it.

`proxy_loop` (interactive) and `headless_loop` (headless) are two distinct
loops for this reason — trying to make one generic over "is there an outer
terminal or not" would have been a worse abstraction than two loops that
share their genuinely-common parts: `read_master` (the `EIO`-vs-`EOF`
handling described above) and `finish_session` (turning a `StopReason` into
"kill the child if it didn't already exit, reap it, write the footer, work
out rustlogger's own exit code").

**Getting the tracked command's tty path required a small `pty_session.rs`
fix.** The natural-seeming `nix::unistd::ttyname(&session.master)` doesn't
give the slave's path (e.g. `/dev/pts/7`) — called on the *master* fd,
`ttyname()` returns `/dev/ptmx`, the pty control device, because the master
isn't itself "a terminal" in the sense that function cares about. The slave
fd is the only side that reports its own real path, and `spawn_command`
drops the parent's copy of that fd once the child has its own (duped onto
its 0/1/2) — so the name has to be captured right after `openpty()`, before
that happens. `PtySession` now carries it as a `pub tty: String` field.
Interactive mode doesn't use this field (it still reports the *outer* real
terminal's path, which is a different, correct thing to want there); only
headless mode needed it.

## Chunk 8 notes: "type-checks for Android" is not the same claim as "works on Android"

`cargo check --target aarch64-linux-android` and `--target armv7-linux-androideabi`
both pass with zero source changes — every `nix` API rustlogger uses
(`openpty`, `cfmakeraw`/`tcgetattr`/`tcsetattr`, `sigaction`/`kill`/`raise`,
`poll`) exists and type-checks against Android's headers. That's a real,
useful signal (it rules out the API-doesn't-exist-on-this-platform failure
mode entirely), but `cargo check` never links or runs anything, so it says
nothing about runtime behavior — whether `TIOCSCTTY` actually grants a
controlling terminal the way rustlogger's `pty_session.rs` assumes, whether
raw-mode termios flags behave identically under Termux's environment, or
whether Android's more aggressive app-lifecycle process management interferes
with signal delivery the way it wouldn't on a desktop OS. None of that has
been verified, because there's no Android device or emulator available in
this dev environment to verify it on. See `docs/rustlogger-android-termux.md`
for exactly what still needs checking, and why the right build path is
on-device inside Termux (`pkg install rust`) rather than cross-compiling from
desktop Linux with the Android NDK — Termux has its own prefix and expects
binaries built against it specifically, and NDK cross-compilation is a known
source of subtle mismatches for that reason, independent of the OS being
technically the same Android/bionic base underneath.
