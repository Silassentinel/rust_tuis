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

**Decision (2026-07-30): approved — both `portable-pty` and `windows`, gated to
`[target.'cfg(windows)'.dependencies]` exactly as scoped above.** Chunk 9 is
implemented: `pty_session/windows.rs`, `terminal/windows.rs`,
`signals/windows.rs`, and `session/windows.rs` all written, `cargo check
--target x86_64-pc-windows-gnu -p rustlogger --all-targets` and `cargo clippy`
for the same target both clean, full Unix regression suite (26 unit + 3
integration) still green. See `docs/rustlogger-design.md`'s chunk 9 notes for
the architecture and, importantly, exactly what "approved and implemented"
does *not* yet mean here (no real Windows machine has actually run any of
this) - and for the one place this implementation later needed a Win32 API
surface beyond the `Win32_System_Console`/`Win32_Foundation` scoped here
(`CancelSynchronousIo`, in `Win32_System_IO`/`Win32_System_Threading`, to
cleanly cancel a blocked worker thread) and deliberately did *not* expand
into it without coming back through this same process - that's flagged as a
known limitation instead, not a silent scope-creep.

---

# rustmon proposals (2026-08-01)

Three separate proposals for the `rustmon/` hardware monitor. **None is approved.**
Chunks 1–7 of `docs/TODO-rustmon.md` are deliberately scoped to need none of them —
`rustmon --once --format json` is a complete, dependency-free tool before any of this
is decided. Take them one at a time; there's no need to sign off on all three.

Figures below were pulled from the crates.io API on 2026-08-01 and are cited. Two
items I could **not** verify in this environment and have marked as such rather than
guessed: `cargo audit` output (no toolchain available) and exact transitive
dependency counts (`cargo tree` likewise). Both must be checked at sign-off time.

## rustmon A: `nix` (`fs` feature) — filesystem capacity via `statvfs`

Blocks chunk 8 only.

| # | Question | Answer |
|---|---|---|
| 1 | Problem | Free/used space on a mounted filesystem requires the `statvfs(3)` syscall. `std` exposes no equivalent — `fs::metadata` gives file size, not filesystem capacity, and there is nothing in `/proc` or `/sys` that reports per-mount free blocks. This is a genuine `std` gap, not a convenience. |
| 2 | Alternatives | (a) Hand-rolled `extern "C"` FFI to `statvfs` — doable and small, but means hand-writing the `struct statvfs` layout per-arch, which is exactly the error-prone `unsafe` the project already decided to delegate to `nix` for rustlogger. (b) `sysinfo` — far larger surface, brings its own collection layer that duplicates everything chunks 2–6 do by hand. (c) Ship without capacity numbers and keep only throughput — a real option; the feature is nice-to-have, not core. |
| 3 | Maintenance | Already assessed and approved for `rustlogger` (see the 2026-07-23 decision above). Same crate, same version line. |
| 4 | Adoption | As above — one of the most-depended-on crates in the ecosystem. |
| 5 | License | MIT. |
| 6 | Security | `unsafe` FFI is the crate's purpose. The `fs` feature's `statvfs` is a read-only syscall taking a path — no new attack surface beyond the path confinement `sysfs.rs` already enforces. **`cargo audit` not run — no toolchain in the authoring environment.** |
| 7 | API stability | 0.31.x line already pinned in this repo for `rustlogger`. |
| 8 | Footprint | `nix` is already in `Cargo.lock` for this workspace, so the marginal cost is the `fs` feature's compile time only — no new transitive dependencies. This is the strongest argument for it over any alternative. |
| 9 | Platform | Linux is a Tier-1 target (established in the rustlogger entry). |
| 10 | MSRV | Tracks recent stable; unchanged from the existing usage. |

**Recommendation:** approve, scoped to `features = ["fs"]` only. It's a crate already
in the tree, for one syscall `std` genuinely lacks. If you'd rather ship v1 without
per-mount capacity, option (c) is legitimate and costs nothing — say so and I'll
strike chunk 8.

**Decision (2026-08-12): approved — `nix`, `fs` feature, `cfg(unix)`-gated,
matching the existing rustlogger pin (`0.31`).**

## rustmon B: `ratatui` + `crossterm` — the TUI frontend

Blocks chunk 9 only. Both would sit behind the non-default-able `tui` feature so
`cargo build --no-default-features` stays dependency-free.

| # | Question | `ratatui` | `crossterm` |
|---|---|---|---|
| 1 | Problem | `std` has no layout engine, widget set, or double-buffered diff renderer. Hand-rolling those is a project in itself, not a chunk. | `std` has no raw mode, no alternate screen, no key/mouse event decoding. rustlogger hand-rolled raw mode via `nix` termios — viable, but it doesn't decode key events, which a TUI needs. |
| 2 | Alternatives | `cursive` (retained-mode, heavier, callback-driven); hand-rolled ANSI drawing (realistic for a fixed layout, but re-solves the diff-render problem). | `termion` (Unix-only, thinner); `termwiz` (heavier); reusing rustlogger's `nix` termios approach + writing our own escape-sequence input parser. |
| 3 | Maintenance | 0.30.2 released **2026-06-19**; 77 releases; actively developed, the maintained continuation of `tui-rs`. ([crates.io](https://crates.io/api/v1/crates/ratatui)) | 0.29.0 released **2025-04-05** — over a year with no release. Not archived and still the default ratatui backend, but flag it: this is the least-actively-released crate in these three proposals. ([crates.io](https://crates.io/api/v1/crates/crossterm)) |
| 4 | Adoption | 34,800,841 all-time / 13,103,713 recent downloads. | 39,983,072 recent downloads. Both are the de facto standard for Rust TUIs. |
| 5 | License | MIT. | MIT. |
| 6 | Security | Pure-Rust rendering, no FFI. Relevant risk is ours not theirs: kernel-supplied strings must be sanitised before rendering (security model item 7). | Does terminal I/O and mode setting; the `Drop`/panic-hook restore path is the thing to get right, same as rustlogger's `RawGuard`. **`cargo audit` not run.** |
| 7 | API stability | 0.x — breaks between minors, and 0.29→0.30 was a significant one. Accept that pinning is required. | 0.x, but has been stable at 0.2x for a long time. |
| 8 | Footprint | **Not verified** — `cargo tree` unavailable here. Known to pull `unicode-width`, `unicode-segmentation`, `bitflags`, `itertools`, `lru`, `strum`. Must be measured at sign-off. | Pulls `bitflags`, `parking_lot`, `signal-hook`, `mio`, plus `rustix`/`libc` on Unix. Also not verified. |
| 9 | Platform | Cross-platform; Linux is the target. | Cross-platform. |
| 10 | MSRV | **1.88.0** — the highest bar of anything proposed here. Confirm your toolchain meets it before approving. | 1.63.0. |

**Recommendation:** approve as a pair, gated behind `features = ["tui"]`, pinned to
`ratatui = "0.30.2"` and `crossterm = "0.29.0"`. Two things worth your explicit
attention rather than burying: crossterm hasn't had a release in over a year, and
ratatui's MSRV of 1.88.0 is high.

**Decision (2026-08-12): approved — `ratatui = "0.30.2"` + `crossterm = "0.29.0"`,
behind `features = ["tui"]` exactly as scoped above.**

## rustmon C: `nvml-wrapper` — NVIDIA GPU metrics

Blocks the NVIDIA half of chunk 10 only. AMD and Intel GPU support is plain sysfs
reading and needs nothing.

| # | Question | Answer |
|---|---|---|
| 1 | Problem | NVIDIA exposes essentially nothing useful through sysfs — no utilisation, no VRAM, no power. The only interface is NVML, a proprietary C library (`libnvidia-ml.so`) shipped with the driver. There is no file to read. |
| 2 | Alternatives | (a) Shell out to `nvidia-smi` — **rejected on security grounds**: the design model forbids subprocesses entirely (PATH hijack, command injection). (b) Hand-rolled FFI to NVML — same `dlopen` trust problem, plus we'd hand-write the ABI. (c) Don't support NVIDIA in v1 — perfectly viable if you're not on an NVIDIA machine. |
| 3 | Maintenance | 0.12.1 released **2026-03-30**, 14 releases since 2017, currently published by Brian Martin (`brayniac`) having passed from the original author. Actively maintained. ([crates.io](https://crates.io/api/v1/crates/nvml-wrapper)) |
| 4 | Adoption | 4,252,259 all-time / 853,313 recent downloads. Modest but healthy for a vendor-specific binding. |
| 5 | License | MIT OR Apache-2.0. |
| 6 | Security | **This is the substantive concern, and it's why this is its own proposal.** Using NVML means `dlopen`-ing a closed-source vendor library into rustmon's address space. Everything else in this crate reads text files; this loads foreign code. The wrapper itself is a thin safe layer over `unsafe` FFI — reasonable — but the trust decision is about NVIDIA's blob, not about this crate. Mitigation: non-default `gpu-nvidia` feature, so a default build never loads it, and initialisation failure degrades to "GPU absent" rather than erroring. **`cargo audit` not run.** |
| 7 | API stability | 0.x (0.12.1). NVML itself is documented by NVIDIA as backwards-compatible across versions. |
| 8 | Footprint | Small — `nvml-wrapper-sys`, `bitflags`, `thiserror`, `static_assertions`, `wrapcenum-derive`. Crate size 104 KB, ~8k lines of Rust. **Exact tree not verified.** |
| 9 | Platform | Linux and Windows, wherever the NVIDIA driver is installed. Useless (and correctly inert) on machines without it. |
| 10 | MSRV | 1.60.0 — comfortably below anything else here. |

**Recommendation:** decide this one last, and only if you actually have an NVIDIA
GPU to monitor. If you don't, option (c) is free and keeps a proprietary blob out of
the process. Tell me which vendor's GPU is in the target machine and that settles it.

**Decision (2026-08-12): declined — no NVIDIA GPU on the target machine.** Option
(c): NVIDIA stays unsupported. AMD/Intel GPU support (chunk 10) is unaffected.
