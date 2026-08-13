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
- Cross-platform support is implemented but not yet verified on-device: both
  Android via Termux and native Windows (via ConPTY) type-check cleanly for
  their targets and pass Unix-side regression tests unchanged, but neither has
  actually been built and run on a real device/machine yet — see chunks 8 and
  9's notes below. Today it's only actually been run and tested on desktop
  Linux.

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
   Its own crate decision, subject to `docs/crate-checklist.md`. Done; see
   the chunk 9 notes below for exactly what "done" means here.

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
- **The stop phrase fires on data, not just commands.** The detector sees every
  byte from the outer terminal with no notion of which program is consuming it,
  so typing or pasting `stoplogger` on its own line into an editor, pager,
  heredoc or interactive database client ends the session and SIGHUPs the shell.
  Accepted as a documented limitation rather than fixed: every alternative either
  changes the headline interface (“just type `stoplogger`”) or depends on a
  heuristic about the inner program's tty state. See the chunk 6 decision in
  `.security/mitigation-plan.md`.
- **The log is stored byte-exact, which makes it untrusted content to display.**
  Deliberate — the transcript's job is to be an honest record, so nothing is
  stripped or rewritten at write time. Consequence: a logged program controls
  bytes in the file, including escape sequences that execute if the log is
  rendered (`cat`, `less -R`), `\r` tricks that make the display differ from the
  bytes, and lines forged to look like rustlogger's own footer. Sanitization
  belongs at each consumer boundary instead — the docs recommend `cat -v`, and
  the MCP server strips escapes before handing log text to a model. See the
  chunk 4 decision in `.security/mitigation-plan.md`.
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

## Chunk 9 notes: what "done" means for the Windows backend

Each of `pty_session.rs`, `terminal.rs`, `signals.rs`, and `session.rs` is now
a directory (`mod.rs` + `unix.rs` + `windows.rs`), matching the pattern
`docs/TODO-rustlogger.md`'s chunk 9 entry describes. What's genuinely shared
between platforms turned out to be small but load-bearing: `StopSignal`
(what stopped the wrapped process, described platform-agnostically) and
`StopReason`/`finish_session` (what stopped the *session*, and the
terminate/reap/log-footer/exit-code logic that reacts to it) live in each
module's `mod.rs`, built independently by each platform's code but matched
on by neither — `session.rs`'s orchestration code never sees a raw
`nix::sys::signal::Signal` or a raw `CTRL_C_EVENT`.

**The Windows implementation, piece by piece:**

- `pty_session/windows.rs` spawns the wrapped shell/command attached to a
  ConPTY via `portable-pty`, which wraps `CreatePseudoConsole`'s fiddly
  attribute-list/handle-lifetime setup. Unlike Unix's single bidirectional
  `master: File`, portable-pty hands out separate reader/writer trait
  objects, so `PtySession` here carries those instead, plus a `master:
  Box<dyn MasterPty + Send>` field that's never read but has to be kept
  alive for its `Drop` timing — an early version of this code didn't store
  it at all, which (per ConPTY's own documented lifetime rules) would have
  closed the pseudoconsole the instant `spawn_command` returned, before a
  single byte had been read. Caught by reading `portable-pty`'s own source
  rather than by a test, since nothing here can actually run to catch it at
  runtime in this environment.
- `terminal/windows.rs` assembles the raw-mode-equivalent console state from
  individual `ENABLE_*` mode bits (clearing `ENABLE_ECHO_INPUT`/
  `ENABLE_LINE_INPUT`/`ENABLE_PROCESSED_INPUT`, setting
  `ENABLE_VIRTUAL_TERMINAL_INPUT`) via `GetConsoleMode`/`SetConsoleMode`,
  the same role Unix's `cfmakeraw` plays in one call.
- `signals/windows.rs` installs a `SetConsoleCtrlHandler` callback that
  records which `CTRL_*_EVENT` arrived in a global (the same
  record-and-return-immediately shape as the Unix signal handler), then
  translates it into a `StopSignal` with an exit code chosen directly
  (there's no Windows equivalent of POSIX's `128+n` convention): `130` for
  `CTRL_C_EVENT`/`CTRL_BREAK_EVENT`, `1` for the close/logoff/shutdown
  events.
- `session/windows.rs` is where the real design difference lives: `poll()`
  is POSIX-only, and there's no direct Windows equivalent for arbitrary
  `Read`/`Write` trait objects (as opposed to raw `HANDLE`s, which
  `WaitForMultipleObjects` needs and portable-pty's abstraction doesn't
  expose). The chosen strategy is one dedicated OS thread per direction
  (reading the ConPTY, reading the outer input), each forwarding raw byte
  chunks back to the main thread over an `mpsc` channel; the main thread
  does all the actual decision-making (feeding the `StopTrigger`, writing
  the log) so neither of those needs to be `Send`, and polls
  `signals::received()` via `recv_timeout` in between messages as the
  closest available substitute for `poll()`'s multi-source wait.

**Known limitation, left as such rather than papered over:** the worker
threads use plain blocking reads/writes with no portable way to cancel one
from another thread. Windows' own mechanism for that (`CancelSynchronousIo`)
needs a Win32 API surface (`Win32_System_IO`/`Win32_System_Threading`) beyond
the two namespaces `docs/crate-checklist.md`'s sign-off scoped the `windows`
crate dependency to — expanding that would mean going back through the same
governance process, not a silent expansion mid-implementation. The practical
consequence: once a stop reason is decided, whichever worker thread didn't
report it is simply abandoned (not joined) rather than cancelled, and dies
along with the rest of the process at the end of `run`/`run_headless`. This
is also why `proxy_loop`/`headless_loop` use plain detached `thread::spawn`
rather than `std::thread::scope` — scope would force a join before
returning, which could hang on exactly that abandoned thread.

**Verified:** `cargo check --target x86_64-pc-windows-gnu -p rustlogger
--all-targets` (lib, bin, and the integration test suite) and `cargo clippy`
for the same target are both clean with zero warnings. Getting the
integration test suite itself to type-check for Windows surfaced one real
gap along the way: `tests/session_end_to_end.rs` originally read
`session.master` directly (a `pub master: File` field that only exists on
the Unix `PtySession`), so the test suite wasn't actually portable even
though the library was — fixed by adding a small `OuterPty` wrapper local to
the test file, `cfg`-gated the same way the library itself is, so the test
bodies stay identical across platforms. The full Unix test suite (26 unit +
3 integration tests) stays green, run three times in a row with no
flakiness, confirming the whole cross-platform refactor didn't regress the
one platform this can actually be tested on.

**Not verified, and not claimed as such — this needs real Windows hardware
to actually confirm:**

- Whether ConPTY EOF genuinely surfaces as a clean `Ok(0)` on the cloned
  reader handle the way `pty_session/windows.rs` assumes, the same question
  chunk 4 had to resolve for Unix's `EIO`-at-EOF quirk (see that chunk's
  notes above) — Windows may have its own equivalent wrinkle that simply
  hasn't been discovered yet.
- Whether `CTRL_CLOSE_EVENT`/`CTRL_LOGOFF_EVENT`/`CTRL_SHUTDOWN_EVENT`'s
  short OS-enforced grace period (historically ~5 seconds, per Microsoft's
  own docs) actually gives `finish_session` enough time to terminate the
  child, reap it, and flush the log's footer before the process is
  force-killed regardless.
- Whether the abandoned-worker-thread limitation above has any observable
  effect in practice (expected answer: no, since the process exits and
  tears everything down with it — but "expected" isn't "confirmed" here).

Same honesty bar as chunk 8: "type-checks and passes the Unix regression
suite" is the accurate claim. "Works on Windows" is not yet earned.
