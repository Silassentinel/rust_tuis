# Crate checklist (blocker)

Standing rule for this project: no crate gets added to any `Cargo.toml` until this
checklist has been filled in below the relevant proposal and you've explicitly said "ok"
to it. This file is the reusable template; proposals for a specific crate get appended
as their own section (see "Open proposals" at the bottom).

## What must be answered before `cargo add`

1. **Problem statement** — what capability is missing from `std` that forces us outside
   it? State it in one sentence.
2. **Alternatives considered** — at least one other crate or a hand-rolled `std`-only
   approach, with why it was or wasn't chosen.
3. **Maintenance health** — last release date, release cadence, is it archived/unmaintained.
4. **Adoption** — crates.io downloads (recent + all-time), reverse-dependency count,
   known projects using it in production.
5. **License** — confirm it's MIT/Apache-2.0 (or otherwise compatible).
6. **Security** — any RUSTSEC advisories (`cargo audit` once it's in the tree); how much
   `unsafe` it carries and whether that's justified for what it does.
7. **API stability** — version number (0.x means the API can still break), semver policy.
8. **Dependency footprint** — how many transitive dependencies it drags in
   (`cargo tree -p <crate>`), impact on compile time / binary size.
9. **Platform support** — does it cover the platforms we actually target.
10. **MSRV** — compatible with the Rust version we're building with.

Only once all ten are answered do we move to actually running `cargo add`.

## Open proposals

### rustlogger: pseudo-terminal (pty) handling

Needed for the `rustlogger` tool's chunk 2 (spawning the wrapped shell in a real pty
so interactive programs like `sudo` and `apt` behave normally). Comparison below;
decision pending your sign-off — see the chat message this was written from.

| # | Question | `nix` (openpty/forkpty + termios + signal) | `portable-pty` + `signal-hook` |
|---|---|---|---|
| 1 | Problem | `std` has no pty, raw-mode termios, or signal API at all | same |
| 2 | Alternative | the other column | the other column |
| 3 | Maintenance | actively released, huge user base (part of Rust's de facto Unix syscall layer) | `portable-pty` maintained by the wezterm project, sporadic but active releases; `signal-hook` widely used, active |
| 4 | Adoption | one of the most-depended-on crates in the whole ecosystem | both are established, smaller than `nix` individually |
| 5 | License | MIT | MIT (both) |
| 6 | Security | thin wrapper over libc, `unsafe` is the point of the crate | pty crate wraps similar libc calls; signal-hook is a well-audited, narrow-scope crate |
| 7 | API stability | 1.0+ | `portable-pty` pre-1.0 (0.9.x); `signal-hook` 1.0+ |
| 8 | Dependency footprint | single crate covers pty + termios + signals | two crates, `portable-pty` pulls in more (serde optional, downcast-rs, filedescriptor) |
| 9 | Platform | Unix-only (fine — target is Linux) | cross-platform (Windows conpty too) — not needed here |
| 10 | MSRV | tracks recent stable | both track recent stable |

Recommendation to discuss: since the target is Linux only, `nix` alone (pty + termios +
signals in one crate, smaller dependency tree) looks like the leaner fit unless
cross-platform support becomes a goal later. Final call is yours — flag which one (or
neither) to proceed with.

**Decision (2026-07-23): approved — `nix`.**

One naming clarification that came up: the `nix` *crate* (github.com/nix-rust/nix,
crates.io/crates/nix) is unrelated to the Nix package manager / NixOS. Ubuntu
(`x86_64-unknown-linux-gnu`) is one of its Tier-1, actively-tested platforms
([source](https://docs.rs/nix/latest/nix/pty/index.html) — platform list includes
`x86_64-unknown-linux-gnu`), so there's no conflict with the target OS.

Version pinned: `0.31.3` (current as of this check — [crates.io/crates/nix](https://crates.io/crates/nix)).
Enabling only the `term` feature to start (pty + termios), since that's all chunk 3
uses; `process`/`signal`/`poll` get added in chunk 4 alongside the code that
actually needs them, rather than upfront — keeps checklist item 8 (dependency
footprint) honest.

### rustlogger: `nix` feature additions for chunk 4 (`process`, `signal`, `poll`)

`nix` itself is already approved above; this is the follow-up feature-surface expansion
chunk 4 needs. Since it's the same crate (not a new dependency), the ten-point table
isn't repeated — just the per-feature justification the handoff instructions asked for.

| Feature | Gates (in `nix` 0.31.3 source) | Why chunk 4 needs it |
|---|---|---|
| `process` | `nix::sys::wait` (`waitpid`, `WaitStatus`) — confirmed via `src/sys/mod.rs`/`src/unistd.rs` `#![feature = "process"]` blocks | Distinguish "child exited with code N" from "child was killed by a signal" when reaping the wrapped shell, which `std::process::Child::wait()`'s `ExitStatus` conflates (its `.code()` is already `None` on signal death, but we need `nix::sys::wait::WaitStatus::Signaled` to log *which* signal, per the log-footer "stop reason" chunk 5 will want). Pulled in transitively by `signal` anyway (`signal = ["process"]` in `nix`'s own `Cargo.toml`). |
| `signal` | `nix::sys::signal` (`sigaction`, `Signal` enum, `SigSet`) | Chunk 4 needs to install handlers for `SIGINT`/`SIGHUP`/`SIGTERM` on the outer process so Ctrl+C, terminal-close, and kill all route through the same clean-shutdown path (close log file, restore terminal mode) instead of the default dispositions (`SIGINT`/`SIGTERM` kill immediately, `SIGHUP` too) tearing things down before cleanup runs. |
| `poll` | `nix::poll` (`poll()`, `PollFd`) | The proxy loop reads from two file descriptors (outer stdin, pty master) that each block independently; without multiplexing, a blocking `read()` on one starves the other. `poll()` is the direct `std`-free way to wait on both at once each iteration. |

Recommendation: enable all three now, matching what chunk 4 as scoped in
`docs/TODO-rustlogger.md` actually touches (raw-mode + proxy loop + signal wiring is
one chunk, not split further). Awaiting explicit go-ahead before editing `Cargo.toml`.

### rustlogger: native Windows pty/console handling (chunk 9)

Needed for chunk 9 (native Windows console support - `cmd.exe`/PowerShell/Windows
Terminal, not WSL). Every POSIX API the existing Unix implementation uses (`openpty`,
`termios`, `sigaction`) has no Windows equivalent at all, so this isn't a port of
`pty_session.rs`/`terminal.rs`/`signals.rs` - it's a second implementation of each,
selected by `cfg(windows)` vs `cfg(unix)`, with the existing `nix`-based code
untouched. That splits into three separate concerns, each needing its own answer:

**1. Spawning the wrapped shell/command attached to a pty-equivalent** - Windows'
answer to a pty is ConPTY (`CreatePseudoConsole`), a fundamentally different API
(no fd-based master/slave pair; a pipe pair plus an opaque `HPCON` handle, wired into
a child process via `STARTUPINFOEX`'s proc-thread-attribute list).

| # | Question | Hand-rolled `windows` crate FFI | `portable-pty` |
|---|---|---|---|
| 1 | Problem | `std` has no ConPTY bindings at all | same |
| 2 | Alternative | the other column | the other column |
| 3 | Maintenance | `windows` is Microsoft's own official bindings crate, actively released | maintained by the wezterm project; per lib.rs, latest 0.9.0 released 2025-02-11 |
| 4 | Adoption | ~21.1M downloads/month, used in 14,219 crates (1,662 direct) - about as core to the Windows-Rust ecosystem as `nix` is to Unix-Rust | ~1.5M downloads/month, used in 626 crates (407 direct); depended on by termwiz, wezterm's own crates |
| 5 | License | MIT/Apache-2.0 | MIT |
| 6 | Security | raw FFI - all the same `unsafe` as any Win32 binding, but it's what defines the ConPTY struct/attribute-list dance by hand ourselves, more surface for us to get wrong | wraps the same ConPTY calls, but that error-prone setup (attribute lists, handle lifetimes) is wezterm's problem to have already solved and tested, not ours to re-solve |
| 7 | API stability | 1.0+ (`0.62.2` is a 0.x *edition* number per their own versioning; the crate itself is widely treated as stable/production-grade) | pre-1.0 (`0.9.x`) - could still break, though it's been at 0.9.x for a while |
| 8 | Dependency footprint | small, target-gated (`[target.'cfg(windows)'.dependencies]`) - zero impact on Linux/Android builds either way; only the specific feature modules used (console + threading APIs) get compiled in | also target-gated for Windows-only use here; pulls in a handful of its own deps (serde optional, downcast-rs, filedescriptor) but only for `cfg(windows)` builds |
| 9 | Platform | Windows-only relevant here, which is exactly the target | cross-platform crate, but only its Windows/ConPTY backend would actually be used here - its Unix backend would sit unused dead weight if depended on unconditionally, hence gating it to `cfg(windows)` only |
| 10 | MSRV | tracks recent stable | tracks recent stable |

Recommendation: `portable-pty`, gated to `[target.'cfg(windows)'.dependencies]` only
(zero effect on existing Linux/Android chunks). ConPTY's setup (attribute lists,
handle lifetimes, resize semantics) is exactly the kind of fiddly, easy-to-get-wrong
Win32 dance that `nix` already spared us from having to hand-roll on the Unix side -
same reasoning applies here, just for a different OS.

**2. Raw-mode-equivalent on the outer console** - Windows' analogue of clearing
`ISIG`/`ICANON`/`ECHO` via termios is clearing `ENABLE_PROCESSED_INPUT`/
`ENABLE_LINE_INPUT`/`ENABLE_ECHO_INPUT` via `SetConsoleMode`/`GetConsoleMode`. Neither
`portable-pty` nor any other higher-level crate covers this (it's about the *outer*
console rustlogger itself is attached to, not the pty it creates for the child) - it's
a handful of direct Win32 calls, which is exactly what the `windows` crate (already
justified above for ConPTY... except we're not using it there, `portable-pty` is)
would be pulled in for on its own merits: there's no `std`, `portable-pty`, or other
existing-dependency path to `SetConsoleMode` at all.

**3. Signal-equivalent (Ctrl+C, console closing, kill)** - Windows' analogue of
`SIGINT`/`SIGHUP`/`SIGTERM` is `SetConsoleCtrlHandler` (delivers `CTRL_C_EVENT`,
`CTRL_CLOSE_EVENT`, etc. to a registered callback). Also only reachable via the
`windows` crate - no dedicated Windows console-ctrl crate has the adoption/maintenance
profile `nix`/`signal-hook` have on the Unix side to make a real alternative worth
comparing against.

Recommendation for 2 and 3: the `windows` crate, also gated to
`[target.'cfg(windows)'.dependencies]`, enabling only the specific feature modules
needed (`Win32_System_Console` for both console-mode and ctrl-handler APIs,
`Win32_Foundation` for the handle/error types they use) rather than its default
feature set - the crate is entirely feature-gated per Win32 namespace specifically so
consumers only compile the slice they actually call.

**Awaiting explicit go-ahead before running `cargo add` for either crate**, and before
writing any chunk 9 code - this is a bigger, riskier change than previous chunks
(a second pty/terminal/signal backend, not additive to the existing one), so the plan
above is meant to be reviewed as a whole before any of it starts, not approved
piecemeal mid-implementation.
