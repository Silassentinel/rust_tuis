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

Then check the log:

```bash
cat rustlogger-20260723-143207.log
```

## 4. Troubleshooting

- **`man rustlogger` says "No manual entry"**: run `manpath` and confirm the
  directory you installed into (Option A or B above) is actually listed. If
  you just ran `sudo mandb`, that only affects system directories — a
  per-user directory needs `MANPATH` set as in Option B, not `mandb`.
- **Log file not where you expected**: rustlogger always logs into the
  directory it was *launched* from, not the binary's own location — `cd`
  first if you want it somewhere specific. There's no `--output` flag yet
  (see `docs/rustlogger-design.md`'s non-goals).
- **Colors/interactive programs look wrong in the log**: expected — the log
  is a plain-text transcript including raw ANSI escape codes exactly as they
  were displayed, not a rendered/cleaned-up view. `cat -v` or `less -R` on
  the log will show it closer to how it looked live.

See `README.md` for a quicker reference and `docs/rustlogger-design.md` /
`docs/TODO-rustlogger.md` (repo root) for the architecture and build history.
