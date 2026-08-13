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

That file is named after the UTC time the session started, and by default
lands in whatever directory you launched rustlogger from. Pass `--log-dir
<path>` (or set `RUSTLOGGER_LOG_DIR=<path>`; the flag wins if both are given)
to put it somewhere else instead — the directory is created if it doesn't
already exist. Useful for automated/repeated invocations (a git hook firing
on every commit, say) where scattering a log into whatever the caller's cwd
happens to be isn't wanted:

```bash
rustlogger --log-dir ~/.rustlogger/logs
```

`--log-dir` (like `RUSTLOGGER_LOG_DIR`) is consumed strictly as rustlogger's
own leading argument — anything after the wrapped command's own name belongs
to it instead and is never reinterpreted, so `rustlogger --log-dir /x mycmd
--log-dir /y` passes the second `--log-dir` through to `mycmd` untouched.

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
reason: process exited
exit code: 0
```

Only what actually appeared on screen is logged, not raw keystrokes — which
means a prompt that turns off terminal echo (most commonly `sudo` asking for
a password) is never captured, since nothing was ever echoed to log. See
`docs/rustlogger-design.md`'s chunk 5 notes for why that's the correct
behavior rather than a limitation.

Everything *else* on screen is captured verbatim, though, so treat a log as
sensitive: it will contain any secret that was printed, pasted, or echoed
during the session. Logs are created `0600` (owner-only) for that reason.

### Reading a log

The transcript is stored byte-for-byte, escape sequences included, so that
it's an honest record. That makes it untrusted content: use `cat -v` (or
plain `less`) rather than `cat`/`less -R`, which hand those sequences to
your terminal to execute. A tracked program can also print lines that look
exactly like rustlogger's own footer. See HOWTO.md §6 for the details.

## Headless tracking mode

```bash
rustlogger [--log-dir <path>] <command> [args...]
```

`--log-dir`/`RUSTLOGGER_LOG_DIR` (see above) apply here too — must come
before `command`, since everything from `command` onward is that command's
own argv.

Runs `command` (not your shell) attached to a pty with no outer terminal
touched at all — no raw terminal mode, no `stoplogger` phrase (there's no
live keystroke stream to watch for it). Output is logged the same way and
also mirrored to rustlogger's own stdout. The session ends when `command`
exits, or when rustlogger itself is sent `SIGINT`/`SIGHUP`/`SIGTERM` (the
tracked command gets `SIGHUP` first, so it isn't left running).

This is what the [rustlogger MCP server](../rustlogger-mcp-server/README.md)
uses to let Claude start a program in the background, check its log later,
and stop it — see that project's README for the tools it exposes.

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
- Cross-platform support (native Windows console via ConPTY, Android via
  Termux) type-checks and passes the Linux test suite, but hasn't been built
  or run on either platform yet — see `docs/rustlogger-design.md`'s chunk 8/9
  notes. Actually tested and run today only on desktop Linux.
- No window-resize (`SIGWINCH`) propagation to the wrapped shell yet.
