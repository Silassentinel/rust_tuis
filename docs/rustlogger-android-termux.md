# rustlogger on Android (via Termux)

## Why Termux, and why build on-device

Stock Android has no concept of "run a CLI binary in your terminal" the way
desktop Linux does — apps are sandboxed, there's no shell environment a user
launches things from. [Termux](https://termux.dev/) is the exception: it's a
terminal emulator app that provides a real, POSIX-like userland (its own
`bash`, its own package manager `pkg`, its own prefix at
`/data/data/com.termux/files/usr`) running under Android's app sandbox. That
userland is what makes "wrap the shell you launched this in" a meaningful
thing to do on Android at all — this only targets Termux, not a native
Android app.

**Build on-device with Termux's own toolchain (`pkg install rust`), not by
cross-compiling from a desktop machine with the Android NDK.** Termux
binaries need to match Termux's own prefix/libc setup; NDK-built binaries
compiled elsewhere are a common source of subtle mismatches (wrong paths,
ABI assumptions that don't hold under Termux specifically) even though the
underlying OS is technically the same Android/bionic base. Building where
it'll actually run avoids that class of problem entirely.

## Status

- `cargo check --target aarch64-linux-android` and
  `--target armv7-linux-androideabi` both pass cleanly with no changes to
  rustlogger's source — every `nix` API it uses (`openpty`, `termios`,
  `signal`, `poll`) exists and type-checks for Android. That's a real,
  verified signal that the *source* is compatible.
- **Not yet verified: an actual on-device build and run.** There's no
  Android device or emulator available in this dev environment to test
  runtime behavior - whether `openpty`/`TIOCSCTTY`/raw termios mode/signal
  delivery actually behave correctly inside Termux's sandboxed environment,
  as opposed to just type-checking against Android's headers. Termux runs
  plenty of terminal multiplexers (`tmux`, `screen`) and interactive shells
  successfully, which is a good precedent, but that's not the same as this
  specific tool having been run and its tests passed there. Treat this the
  same way the project's own handoff convention treats any
  never-actually-run code: unverified until `cargo test` has actually gone
  green on-device.

## Building it, on a real device or emulator running Termux

```bash
pkg update
pkg install rust git
git clone https://github.com/Silassentinel/rust_tuis.git
cd rust_tuis
cargo test -p rustlogger
cargo build --release -p rustlogger
./target/release/rustlogger
```

Termux's own terminal already gives you a real pty, so interactive mode
should work the same way it does on desktop Linux - `$SHELL` inside Termux
is normally set correctly (its default shell is `bash`), so no special
handling should be needed there.

## What to actually check, on-device

- `cargo test -p rustlogger` - does the full suite (unit + integration,
  including the ones that spawn a real `/bin/sh` in a pty) actually pass,
  not just compile.
- Run it interactively: does raw mode behave correctly (no double-echo, no
  garbled input), does `stoplogger` still detect correctly, does exiting
  the wrapped shell end the session and restore the terminal cleanly.
- Ctrl+C / closing the Termux session: do `SIGINT`/`SIGHUP` still reach
  rustlogger and trigger clean shutdown the way they do on desktop Linux -
  Android's process lifecycle management is more aggressive about killing
  backgrounded app processes than a desktop OS, so this is the area most
  worth specifically confirming.
- Headless tracking mode (`rustlogger <command> [args...]`) - same checks,
  plus whether a tracked process survives if Termux itself gets backgrounded
  by Android (this is a real open question, not something to assume either
  way without testing).

## If cross-compiling from desktop Linux is wanted instead

Possible via the Android NDK (`aarch64-linux-android`/`armv7-linux-androideabi`
targets, both already added to this dev environment's rustup), but not done
here - the NDK is a large download (roughly 1GB), and for Termux specifically
the on-device build above is the more reliable path anyway. Worth doing only
if the goal shifts to producing prebuilt binaries for distribution rather
than building where it runs.
