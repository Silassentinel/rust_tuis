# How to build, install, and use rustlogger

## 1. Build it

From the repo root:

```bash
cargo build --release -p rustlogger
```

The binary lands at `target/release/rustlogger`. (Drop `--release` for a
faster, unoptimized build while developing — `target/debug/rustlogger`.)

## 2. Install the man page (`man rustlogger`)

The man page source lives at `rustlogger/man/rustlogger.1`. It's not picked
up automatically just by being in the repo — `man` only looks in directories
listed in its search path (`manpath`). You have two options:

### Option A — system-wide (needs `sudo`, works for every user)

```bash
sudo cp rustlogger/man/rustlogger.1 /usr/local/share/man/man1/
sudo mandb
```

`/usr/local/share/man` is already in this system's default `manpath`, so no
further configuration is needed. After this, `man rustlogger` works from
anywhere, for anyone on the machine.

### Option B — just for your user (no `sudo`)

```bash
mkdir -p ~/.local/share/man/man1
cp rustlogger/man/rustlogger.1 ~/.local/share/man/man1/
```

Then add `~/.local/share/man` to your `MANPATH` so `man` actually looks
there — add this line to `~/.bashrc` (or `~/.zshrc`):

```bash
export MANPATH="$HOME/.local/share/man:$MANPATH"
```

and reload your shell (`source ~/.bashrc`) or open a new terminal. This
system doesn't scan `~/.local/share/man` by default, so this step is
required — check your own `manpath` output first if you're unsure whether
you already have a user man directory configured.

### Just want to read it right now, no install?

```bash
man -l rustlogger/man/rustlogger.1
```

works immediately, with zero setup, from the repo.

## 3. Run your first logged session

```bash
./target/release/rustlogger
```

You'll see something like:

```
rustlogger: logging session to rustlogger-20260723-143207.log
```

From here it's just your normal shell — run whatever you'd normally run.
When you're done, either:

- exit the shell normally (`exit`, Ctrl+D) — rustlogger exits with the
  shell's own exit code, or
- type `stoplogger` on its own line to end the session without necessarily
  exiting the shell.

Then check the log — use `cat -v`, not a plain `cat`:

```bash
cat -v rustlogger-20260723-143207.log
```

The log is a byte-exact transcript, which means it contains whatever escape
sequences the programs in your session emitted. Displaying it with a plain
`cat` (or `less -R`) re-executes those sequences in *your* terminal. For your
own session that's usually just noise; for a transcript of anything you don't
fully trust it's a real hazard — see "Viewing a log safely" below.

## 4. Headless tracking mode (for scripting, or the MCP server)

```bash
./target/release/rustlogger npm run build
```

Runs `npm run build` (in place of your shell) attached to a pty, logs it the
same way, and also mirrors its output to rustlogger's own stdout — but
touches no outer terminal: no raw mode, no `stoplogger` detection (there's
no live keystroke stream to watch for it). To end it early, send rustlogger
itself a signal:

```bash
kill <pid>
```

which sends the tracked command `SIGHUP` first, so it isn't left running
detached, then reaps it and closes the log with that as the recorded reason.

This is what the `rustlogger-mcp-server` project (alongside this one) uses
to let Claude start a program in the background and check its log later —
see that project's own README for the tools it exposes.

## 5. Choosing where the log goes (`--log-dir`)

By default the log lands wherever you launched rustlogger from. For a
one-off interactive session that's usually what you want, but for something
that runs rustlogger repeatedly and unattended — a git hook firing on every
commit, say — that scatters a fresh log file into every repo's working
directory. Point it somewhere else instead:

```bash
./target/release/rustlogger --log-dir ~/.rustlogger/logs
```

or, for headless tracking mode (the flag goes before the tracked command):

```bash
./target/release/rustlogger --log-dir ~/.rustlogger/logs npm run build
```

The directory is created for you if it doesn't already exist. `RUSTLOGGER_LOG_DIR`
works the same way as an environment-variable default, for when you'd rather
set it once (e.g. in your shell profile, or the hook's own environment) than
pass the flag every time:

```bash
export RUSTLOGGER_LOG_DIR=~/.rustlogger/logs
./target/release/rustlogger npm run build   # picks up $RUSTLOGGER_LOG_DIR
```

`--log-dir` wins if both are set. Either way, only the very first `--log-dir`
rustlogger sees counts — one appearing after the tracked command's own name
belongs to that command instead, e.g. `rustlogger --log-dir ~/.rustlogger/logs
mytool --log-dir /elsewhere` passes `--log-dir /elsewhere` through to `mytool`
untouched.

## 6. Viewing a log safely

rustlogger stores the transcript **byte-for-byte**, deliberately: its job is
to be an honest record of what actually crossed the terminal, so it does not
strip or rewrite anything a program printed. The flip side is that a log is
untrusted content — everything the tracked program emitted is in there
verbatim, including terminal escape sequences it chose to print.

Rendering those sequences in a live terminal is what makes them dangerous.
A transcript can rewrite your window title, clear your screen, write to your
clipboard (OSC 52), use `\r` to overwrite a line so what you *see* differs
from what the file actually contains, or forge lines that look exactly like
rustlogger's own `=== rustlogger session ended … ===` / `reason:` /
`exit code:` footer.

**Safe:**

```bash
rustlogger --view rustlogger-20260723-143207.log   # renders escapes as visible text (^[ etc.)
cat -v rustlogger-*.log      # same idea, if you'd rather not name the exact file
less rustlogger-*.log        # without -R, less escapes control chars itself
grep something rustlogger-*.log
```

`rustlogger --view <path>` exists so you don't have to remember the `-v`/no-
`-R` rule under time pressure — it's the same rendering (caret notation:
`ESC` → `^[`, a bare `CR` → `^M`, high-bit bytes get an `M-` prefix), built
in rather than left to `cat` folklore. It only reads and renders the file;
it never wraps a shell or writes anything.

**Unsafe on a log you don't fully trust:**

```bash
cat  rustlogger-*.log        # executes the escape sequences in your terminal
less -R rustlogger-*.log     # -R explicitly passes them through
```

For your own local session this is mostly a cosmetic annoyance. It matters
when you're reviewing a transcript of something untrusted — a CI job, a
build script, anything another person or an agent kicked off. Treat the
footer as advisory rather than authoritative when it matters: a program
inside the session can print lines that look identical to it.

The [rustlogger MCP server](../rustlogger-mcp-server/README.md) sanitizes
escape sequences out before handing log text to a model, for exactly this
reason — the raw file stays honest, and each consumer makes it safe for its
own display.

## 7. Troubleshooting

- **`man rustlogger` says "No manual entry"**: run `manpath` and confirm the
  directory you installed into (Option A or B above) is actually listed. If
  you just ran `sudo mandb`, that only affects system directories — a
  per-user directory needs `MANPATH` set as in Option B, not `mandb`.
- **Log file not where you expected**: by default rustlogger logs into the
  directory it was *launched* from, not the binary's own location. Use
  `--log-dir <path>` (or `RUSTLOGGER_LOG_DIR`) to choose explicitly — see
  section 5.
- **Colors/interactive programs look wrong in the log**: expected — the log
  is a plain-text transcript including raw ANSI escape codes exactly as they
  were displayed, not a rendered/cleaned-up view. See "Viewing a log safely"
  above for how to read it without handing those escapes to your terminal.
- **`stoplogger` ended a session you didn't mean to end**: the trigger is
  matched against everything you type, with no way to tell whether your shell
  or some other program is consuming it. Typing or pasting `stoplogger` on
  its own line into an editor, a pager, a heredoc, or an interactive database
  client ends the session too. This is a known limitation of the current
  design — see `docs/rustlogger-design.md`.

See `README.md` for a quicker reference and `docs/rustlogger-design.md` /
`docs/TODO-rustlogger.md` (repo root) for the architecture and build history.
