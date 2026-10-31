# Spec: rustlogger forwards input, window size and resizes (chunk 10)

Requested 2026-10-24 by the virtualisation-environment project
(`~/code/virtualisation environment`, its Chunk 123, direction 1, chosen
by the user that day). Written by the session that scoped it there; the
change itself is for a session in this repo (the user: "you don't do the
change"). Nothing below is implemented or run yet.

## Problem (measured 2026-10-15)

The virtualisation repo's `scripts/track.sh tui` runs
`rustlogger embodied tui` in headless mode (`session::run_headless` →
`headless_loop`, `rustlogger/src/session/unix.rs`). Three results, from
that repo's `docs/chunks/governance/done/103-rustlogger-stale-ci-framing-and-untested-tui-subcommand/d1-run.md`:

1. **0 × 0 window.** `PtySession::spawn_command`
   (`rustlogger/src/pty_session/unix.rs`) never sets a window size on the
   new pty, so the child's `TIOCGWINSZ` returns 0 rows × 0 columns. A
   ratatui/crossterm TUI then draws empty frames. Interactive mode
   (`session::run`) has the same gap: `docs/rustlogger-design.md` lists
   SIGWINCH propagation as out of scope.
2. **No input.** `headless_loop` polls only the pty master and never
   reads rustlogger's own stdin, so keys never reach the child (`q`/Esc
   cannot quit the TUI).
3. **Exit code lost.** The only way out is a signal: SIGINT gives exit
   130 and a transcript ending `exit code: (none)`.

## Required behaviour

- **W1. Initial size, both modes.** If one of rustlogger's stdin, stdout
  or stderr is a terminal, read its size (`TIOCGWINSZ`). Then set that
  size on the child pty (`TIOCSWINSZ`) before the child execs, so the
  child's first query already sees it. `nix::libc` already provides both
  ioctls (`pty_session/unix.rs` uses `libc::ioctl` for `TIOCSCTTY`), so
  no new crate and no new nix feature should be needed. If one turns out
  to be needed, it goes through `docs/crate-checklist.md` first
  (CLAUDE.md rule 4).
- **W2. Resizes, both modes.** On SIGWINCH, re-read the outer size and
  set it on the master; the kernel then signals the child's foreground
  process group. Use the existing record-in-handler / act-in-loop
  pattern (`signals/unix.rs`: `record` + `received()`; `poll()` returns
  `EINTR`). SIGWINCH must **not** be a stop reason.
- **I1. Input in headless mode, only when stdin is a terminal.**
  Raw-mode stdin (`terminal::RawGuard`, restored on every exit path) and
  forward its bytes to the master, as `proxy_loop` does. Headless mode
  keeps `stoplogger` detection off. Stdin reaching EOF must not end a
  headless session.
- **I2. Input logging: open, ask the user.** Interactive mode logs input;
  headless mode logs output only. Logging forwarded keys keeps the
  transcript complete, but it also records anything typed, passwords
  included. Recommendation: leave the log format unchanged (output only)
  unless the user says otherwise.
- **X1. Exit code.** A child that exits after a forwarded key (TUI quit
  with `q`) goes through the existing `ChildExited` → `finish_session`
  path, and rustlogger exits with the child's code.
- **N1. No terminal: no change.** When none of stdin/stdout/stderr is a
  terminal, behaviour and log bytes stay exactly as they are today: no
  ioctl, no raw mode, no stdin read. That covers the MCP server, which
  spawns rustlogger with `stdio: ["ignore", fd, fd]`
  (`rustlogger-mcp-server/src/sessionManager.ts:114-117`).
- **P1. Windows.** Out of scope unless it is trivial (ConPTY has
  `ResizePseudoConsole`). `cargo check --target x86_64-pc-windows-gnu -p
  rustlogger --all-targets` and clippy for that target must stay clean
  (chunk 9).

## Tests

All of these run the compiled binary under a real pty, the way
`tests/session_end_to_end.rs` already does via `PtySession::spawn_command`:

- **T1, initial size:** outer pty set to 40 × 120; `rustlogger stty size`
  in headless mode prints `40 120`.
- **T2, resize:** the child is
  `sh -c 'trap "stty size" WINCH; while :; do sleep 0.1; done'`. Change
  the outer size, then send rustlogger SIGWINCH; the child prints the new
  size. Cover interactive mode too.
- **T3, input and exit code:** with `rustlogger sh -c 'read x; exit $x'`,
  writing `7\n` to the outer pty makes rustlogger exit 7.
- **T4, no terminal:** `rustlogger echo hi </dev/null >file` gives the
  same output and log as before, with no raw-mode or ioctl error.
- **T5, termios restored:** the outer termios is restored after a child
  exit, after SIGINT and after SIGTERM.

The existing suite stays green (`cargo test -p rustlogger`; CLAUDE.md
rule 3: run it yourself).

## Done when

- W1, W2, I1, X1 and N1 hold, T1–T5 pass, and I2 is decided by the user.
- `docs/rustlogger-design.md`, `README.md`, `HOWTO.md` and the man page
  are updated, and the SIGWINCH out-of-scope note is removed.
  `rustlogger/man/rustlogger.1` had someone's uncommitted edits on
  2026-10-24; don't overwrite them.
- `docs/TODO-rustlogger.md` chunk 10 is checked, with the test count.
- Hand-back: give the virtualisation repo's Chunk 123 the commit. It
  re-runs `d1-run.md`'s case (`script(1)` plus a FIFO): the TUI draws,
  `q` quits it, and exit 0 comes back through `track.sh`. Its own
  directions 2 (`track.sh` refuses `tui`) and 3 (`embodied tui` refuses
  a 0 × 0 terminal) are the interim guard until this lands.
