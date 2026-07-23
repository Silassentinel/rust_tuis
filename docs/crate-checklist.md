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
