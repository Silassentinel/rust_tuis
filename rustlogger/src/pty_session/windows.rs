//! Windows equivalent of `pty_session/unix.rs`: spawns the wrapped
//! shell/command attached to a ConPTY-backed pseudo-console via
//! `portable-pty`, which wraps the fiddly attribute-list/handle-lifetime
//! setup `CreatePseudoConsole` itself needs - see `docs/crate-checklist.md`
//! ("rustlogger: native Windows pty/console handling") for why this crate
//! rather than hand-rolled FFI here specifically.
//!
//! Unlike the Unix implementation, there's no single bidirectional
//! fd/File for the pty master - portable-pty exposes separate reader and
//! writer handles instead, so this `PtySession` carries `reader`/`writer`
//! fields rather than a single `master: File`. `session/windows.rs`'s
//! proxy loop is written against this shape directly.
//!
//! **Unverified beyond `cargo check`** - there's no Windows machine or CI
//! runner available while writing this, so none of it has actually been
//! built and run. See `docs/rustlogger-design.md`'s chunk 9 notes for
//! exactly what that leaves unconfirmed (most importantly: does EOF on
//! the ConPTY reader really show up as a clean `Ok(0)`, the way this
//! module assumes, or does it need an equivalent to the Unix `EIO`-at-EOF
//! quirk `session/unix.rs` had to special-case?).

use std::io::{self, Read, Write};
use std::process::Command;

use portable_pty::{native_pty_system, Child as PtyChild, CommandBuilder, MasterPty, PtySize};

/// A shell/command running attached to a ConPTY, plus the reader/writer
/// handles used to talk to it (write = keystrokes in, read = everything
/// the child prints).
pub struct PtySession {
    child: Box<dyn PtyChild + Send + Sync>,
    /// The pseudoconsole itself. Never read after construction - kept
    /// alive purely for `Drop` timing. Microsoft's ConPTY docs (and
    /// `portable-pty`'s own Windows backend, `src/win/conpty.rs`, which
    /// stores the equivalent of this behind an `Arc` shared with the
    /// reader/writer it hands out) treat the pseudoconsole as needing to
    /// outlive anything still reading/writing it; the first version of
    /// this struct didn't store this field at all, so `spawn_command`
    /// dropped it - and, with it, the underlying `ClosePseudoConsole` -
    /// the instant the function returned, before a single byte had been
    /// read. `reader`/`take_writer` clone/move independent OS handles out
    /// (confirmed by reading `try_clone_reader`/`take_writer`'s
    /// implementations), so this isn't required for the handles to work
    /// at the OS level, but closing the pseudoconsole this early is
    /// exactly the mistake ConPTY's own docs warn against, so it's kept
    /// around defensively.
    #[allow(dead_code)]
    master: Box<dyn MasterPty + Send>,
    reader: Option<Box<dyn Read + Send>>,
    writer: Option<Box<dyn Write + Send>>,
    /// There's no Windows equivalent of a pty device path (`/dev/pts/N`)
    /// to report here - a fixed, descriptive placeholder for the log
    /// header's `tty:` field instead.
    pub tty: String,
}

impl PtySession {
    /// Spawn `shell` attached to a new ConPTY.
    pub fn spawn(shell: &str) -> io::Result<Self> {
        Self::spawn_command(Command::new(shell))
    }

    /// Like `spawn`, but takes an already-configured `std::process::Command`,
    /// the same public shape as the Unix implementation, so callers
    /// (including the integration tests under `tests/`, which build one
    /// via the standard library directly) don't need any platform-specific
    /// code of their own. Translated into a `portable_pty::CommandBuilder`
    /// internally via `Command`'s stable introspection getters
    /// (`get_program`/`get_args`/`get_envs`/`get_current_dir`) - the two
    /// builder APIs are otherwise incompatible types.
    pub fn spawn_command(command: Command) -> io::Result<Self> {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(to_io_error)?;

        let builder = command_builder_from(&command);
        let child = pair.slave.spawn_command(builder).map_err(to_io_error)?;
        // The slave side isn't needed once the child has it - mirrors why
        // pty_session/unix.rs drops the pty's slave fd after spawning.
        drop(pair.slave);

        let reader = pair.master.try_clone_reader().map_err(to_io_error)?;
        let writer = pair.master.take_writer().map_err(to_io_error)?;

        Ok(PtySession {
            child,
            master: pair.master,
            reader: Some(reader),
            writer: Some(writer),
            tty: "CONPTY".to_string(),
        })
    }

    /// Block until the wrapped shell/command exits, returning its exit
    /// code. Unlike the Unix implementation, this never returns `None`:
    /// portable-pty's Windows `Child::wait()` always constructs its
    /// `ExitStatus` via `with_exit_code` (confirmed by reading
    /// `portable-pty`'s own source, `src/win/mod.rs`) - `.signal()` is
    /// never populated on this platform. A forcibly-terminated Windows
    /// process still reports a (often large) numeric exit code; there's
    /// no real Windows equivalent of "no code at all" worth preserving.
    pub fn wait(&mut self) -> io::Result<Option<i32>> {
        let status = self.child.wait()?;
        Ok(Some(status.exit_code() as i32))
    }

    /// Asks the wrapped shell/command to stop. `portable_pty::Child`
    /// requires `&mut self` for this (unlike the Unix implementation's
    /// `terminate(&self)`, a plain `kill(pid, SIGHUP)` needing no mutable
    /// access) - callers already hold `&mut PtySession` by the time they
    /// call this (see `session::finish_session`), so the difference
    /// doesn't leak out to shared code.
    ///
    /// **Unverified**: `Child::kill()` on Windows terminates the process
    /// outright (there's no direct Windows equivalent of `SIGHUP`'s
    /// "please wrap up" semantics to ask for instead) - unlike Unix,
    /// where the wrapped shell gets a chance to run its own exit/cleanup
    /// handling before actually dying.
    pub fn terminate(&mut self) -> io::Result<()> {
        self.child.kill()
    }

    /// Non-blocking counterpart to [`wait`](Self::wait), for
    /// `session::stop_and_reap`'s escalation loop. Same contract as the
    /// Unix implementation, except the inner `Option<i32>` is always
    /// `Some` here for the reason [`wait`](Self::wait) documents.
    pub fn try_wait(&mut self) -> io::Result<Option<Option<i32>>> {
        Ok(self
            .child
            .try_wait()?
            .map(|status| Some(status.exit_code() as i32)))
    }

    /// Windows has no signal hierarchy to escalate through: `Child::kill()`
    /// is `TerminateProcess`, which the target cannot catch, block or
    /// ignore. So both escalation steps the Unix side distinguishes
    /// (`SIGTERM`, then `SIGKILL`) collapse into the same call as
    /// [`terminate`](Self::terminate) here. The escalation loop in
    /// `session::stop_and_reap` still serves a purpose on this platform -
    /// it bounds how long rustlogger waits before giving up and writing
    /// the footer anyway, rather than blocking in `wait()` forever.
    pub fn terminate_forcefully(&mut self) -> io::Result<()> {
        self.child.kill()
    }

    /// See [`terminate_forcefully`](Self::terminate_forcefully): identical
    /// on Windows, kept as a separate name so shared code in
    /// `session/mod.rs` reads the same on both platforms.
    pub fn kill_now(&mut self) -> io::Result<()> {
        self.child.kill()
    }

    /// Takes ownership of the pty master's readable side (everything the
    /// wrapped shell/command prints) out of this session. Returns an
    /// owned handle rather than a borrow because `session/windows.rs`
    /// hands it to a dedicated OS thread it doesn't join before
    /// returning - a borrow tied to the caller's stack frame couldn't
    /// cross that boundary without an unnatural `'static` requirement on
    /// the caller itself. Panics if called twice; `proxy_loop`/
    /// `headless_loop` each call it exactly once per session, the same
    /// one-shot contract `portable_pty::MasterPty::take_writer` already
    /// documents for itself.
    pub fn take_reader(&mut self) -> Box<dyn Read + Send> {
        self.reader.take().expect("PtySession::take_reader called twice")
    }

    /// Takes ownership of the pty master's writable side (keystrokes/input
    /// going in) out of this session. See `take_reader` for why this
    /// hands over ownership rather than a borrow, and its one-shot
    /// contract.
    pub fn take_writer(&mut self) -> Box<dyn Write + Send> {
        self.writer.take().expect("PtySession::take_writer called twice")
    }
}

/// Translates a `std::process::Command`'s program/args/env/cwd into an
/// equivalent `portable_pty::CommandBuilder`. `CommandBuilder::new`
/// already inherits the current process's environment the same way
/// `std::process::Command` does by default (confirmed via
/// `portable-pty`'s own source, `src/cmdbuilder.rs`'s `get_base_env`), so
/// only explicitly-set env vars need copying across.
///
/// Known gap: a `Command::env_remove`'d variable shows up in
/// `get_envs()` as `(key, None)`, which this doesn't translate into a
/// `CommandBuilder::env_remove` call - not a real concern for anything
/// rustlogger itself constructs (it never calls `env_remove`), but worth
/// knowing if this function is ever reused for something that does.
fn command_builder_from(command: &Command) -> CommandBuilder {
    let mut builder = CommandBuilder::new(command.get_program());
    builder.args(command.get_args());
    for (key, value) in command.get_envs() {
        if let Some(value) = value {
            builder.env(key, value);
        }
    }
    if let Some(dir) = command.get_current_dir() {
        builder.cwd(dir);
    }
    builder
}

/// `portable-pty`'s own error type is `anyhow::Error`, not `io::Error` -
/// round-tripped through its `Display` text rather than pulling in an
/// extra conversion crate, matching this project's existing
/// `nix_err_to_io`-style per-module helpers on the Unix side. Generic
/// over `Display` rather than naming `anyhow::Error` directly: `anyhow`
/// is only a transitive dependency (pulled in by `portable-pty` itself),
/// not one this crate declares, so Rust's extern prelude won't resolve
/// `anyhow::Error` by name here even though the type is the same at
/// runtime - see `docs/crate-checklist.md` before ever adding it as a
/// direct dependency just to name the type.
fn to_io_error<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::other(e.to_string())
}
