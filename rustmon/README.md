# rustmon

A read-only hardware monitor for Linux. Part of the [`rust_tuis`](../README.md)
collection.

> **Status: the live TUI, `--once`, and `--summary` all work and are fully
> tested.** 331 unit tests + 8 end-to-end tests against the compiled binary,
> all green. `rustmon`, `rustmon --once`, `rustmon --once --format json`, and
> `rustmon --summary` are all real, working commands — run for real against
> this machine and verified, not just unit-tested: the JSON was piped through
> Python's `json.load` to confirm it's genuinely valid, then checked field by
> field against real hardware; the live TUI was driven interactively through
> a real pty; `--summary` was timed at ~7–23ms across dozens of runs (well
> under its 100ms budget) with both its silent and JSON-emitting paths
> exercised. Every collector except NVIDIA GPU support has been verified
> against real hardware, not just fixtures — CPU/memory against `free
> -h`/`nproc`/`uptime -s`; thermal against 7 real hwmon chips; disk/net
> against this machine's actual `/proc/diskstats`, `/proc/self/mounts`, and
> `/proc/net/dev`; mount capacity against `df -B1` (byte-exact on every
> mount); AMD GPU against two real cards; **connections against `ss -tp`
> (every attributable process/port/remote-endpoint match exact, including
> fd-level detail, and a real loopback connection correctly excluded)**.
> NVIDIA GPU support was declined (chunk 10's remainder, `nvml-wrapper`) — no
> NVIDIA hardware on the target machine; AMD/Intel GPU support is
> unaffected. The original 11-chunk build plan is complete; a second phase
> (man page, `--summary`, connection visibility with opt-in DNS/traceroute)
> is in progress — see `docs/TODO-rustmon.md` chunks 12 onward.

## What it does

Reads CPU, memory, temperature, fan, disk, network and (AMD/Intel) GPU state
from `/proc` and `/sys`, and presents it two ways:

```
rustmon                          # live TUI — works
rustmon --once                   # one snapshot as aligned text — works
rustmon --once --format json     # one snapshot as JSON, for scripts — works
rustmon --summary                # shell-prompt one-liner — works
```

### The live TUI

Six panels (CPU, memory, thermal, disk, net, GPU) laid out responsively —
single column under 60 columns, two columns from 60–119, three from 120 up,
each panel dropped before it's ever shown squashed to nothing. Panels for
hardware the machine doesn't have (no GPU, no sensors) are omitted entirely,
not shown empty.

| Key | Does |
|---|---|
| `q`, `Esc`, `Ctrl-C` | Quit |
| `Tab` / `Shift-Tab` | Cycle panel focus |
| `1`–`6` | Jump to a panel |
| `space` | Pause/resume |
| `r` | Reset the rate tracker (drops the next interval rather than averaging across the gap) |
| `?` | Toggle the help overlay |
| `+` / `-` | Adjust the refresh interval (floor: 100 ms) |

### Shell-prompt summary mode

`rustmon --summary` is built for something like an [oh-my-posh][ohmyposh]
command segment — run before every prompt, so it stays well under 100 ms
(measured on this machine: ~7–23 ms per run). It deliberately collects only
`cpu`, `memory`, and `thermal` — the only three collectors in this crate
with zero exposure to a slow read (no `statvfs`, no `/proc/<pid>` walk) —
regardless of any `--only`/`--skip` also given.

If there's nothing worth surfacing (no sensor at warning/critical severity,
no collector error), it prints **nothing** and exits `1`. Otherwise it
prints one compact JSON object and exits `0`:

```jsonc
{"status": "warning", "headline": "1 warning"}
```

`status` is `ok` (never actually printed — see above), `warning`, or
`critical`. A shell-prompt integration that hides its segment on a nonzero
exit code or empty output — oh-my-posh's command segment does this natively
— needs no other configuration to appear only when there's something to
say.

[ohmyposh]: https://ohmyposh.dev/

## Usage

```sh
rustmon --once                                # aligned text to stdout, then exit
rustmon --once --format json                  # the schema below, to stdout
rustmon --once --only cpu,memory               # just these collectors
rustmon --once --skip gpu                      # everything except these
rustmon --once --sysfs-root /path/to/fixture   # read from somewhere other than /
rustmon --once --verbose                       # include which collectors failed, and why
rustmon --summary                              # shell-prompt one-liner, then exit
rustmon --help                                 # every flag, documented
```

Full reference, including every flag and TUI keybinding: `man ./man/rustmon.1`
(or install it into your `MANPATH` to just run `man rustmon`).

`--only`/`--skip` names must be one of `cpu`, `memory`, `thermal`, `disk`,
`net`, `gpu`, `connections` — a typo (`--only cpuu`) is a hard error, not a
silently empty result. `--interval` and `--history` only matter for the live
TUI; they're accepted and validated by `--once` too, but have nothing to
affect since a single snapshot has no refresh loop.

There is no live-refreshing `--once` mode by design: taking two snapshots an
interval apart to synthesise a rate would make `--once` block, which is
exactly what a script invoking it doesn't expect. A single snapshot reports
counters and gauges (CPU model, memory used, temperatures, mount points) but
not throughput or CPU busy% — those need two readings and only exist in the
live TUI.

## JSON output schema (`--format json`)

One JSON object per invocation, newline-terminated. This is `schema: 1` and a
stability commitment for `--once --format json` specifically: a field's
*meaning* won't change without bumping `schema`, and a script can rely on
that. Adding a new field does not bump it — check for fields you know about,
don't assume the object's key set is closed.

Every section below is present only if that hardware/data exists on the
machine — a VM with no GPU has no `"gpu"` key at all, not an empty one.
`--once` (no rates) never includes throughput or busy% fields; those exist
only in the live TUI's on-screen display (gauges, sparklines, per-second
figures) — the TUI has no JSON export of its own, and this schema stays a
`--once --format json`-only contract.

```jsonc
{
  "schema": 1,
  "taken_at": "2026-08-11T06:08:46Z",       // RFC 3339 UTC

  "cpu": {
    "model": "AMD Ryzen 9 9900X 12-Core Processor",  // absent on some ARM/VM kernels
    "cores": 24,
    "load_avg": [0.19, 0.34, 0.35],          // 1 / 5 / 15 minute
    "per_core": [
      { "core": 0, "freq_khz": 4395724 }     // freq_khz absent with no cpufreq driver
    ]
  },

  "memory": {
    "total_bytes": 64829665280,
    "available_bytes": 57634226176,          // MemAvailable, not MemFree — see docs/rustmon-design.md
    "used_bytes": 7195439104,                // total - available
    "swap_total_bytes": 8589930496,
    "swap_used_bytes": 897024
  },

  "thermal": [                               // one entry per hwmon chip (or one
    {                                        // synthetic "thermal_zone" chip as
      "name": "k10temp",                     // a fallback with no hwmon at all)
      "temps": [
        {
          "label": "Tctl",
          "celsius": 44.625,
          "max_celsius": 95.0,               // max/crit absent if the chip doesn't report them
          "crit_celsius": 100.0
        }
      ],
      "fans": [
        { "label": "fan1", "rpm": 1200 }
      ]
    }
  ],

  "disk": [
    { "name": "sda" }                        // no rate fields on --once — see "Usage" above.
  ],                                          // Loop/dm-/ram/zram devices and
                                               // partitions of a listed disk are filtered out.
  "mounts": [
    {
      "source": "/dev/mapper/ubuntu--vg-ubuntu--lv",
      "mount_point": "/",
      "fs_type": "ext4",
      "total_bytes": 1964601909248,
      "available_bytes": 297806995456
      // total_bytes/available_bytes come from statvfs(3) (the fs-capacity
      // feature, in `default`) and are absent — not null — if that syscall
      // fails for this one mount (permission denied, a stale network mount
      // timing out, or --no-default-features): capacity is decoration on top
      // of the mount listing, so losing it for one mount doesn't cost you
      // the rest. Pseudo-filesystems (proc, sysfs, tmpfs, overlay, ...) are
      // filtered out of this list entirely, not shown with null capacity.
    }
  ],

  "net": [                                   // "lo" is hidden by default
    {
      "name": "enp13s0",
      "operstate": "up",                     // absent if /sys is masked
      "mtu": 1500
    }
  ],

  "gpu": [                                   // AMD and Intel only — NVIDIA
    {                                        // support was declined (no
      "name": "card1",                       // NVIDIA hardware to target)
      "vendor": "amd",                       // "amd" | "intel" | "nvidia" | "unknown"
      "busy_percent": 9.0,                   // Intel: always absent, no perf-counter access
      "freq_khz": 1450000,                   // Intel only today; AMD's clock isn't wired up
      "vram_total_bytes": 17095983104,
      "vram_used_bytes": 1811267584,
      "celsius": 26.0,
      "watts": 9.0,
      "fan_rpm": 0
    }
  ],

  "connections": [                           // internet-facing only — no LAN,
    {                                        // loopback, or listening sockets
      "protocol": "tcp",                     // "tcp" | "udp"
      "local_addr": "10.158.92.132",
      "local_port": 50206,
      "remote_addr": "140.82.121.6",
      "remote_port": 443,
      "state": "established",                // TCP only — absent for udp
      "uid": 1000,                           // always present, even when pid/program aren't
      "pid": 10791,                          // absent if the owning process couldn't be
      "program": "claude-desktop"            // read (a different user's process — EACCES)
    }
  ],

  "errors": [                                // only with --verbose
    { "collector": "gpu", "message": "..." } // which collector failed, and why
  ]
}
```

Every string in this output — chip names, sensor labels, device names, mount
paths — is escaped per RFC 8259 §7 and sanitised of control characters and
ANSI escape sequences before it reaches JSON, because several of these
strings originate in device firmware or userspace daemons (a USB device's
self-reported name, a FUSE filesystem's chosen type string) and this output
gets piped into other programs. `NaN`/`Infinity` (which have no JSON
representation) are emitted as `null`.

## What needs root, and why

**Nothing does, by design** — but some individual sensors do, depending on the
kernel and driver:

- A handful of hwmon files (some power/voltage sensors on server-class boards)
  are root-only in some kernel configurations. Under an unprivileged user,
  that specific sensor is absent — reported as missing, not as an error, and
  every *other* sensor on the same chip is unaffected. `--verbose` shows which
  collector had trouble and why.
- `/proc/net/dev` and `/proc/diskstats` are world-readable on every distro
  this crate targets; nothing there needs root.
- `statvfs(3)` (mount capacity) needs no elevated privilege either — it's
  the same syscall `df` uses as an unprivileged user.
- `proc/net/{tcp,tcp6,udp,udp6}` (the connection list itself) is also
  world-readable, but attributing a connection to a specific `pid`/`program`
  needs the same access `ps`/`ss -p` do: reading your *own* processes'
  `/proc/<pid>/fd` works unprivileged, another user's doesn't (`EACCES`).
  That connection still shows up, just with `pid`/`program` absent and only
  its owning `uid` known — never as an error, and never a reason to suggest
  running rustmon as root.

rustmon never re-execs itself under `sudo`, is never meant to be installed
setuid, and there is no code path that requests elevated privileges — running
it as root would only make the handful of already-rare root-only sensors
readable, at the cost of trusting a bigger attack surface with root. Running
unprivileged and accepting a few blank fields is the recommended way to run
it.

## Design guarantees

These are structural, not aspirational — the API is shaped so they're hard to
violate:

- **Read-only.** No write path exists anywhere in the crate. Writing to
  `/sys/class/hwmon/*/pwm1` can stop a fan and damage hardware; rustmon offers
  no way to do it.
- **No subprocesses.** No `lspci`, no `nvidia-smi`, no shelling out at all. This
  removes command injection and PATH hijacking as a category rather than
  mitigating them.
- **No root required.** Anything unreadable is reported as absent with a reason.
  rustmon must never be installed setuid and never re-execs itself under `sudo`.
- **Bounded.** Every file read is size-capped, every directory listing is capped,
  the history buffer is fixed-capacity, and the refresh interval has a floor.
- **One filesystem entry point.** All reads go through `sysfs::SysfsReader`,
  which does path confinement and size capping. Nothing else in the crate calls
  `std::fs`.

Full reasoning: `docs/rustmon-design.md`.

## Dependencies

**Three, all opt-out.** `nix` (`fs` feature, for `statvfs`) and `ratatui` +
`crossterm` (the live TUI) are all in `default` — a plain `cargo build`/
`cargo run` pulls in all three and gives you the full tool, live TUI
included. `cargo build --no-default-features` stays genuinely
dependency-free; `--once --format json` still works, just without per-mount
capacity numbers, and bare `rustmon` (no `--once`) returns
`Error::Unsupported` instead of a UI that was never compiled in.

Three crate proposals live in `docs/crate-checklist.md`; two are approved:

| Feature | Needs | Gives you | Status |
|---|---|---|---|
| `fs-capacity` | `nix` (`fs`) | per-mount disk capacity via `statvfs` | **approved, in `default`** |
| `tui` | `ratatui`, `crossterm` | the live terminal UI | **approved and implemented, in `default`** |
| `gpu-nvidia` | `nvml-wrapper` | NVIDIA GPU metrics | declined — no NVIDIA hardware to target |

```sh
cargo build -p rustmon --no-default-features   # zero external crates
```

## Build plan

`docs/TODO-rustmon.md`. One chunk at a time; each needs passing tests and updated
docs before the next starts.
