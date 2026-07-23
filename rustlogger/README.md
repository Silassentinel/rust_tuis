# rustlogger

Wraps the shell in whatever terminal it's launched from and logs the whole
session — everything typed and everything printed — to a file, until the
wrapped shell exits, you type `stoplogger`, or the session ends any other way
(Ctrl+C, the terminal closing, or the process being killed).

## Usage

```bash
cargo run -p rustlogger
```

or, once built:

```bash
./target/debug/rustlogger
```

Your `$SHELL` (falling back to `/bin/sh`) runs attached to a real
pseudo-terminal, exactly as it would in a normal terminal — interactive
programs, colors, `sudo` prompts, job control, all work as usual. On start,
rustlogger prints where it's logging to:

```
rustlogger: logging session to rustlogger-20260723-143207.log
```

That file is written in whatever directory you launched rustlogger from,
named after the UTC time the session started.

### Ending a session

- Run whatever you'd normally run, then exit the shell as usual (e.g. `exit`,
  Ctrl+D) — rustlogger exits with the shell's own exit code.
- Type `stoplogger` on its own line at any point to end the session without
  necessarily exiting the shell yourself.
- Ctrl+C, closing the terminal window/tab, or killing the rustlogger process
  all end the session too, cleanly (the wrapped shell is sent `SIGHUP`, and
  your terminal's mode is always restored, before rustlogger exits).

## The log file

```
=== rustlogger session started 2026-07-23T14:32:07Z ===
shell: /bin/bash
tty: /dev/pts/4
[2026-07-23T14:32:09Z] $ echo hello
[2026-07-23T14:32:09Z] hello
[2026-07-23T14:32:12Z] $ exit
=== rustlogger session ended 2026-07-23T14:32:12Z ===
reason: wrapped shell exited
exit code: 0
```

Only what actually appeared on screen is logged, not raw keystrokes — which
means a prompt that turns off terminal echo (most commonly `sudo` asking for
a password) is never captured, since nothing was ever echoed to log. See
`docs/rustlogger-design.md`'s chunk 5 notes for why that's the correct
behavior rather than a limitation.

## Development

See `docs/rustlogger-design.md` for the architecture and the Rust Book
chapters that shaped it, and `docs/TODO-rustlogger.md` for build status.

```bash
cargo test -p rustlogger
```

runs both the unit tests (in each module under `src/`) and the end-to-end
integration tests (`tests/session_end_to_end.rs`), which spawn the compiled
binary itself under a real pty and check both its exit code and the log file
it produces.

## Non-goals (for now)

- Not a replacement for full asciinema-style timing/playback — this is a
  plain-text transcript, not a recorded-and-replayable format.
- Not cross-platform — Linux only, matching where it's actually run.
- No window-resize (`SIGWINCH`) propagation to the wrapped shell yet.
