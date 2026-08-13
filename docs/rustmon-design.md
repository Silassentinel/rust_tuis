# rustmon — design

Hardware monitor for Linux. Sub-project of `rust_tuis`, sibling of `rustlogger/`.

Status: **design + boilerplate only.** No function bodies are implemented yet, and
no Rust toolchain was available in the environment this was written in — see
"Verification status" at the bottom before trusting any of the code in
`rustmon/src/`.

## What it does

Reads hardware and system state from the Linux kernel's own interfaces
(`/proc`, `/sys`) and presents it two ways:

- **`rustmon`** — a live-refreshing terminal UI with per-metric panels.
- **`rustmon --once --format json|text`** — a single snapshot to stdout, for
  scripting, for `watch`, and for the same MCP-server pattern `rustlogger`
  already uses.

Both are thin frontends over one library (`rustmon::lib`) that does all the
collection. The library itself has **zero external dependencies and no UI code**,
so it can be unit-tested against fixture directories without a terminal.

## Scope for v1 (agreed 2026-08-01)

| Area | In v1 | Source of truth |
|---|---|---|
| CPU | per-core + aggregate utilisation, per-core frequency, load average, core count | `/proc/stat`, `/proc/loadavg`, `/sys/devices/system/cpu/cpu*/cpufreq/scaling_cur_freq` |
| Memory | total/used/free/available, buffers, cached, swap total/used | `/proc/meminfo` |
| Thermal | per-sensor temperature, per-chip labels, critical/max trip points, throttle state | `/sys/class/hwmon/hwmon*/temp*_{input,label,crit,max}`, `/sys/class/thermal/thermal_zone*` |
| Fans | RPM per fan, per-chip labels | `/sys/class/hwmon/hwmon*/fan*_{input,label}` |
| Disk | per-device read/write throughput + IOPS (rate, from counter deltas), per-mount capacity/used | `/proc/diskstats`, `/proc/self/mounts` + `statvfs` |
| Network | per-interface RX/TX throughput + packet rate, link state, MTU | `/proc/net/dev`, `/sys/class/net/*/{operstate,mtu}` |
| GPU | utilisation, VRAM used/total, temperature, power draw, fan | AMD: `/sys/class/drm/card*/device/*`; Intel: `/sys/class/drm/card*/`; NVIDIA: **NVML, blocked on a crate decision** |

### Explicit non-goals for v1

- Windows and macOS. The collector layer is trait-based so a second backend can
  be added later per the repo's `cross-platform-rust-cfg-split` pattern, but v1
  is Linux-only and does not pretend otherwise.
- Historical persistence / time-series database. In-memory ring buffer only,
  sized by config; nothing is written to disk.
- Alerting, notifications, remote/agent mode, Prometheus endpoint.
- Any form of control (setting fan curves, governors, power limits). rustmon is
  **read-only by design** — see the security model.
- Process-level accounting (a `top` clone). Hardware, not processes.

## Architecture

```
                     ┌───────────────────────────────┐
   /proc, /sys ─────▶│ sysfs::SysfsReader            │  path validation, size caps,
   (read-only)       │ (the ONLY filesystem entry)   │  fail-soft on EACCES
                     └───────────────┬───────────────┘
                                     │
                     ┌───────────────▼───────────────┐
                     │ collectors::{cpu, memory,     │  one Collector impl each,
                     │  thermal, disk, net, gpu}     │  each independently skippable
                     └───────────────┬───────────────┘
                                     │ raw counters + gauges
                     ┌───────────────▼───────────────┐
                     │ sample::Snapshot              │  plain data, no I/O, Clone
                     └───────────────┬───────────────┘
                                     │
                     ┌───────────────▼───────────────┐
                     │ delta::RateTracker            │  counter → per-second rate
                     │ (needs two snapshots)         │  handles counter wrap/reset
                     └───────┬───────────────┬───────┘
                             │               │
              ┌──────────────▼───┐   ┌───────▼──────────────┐
              │ render::{json,   │   │ ui:: (feature "tui") │
              │  text} — one-shot│   │  live TUI frontend   │
              └──────────────────┘   └──────────────────────┘
```

Two things this shape buys us:

1. **Every collector is optional and fail-soft.** `Snapshot`'s fields are
   `Option<...>`. A machine with no `hwmon` sensors, no GPU, or a container with
   `/sys` partially masked still produces a useful snapshot instead of an error.
   A collector that fails records the failure in `Snapshot::errors` and the rest
   carry on.
2. **Rates are derived, never collected.** `/proc/diskstats` and `/proc/net/dev`
   give monotonic counters, not rates. Turning those into "MB/s" needs two
   snapshots and the wall time between them. Keeping that in `delta.rs` — out of
   the collectors — means the collectors stay pure "read and parse" and are
   testable against a static fixture directory with no timing involved.

### Why `Option` per area rather than `Result` per area

`Snapshot { cpu: Option<CpuSample>, ... }` plus a separate `errors: Vec<CollectorError>`
rather than `cpu: Result<CpuSample, E>`. The consumer almost always wants "show
what you've got"; the error list is for the `--verbose`/diagnostic path. This is
the `Option`/`Result` distinction from the Rust Book Ch. 6 (`Option` for absence
that is normal, `Result` for failure that needs handling) applied at the struct
level.

### Feature gating

The TUI's dependencies must not be forced on library consumers or on the
one-shot JSON path:

```toml
[features]
default = ["tui"]
tui = []              # will gain ratatui/crossterm once approved
gpu-nvidia = []       # will gain nvml-wrapper once approved
```

`src/ui/` is `#[cfg(feature = "tui")]`. `cargo build --no-default-features`
must produce a working `--once` binary with zero external crates. That is a hard
constraint, not a nice-to-have — it's what keeps the crate-checklist rule
meaningful.

## Security model

Standing project rule is to approach every step from a security perspective.
For a tool that reads system state, the realistic threats are: reading something
it shouldn't, being tricked into reading somewhere it shouldn't, hanging or
exhausting memory on hostile input, and emitting output that injects into
whatever consumes it.

1. **Read-only, always.** `SysfsReader` exposes no write path at all. No
   `OpenOptions::write`, no `create`, anywhere in the crate. Writing to
   `/sys/class/hwmon/*/pwm1` can physically damage hardware by stopping fans;
   the API simply doesn't offer it.
2. **No privilege escalation, no root requirement.** rustmon must be useful as
   an unprivileged user. Anything unreadable (`EACCES`) is reported as *absent*,
   not fatal. It must never be installed setuid and must never re-exec itself
   under `sudo`. Some sensors genuinely need root — those are shown as
   unavailable with a reason, and that's the correct behaviour.
3. **No subprocesses at all.** No `std::process::Command`, no shelling out to
   `lspci`/`nvidia-smi`/`lsblk`. This removes command-injection and PATH-hijack
   as a category rather than mitigating it. Everything comes from files.
4. **Path confinement.** Device identifiers (`sda`, `hwmon3`, `eth0`, `card0`)
   come from directory listings, but they are still untrusted input — a
   container mount or a crafted `--sysfs-root` could contain anything.
   `sysfs::is_safe_component` rejects any name containing `/`, `..`, a NUL, a
   leading `.`, or characters outside `[A-Za-z0-9_.:-]`, and `SysfsReader::resolve`
   rejects absolute paths and verifies the canonicalised result is still under
   the configured root. Symlink escape is the specific thing being defended
   against; `/sys` is full of symlinks.
5. **Bounded reads.** Every read goes through a size cap (`max_read_bytes`,
   default 1 MiB) using `Read::take`, never a bare `fs::read_to_string`. Most
   `/sys` files report size 0 while returning data, so trusting metadata to size
   a buffer is wrong; and a misconfigured root pointing at, say, `/proc/kcore`
   would otherwise try to read all of physical memory. Parse loops are bounded
   by line count too (`max_lines`).
6. **No panics on malformed input.** Every parse is a `Result`. No `unwrap`,
   `expect`, or slicing by index on parsed content anywhere in the collectors.
   A truncated `/proc/stat` line must yield a parse error, not an abort. Integer
   handling uses `checked_*`/`saturating_*` — counter deltas in particular must
   handle a counter that went *backwards* (device reset, 32-bit wrap on
   `/proc/net/dev`) by dropping the interval, not by underflowing.
7. **Output escaping.** The hand-rolled JSON writer escapes `"`, `\`, and all
   control characters `< 0x20` per RFC 8259. Sensor labels and interface names
   come from the kernel but are attacker-influenced in some setups (a USB device
   can supply its own name string). The TUI likewise must strip control bytes
   and ANSI escape sequences from any kernel-supplied string before drawing it,
   or a crafted device name could rewrite the terminal.
8. **Self-inflicted DoS limits.** The refresh interval is clamped to a floor
   (default 100 ms) so a `--interval 0` can't spin the CPU it's meant to be
   measuring; the history ring buffer is fixed-capacity; the number of
   enumerated devices per class is capped.
9. **Information disclosure.** `/proc` and `/sys` do carry identifying
   information (disk serials, MAC addresses, machine IDs). rustmon collects none
   of it, and the JSON output is a documented fixed schema — nothing is
   included that wasn't explicitly listed in the scope table above. Worth
   restating because JSON output is the thing people pipe into other systems.

   **This deliberately does not extend to the `connections` collector
   (chunk 14).** A per-process list of who's talking to which remote IP is
   genuinely more sensitive than every other collector in this crate
   combined — none of the hardware telemetry above reveals what someone is
   *doing*, and this does. It's still purely local and read-only (no new
   network I/O, no new privilege — everything comes from `/proc`, same as
   every other collector), but it's a deliberate, user-requested expansion
   of what rustmon exposes, not a natural continuation of "read hardware
   sensors." Two mitigations, both already true of the rest of the crate
   and worth restating here specifically: scope is filtered to
   internet-facing connections only (`is_public_ip` excludes LAN/loopback
   traffic — see `collectors::connections`'s module doc), and attribution
   degrades to "owner uid only" rather than erroring when the owning
   process can't be read (a different user's process, `EACCES`), the same
   fail-soft rule as everything else — it never guesses.

An explicit note on NVIDIA: NVML is a `dlopen` of a proprietary shared library
into rustmon's own address space. That is a materially different trust decision
from reading a text file, and it is why NVIDIA support is a separate,
independently-gated feature (`gpu-nvidia`, off by default) rather than part of
the GPU collector — see the crate proposal.

## Rust Book references

Chapters that shaped specific decisions, per the repo's standing rule:

- **Ch. 6 (`Option`/`Result`)** — the `Option` fields + `errors` vec on
  `Snapshot`, discussed above.
- **Ch. 9 (error handling)** — one crate-level `Error` enum in `error.rs`
  carrying the offending path, with `From<io::Error>`; `?` throughout, no
  `unwrap` in library code.
- **Ch. 10 (traits + generics)** — the `Collector` trait, so `Registry` can hold
  a heterogeneous set and so tests can substitute a fixture-backed collector.
- **Ch. 12 (`minigrep`: lib + thin bin)** — same split `rustlogger` already
  uses: `lib.rs` holds everything, `main.rs` only parses args and calls in.
- **Ch. 13 (iterators)** — parsers are iterator chains over `lines()`; keeps the
  "no index slicing" security rule easy to hold to.
- **Ch. 17 (trait objects)** — `Registry` stores `Box<dyn Collector>`; the set of
  collectors is a runtime decision (config + what the machine actually has), so
  static dispatch doesn't fit.
- **Ch. 19 (newtypes)** — `units.rs` wraps raw integers in `Bytes`, `KiloHertz`,
  `MilliCelsius`. Mixing up KiB and bytes when reading `/proc/meminfo` (which
  reports kB) versus `/proc/diskstats` (which reports 512-byte sectors) is the
  single most likely correctness bug in this whole crate; the type system should
  catch it.

## Verification status

Last verified **2026-08-11** on rustc 1.94.1.

| | Status |
|---|---|
| `cargo check -p rustmon` | passes, 0 errors |
| `cargo clippy -p rustmon --all-targets` (both feature configs) | no lints outside the unused-variable warnings `ui/` and NVIDIA's `todo!()` bodies necessarily produce |
| `cargo test -p rustmon` | 233 unit tests + 8 integration tests, green |
| `cargo test -p rustmon --no-default-features` | green — the zero-external-dependency build is real, not aspirational |
| `cargo doc -p rustmon --no-deps` | no warnings |
| `rustmon --once` / `--once --format json` / `--help` / `--version` | run for real, output verified against this machine's actual hardware (JSON piped through Python's `json.load` to confirm validity) |
| `rustmon/tests/integration.rs` | 8 tests running the compiled binary via `std::process::Command`, including a full snapshot-and-diff proving a `--once` run leaves its fixture root byte-for-byte unchanged |

The skeleton's signatures compiled as written, so the original handoff warning
(no Rust toolchain in the authoring environment, nothing verified) is resolved.

**Every chunk in the build plan is now either done or explicitly declined.**
Chunks 1–9 and 11, plus the AMD/Intel half of chunk 10, are complete —
`docs/TODO-rustmon.md` has the full account chunk by chunk. **`rustmon --once`
is a real, complete binary and `rustmon` (no flags) is a real, working live
TUI**, the output schema is documented in `rustmon/README.md`, and the binary
itself (not just its library functions) has automated end-to-end coverage.
Only the NVIDIA half of chunk 10 remains unimplemented, and that's by
decision, not by blocker:

| Chunk | Needs | Status |
|---|---|---|
| 8 — mount capacity | `nix` (`fs` feature) | **approved and implemented, 2026-08-12** |
| 9 — live TUI | `ratatui`, `crossterm` | **approved and implemented, 2026-08-12** |
| 10 — NVIDIA GPU | `nvml-wrapper` | **declined** — no NVIDIA hardware to target |

Nothing about the design changed when chunk 8 or chunk 9 landed — the
collector trait, the `Snapshot` model, and the render/delta layers were all
built to accommodate them (`Gpu::freq_khz` in chunk 10, the
`fs-capacity`/`tui`/`gpu-nvidia` feature flags from chunk 0) without a
rewrite. Chunk 8 is one proof: `read_capacity` slotted into the `todo!()`
`collectors/disk.rs` had already reserved for it, `MountPoint`'s
`total`/`available` fields existed since chunk 6, and `render/json.rs`
already emitted them whenever `Some` — none of that needed to change. Chunk 9
is a second, larger one: `ui::run` was already wired into `lib.rs::run`'s
dispatch since chunk 7, `Rect`/`Layout`/`PanelPresence`/`Panel`/`KeyPress`
were already backend-independent types in the chunk-0 skeleton specifically
so the `ratatui`/`crossterm` types would only ever need to touch
`ui/mod.rs`/`ui/widgets.rs`, and that boundary held — `ui/app.rs` and
`ui/layout.rs` compile and are fully tested with zero `ratatui`/`crossterm`
imports.

**The thermal, disk, and net collectors have each been read against this
machine's real state**, not only fixtures — every value cross-checked against
`cat` on the underlying `/proc` and `/sys` files. Thermal surfaced a real
oddity: one NVMe sensor genuinely reports a `_max` of 65,261.8°C (a
kernel-side sentinel, not a parsing bug — see chunk 5's TODO entry). This
crate reports what the kernel says rather than second-guessing it, the same
principle as the CPU collector's double-counted `guest` jiffies in chunk 3.

Chunk 6's real-hardware pass caught something different and more serious: a
genuine data-corruption bug in `unescape_mount_field`, found while
implementing, not in review. The first draft indexed the mount-string bytes
directly and cast non-escape bytes to `char` — silently wrong for any
multi-byte UTF-8 character in a mount label. Fixed before it ever reached a
test run; see chunk 6's TODO entry for the fix and the regression test. It's
recorded here because it's a class of bug worth watching for elsewhere in
this crate: anywhere a string is walked by byte index rather than by
`chars()`, ask explicitly whether a non-ASCII byte can reach that code.

**Chunk 7 found a second real bug, this time a missed security boundary
rather than a corruption bug**: `parse_mounts` never sanitised its
`source`/`mount_point`/`fs_type` fields. Every other kernel-supplied string in
this crate is sanitised at the collector boundary specifically so no renderer
has to remember to, but mount fields aren't charset-restricted by
`is_safe_component` the way device names are, and `unescape_mount_field` only
decodes four documented octal escapes — it was never in the business of
stripping control characters. A hostile mount source could have carried a raw
ANSI escape straight into JSON output. Fixed in `disk.rs`, where every other
sanitisation call in the crate lives, with a regression test. Two real bugs
found by *implementing the next chunk against the previous one's output*, not
by a dedicated review pass — worth remembering as evidence for why each
chunk's docs get updated immediately rather than batched up.

**Read against real hardware on 2026-08-04**, not only fixtures: the CPU and
memory collectors were driven against `/` and cross-checked against `free -h`,
`nproc`, and `uptime -s`, all matching. That is the first evidence the
`/proc`-parsing half of the design is correct rather than merely plausible.

### Security-model items verified so far

Items 1, 3, 4, 5, and 7 have tests behind them as of chunk 2. Two caveats on
item 4 that the `sysfs` module documents in full and that are **not** defects
but scope decisions, restated here so they aren't rediscovered later:

- The containment check is **vacuous at the production root of `/`** — every
  canonical path starts with `/`. Production is protected by the component
  allowlist and the `..`/absolute rejection; containment is what makes a
  fixture tree or a user-supplied `--sysfs-root` safe.
- **Resolve-then-open leaves a TOCTOU window.** Closing it needs
  `openat2(RESOLVE_BENEATH)`, unreachable from `std` and a `libc`/`nix`
  dependency otherwise. Accepted deliberately: `/proc` and `/sys` are
  root-owned and not attacker-writable.

Items 2, 6, 8, and 9 are structural and are re-checked as each chunk lands.
The claims behind items 1 and 3 are greppable — no `std::fs` outside
`sysfs.rs`, no write path outside its test fixtures, no `process::Command`
anywhere — and should become a CI check in chunk 11.
