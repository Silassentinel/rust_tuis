# rustmon — build TODO

One chunk at a time. Don't start the next box until the current one has tests
passing and docs updated (see `docs/rustmon-design.md`).

Chunks 1–7 need **zero external crates** and are not blocked on anything.
Chunks 8–10 each have a crate-checklist blocker that must be signed off first.

---

- [x] **0. Scaffold + plan.** Design doc (`docs/rustmon-design.md`), this file,
      crate proposals appended to `docs/crate-checklist.md`, and the full
      `rustmon/` module skeleton: every type, every function signature, every
      doc comment, all bodies `todo!()`. Workspace `Cargo.toml` and `INDEX.md`
      updated.

      **Verified 2026-08-04:** the skeleton does compile — `cargo check -p
      rustmon` passes with 0 errors on rustc 1.94.1, and `cargo clippy` reports
      no lints beyond the unused-variable warnings that the `todo!()` bodies
      necessarily produce. The chunk-0 handoff warning is resolved.

- [x] **1. Compile the skeleton + `error.rs` / `units.rs`.** *(done 2026-08-04)*
      Get `cargo check -p rustmon` and `cargo clippy -p rustmon` clean.
      Then implement the two leaf modules that everything else depends on:
      - `error.rs`: `Error` enum (`Io { path, source }`, `Parse { path, line, reason }`,
        `Unsupported`, `PathRejected`), `Display`, `std::error::Error`,
        `From<io::Error>`, and the crate `Result` alias.
      - `units.rs`: `Bytes`, `KiloHertz`, `MilliCelsius`, `Percent`, `Rpm`,
        `BytesPerSec`, `Watts` newtypes + their conversions.
      Tests: unit conversions round-trip; `MilliCelsius → Celsius` rounding;
      `Bytes::human()` boundaries (1023 B, 1024 B, 1 MiB − 1); `Percent`
      clamping. No I/O in this chunk at all.

      **21 tests, green.** Three deviations from the plan above, each
      deliberate:

      1. **No `From<io::Error> for Error`.** It contradicts the module's own
         rule that every variant carries the offending path — an `io::Error`
         alone has none, so the impl would need a placeholder path or a
         path-less variant, and either one makes the "always know which file"
         property unenforceable. `Error::classify_io(path, source)` is the
         replacement and is the only way an `io::Error` enters the crate.
      2. **`classify_io` also maps `EIO`/`ENXIO`/`ENODEV`/`ENODATA`/
         `EOPNOTSUPP` to `Absence::NotReported`** (Linux errno values, behind
         a `target_os` gate). hwmon returns these routinely for a probe that
         isn't physically connected or a device that's been unbound; leaving
         them as hard errors would make chunk 5 fail on ordinary hardware.
         This lives here rather than in the thermal collector to keep
         `classify_io`'s stated property — *the* single place errno gets its
         meaning — actually true.
      3. **`Bytes::human()` carries `PiB`/`EiB` past the planned `TiB`**, and
         promotes a value that rounds up onto the next threshold, so
         1 MiB − 1 prints `1.0 MiB` rather than `1024.0 KiB`.

      **Carried into chunk 2:** `Error::parse`'s `reason` is often built from
      file contents, which are kernel-supplied and attacker-influenced in some
      setups (a USB device names itself). It must be passed through
      `sysfs::sanitize_kernel_string` at the call sites once that function
      exists — it is `todo!()` today. Same for `Error::Display` printing a
      user-supplied `--sysfs-root` verbatim.

- [x] **2. `sysfs.rs` — the filesystem boundary.** *(done 2026-08-04)* This is
      the security-critical module; it gets written and tested *before*
      anything that uses it.
      **Do the `sanitize_kernel_string` wiring noted in chunk 1 as part of
      this chunk**, not as a follow-up.
      - `is_safe_component` rejects `/`, `..`, `.`-prefixed, NUL, and anything
        outside `[A-Za-z0-9_.:-]`.
      - `SysfsReader::resolve` rejects absolute paths, canonicalises, and
        verifies the result is still under `root`.
      - All reads bounded by `max_read_bytes` via `Read::take`.
      - `EACCES`/`ENOENT` map to a distinguishable "absent" result, not an error.
      Tests: build a fixture tree under `tempdir`-equivalent (hand-rolled, no
      crate), including a **symlink pointing outside the root**, and assert it's
      rejected; assert a 2 MiB file is truncated at the cap; assert traversal
      attempts (`../../etc/passwd`, `a/../../b`) are rejected; assert an
      unreadable file yields absent, not a panic.

      **21 tests, green** (42 in the crate). Every listed test exists, plus
      the symlink-*inside*-the-root counterpart — without it a passing escape
      test would also pass if the module simply refused all symlinks, which
      would read nothing on a real machine, since `/sys/class/hwmon/hwmon0` is
      itself a link into `/sys/devices/`.

      Deviations from the plan, each deliberate:

      1. **`resolve` returns `Result<Result<PathBuf, Absence>>`, not
         `Result<PathBuf>`.** `canonicalize` requires the path to exist, so
         `ENOENT`/`EACCES` surface *inside* `resolve` before any caller gets a
         chance to classify them. With the original signature every missing
         file would have become a hard error and the fail-soft rule would have
         been broken at the root of the crate. A hard `Err` from `resolve` now
         means *refused*, which is a louder and rarer thing than absent.
      2. **`read_first_line` maps an empty or whitespace-only file to
         `Absence::NotReported`.** That is the case the variant was defined
         for, and it saves every caller from re-checking for `""`.
      3. **`list_dir` caps entries *examined*, not entries kept**, so a
         directory with a million entries that all fail `keep` still costs a
         bounded amount of work.
      4. Added `with_max_entries` plus `max_read_bytes()` / `max_lines()` /
         `max_entries()` accessors. `max_lines` is enforced by the collectors
         from chunk 3 on — this module hands back whole file contents and has
         no opinion on their shape.

      **Two limitations written into the module docs rather than left to be
      discovered.** Neither is a defect; both are scope decisions:

      - With the production root of `/`, the containment check is **vacuous** —
        every canonical path starts with `/`. What protects production is the
        component allowlist plus the `..`/absolute rejection. Containment earns
        its place for fixture trees and `--sysfs-root`.
      - **Resolve-then-open is a TOCTOU window.** Closing it needs
        `openat2(RESOLVE_BENEATH)`, which `std` doesn't expose and which would
        mean a `libc`/`nix` dependency (rule 4). Accepted: `/proc` and `/sys`
        are root-owned, and the `--sysfs-root` threat model is "user points it
        at a fixture tree", not "attacker races us in a world-writable dir".

      **Chunk 1's carry-forward is discharged:** `sysfs::parse_error` runs the
      offending text through `sanitize_kernel_string` before it reaches an
      `Error::Parse`, with a test asserting no ESC survives into the rendered
      message.

      **For chunk 11:** the crate's three auditable claims are greppable and
      currently hold — no `std::fs` outside `sysfs.rs`, no write path outside
      that module's test fixtures, no `process::Command` anywhere. Worth
      wiring into CI as a check rather than re-running by hand.

- [x] **3. `sample.rs` + `collector.rs` + CPU and memory collectors.**
      *(done 2026-08-04)*
      - `Snapshot`, the per-area sample structs, `CollectorError`.
      - `Collector` trait + `Registry::collect_all` (a failing collector is
        recorded and does not abort the others).
      - `collectors::cpu`: `/proc/stat` per-core jiffies, `/proc/loadavg`,
        `cpufreq/scaling_cur_freq`.
      - `collectors::memory`: `/proc/meminfo` (note: reports **kB**, not bytes —
        the `units.rs` newtypes exist to stop that being silently wrong).
      Tests: parse real captured `/proc/stat` and `/proc/meminfo` fixtures
      including a truncated line, a line with a missing field, and a value that
      overflows `u64` — each must be a `Parse` error, never a panic.

      **39 tests, green** (81 in the crate). Fixtures were captured from this
      machine on 2026-08-04, and the `/proc/stat` one keeps a **non-zero
      `guest` column on purpose** — an all-zero fixture there cannot tell a
      correct `CpuTimes::total()` from one that double-counts.

      **Verified against real hardware, not just fixtures.** A throwaway
      smoke test drove both collectors against `/` and every figure
      cross-checked against the system's own tools: memory total/used/available
      matched `free -h`, swap-used matched it exactly (692 KiB), core count
      matched `nproc` (24), and `btime` decoded to precisely `uptime -s`. The
      file was then deleted — chunk 11 owns integration tests, and as written
      it panicked rather than skipped when `/proc` is masked. **Chunk 11 should
      reinstate it as a tolerant test.**

      Decisions worth knowing about:

      1. **`Registry::from_config` is deliberately still `todo!()`.** It needs
         `Config::wants`, which is chunk 7's work along with the rest of
         argument parsing. `from_collectors` is enough to test the fail-soft
         contract, which is what chunk 3 actually promised. **Each later chunk
         registers its own collector in `from_config` as it lands** — thermal
         (5), disk and net (6), gpu (10).
      2. **Not every failure inside a collector is fatal to it.** `/proc/stat`
         is the point of the CPU collector, so a parse failure there
         propagates. Load average, model name, and per-core frequency are
         decorations — losing one must not cost the caller the utilisation
         counters it came for. Those failures get pushed onto
         `Snapshot::errors` (visible under `--verbose`) and collection
         continues. The rule is written into `collectors/cpu.rs`'s module doc.
      3. **Per-core entries are indexed by the number in `cpuN`, never by line
         order**, with gaps from offline cores left as zeroed entries. Packing
         by order would silently reattribute every later core's counters after
         a hot-unplug. A zeroed entry has `total() == 0`, which
         `Percent::from_ratio` already reports as "no reading" rather than as
         0% busy.
      4. **The core index is capped at 4096** before it sizes a `Vec`. It comes
         from a file, and `cpu4000000000 ...` would otherwise ask for a
         four-billion-entry allocation. Security model item 8.
      5. **A meminfo line with no `kB` suffix is skipped, not read as kB.**
         `HugePages_Total` and friends are counts; multiplying one by 1024
         would be silently, plausibly wrong.
      6. **`parse_loadavg` rejects non-finite values.** `"nan"` and `"inf"`
         both parse happily as `f64` and would otherwise reach the renderer.

- [x] **4. `delta.rs` — counters to rates.** *(done 2026-08-05)* `RateTracker`
      holding the previous snapshot; `rate_between(prev, curr, elapsed)`.
      Tests, and these are the ones that matter: a counter that went *backwards*
      (device reset) drops the interval rather than underflowing; a 32-bit wrap
      on `/proc/net/dev` is handled; a zero or negative elapsed time (clock
      stepped backwards — use `Instant`, not `SystemTime`, for the interval)
      yields no rate instead of dividing by zero.

      **17 tests, green** (98 in the crate), all three named scary cases
      covered plus the ones the design implies but doesn't spell out.

      1. **"Handled" means dropped, not corrected — by design, not by
         omission.** `counter_delta` returns `None` on a 32-bit
         `/proc/net/dev` wrap because it's indistinguishable at this layer
         from a NIC reset (`curr < prev` either way), and the module doc is
         explicit that guessing a wrap width is out of scope: a wrong guess
         is a plausible-looking wrong number, and this crate would rather show
         nothing for one refresh. `counter_delta_does_not_correct_a_32_bit_wrap`
         and the `RateTracker`-level `a_wrapped_interface_is_dropped_for_one_refresh_others_are_not`
         both assert *absence*, not a corrected value — worth rereading if
         someone is ever tempted to "fix" this by reconstructing the wrap.
      2. **The drop is per-device, not per-snapshot.** One NIC resetting
         doesn't blank out every other interface's rate that refresh — each
         device in `disk`/`net` is matched by name and evaluated
         independently. Only a non-positive *interval* (bullet 2) blanks the
         whole `Rates`, because that one is a property of the pair of
         snapshots, not of any one device.
      3. **Matching is genuinely by name, not by position** —
         `devices_are_matched_by_name_not_by_position` reorders and inserts a
         device between two snapshots and asserts each old device still gets
         its own counters, not its neighbour's. CPU cores are the one
         exception: their identity *is* their index in `/proc/stat` (chunk 3
         already established that), so per-core matching is by index instead,
         with a core coming online mid-session correctly reading `None`
         rather than inheriting core 0's jiffies.
      4. **`DiskRate::utilisation` can be `None` while the rest of the row is
         populated.** It's computed from a separate counter
         (`io_ticks_ms`) than throughput/IOPS, and some drivers don't advance
         it — losing utilisation must not cost the caller the throughput
         figures it came for. This is the same "don't let one bad field kill
         a good reading" idea as chunk 3's per-collector fail-soft rule,
         applied one level down.
      5. **Sector-to-byte conversion in a rate does not go through
         `Bytes::from_sectors_512`.** That constructor's overflow check is for
         an absolute byte count; a per-second `f64` rate is multiplied by
         512.0 directly. Using the checked constructor here would be the
         wrong tool — deltas this small can't overflow it, but reaching for it
         out of habit would be worth a raised eyebrow in review.

- [x] **5. `collectors::thermal` — temperatures + fans.** *(done 2026-08-05)*
      Enumerate `/sys/class/hwmon/hwmon*`, read `name`, then
      `temp*_input`/`temp*_label`/`temp*_crit`/`temp*_max` and
      `fan*_input`/`fan*_label`. Fall back to `/sys/class/thermal/thermal_zone*`
      where no hwmon chip exists.
      Tests: fixture hwmon tree with a chip that has temps but no labels, a chip
      with fans but no temps, a `temp*_input` containing garbage, and an empty
      `/sys/class/hwmon` (must yield `None`, not an error).

      **19 new tests (117 in the crate), and read against this machine's real
      hwmon tree** (7 chips: an r8169 NIC, an NVMe drive, `k10temp`, two
      identically-named `spd5118` RAM sensors, and two `amdgpu` GPU chips) —
      every value cross-checked by hand against `cat` on the same files.
      That run caught a real, non-hypothetical case none of the written tests
      anticipated: one NVMe sensor reports `temp2_max` as `65261850`
      millidegrees — 65,261.8°C, a kernel-side sentinel value, not a bug in
      this crate. The code doesn't (and shouldn't) second-guess it; it's
      recorded here so nobody "fixes" it later by clamping `_max` without
      checking the machine that produced it.

      **Fail-soft granularity is per-sensor here, not per-collector.** Chunk
      3's collectors treat their one source file as fatal because that file
      *is* the point of the collector; thermal has no single point of truth —
      a machine has independent chips with independent sensors, so a garbled
      `temp3_input` on one chip degrades to "this one sensor is absent"
      rather than failing the whole refresh. This is written into the module
      doc as the chunk's central design decision, since it's the opposite
      default from chunk 3 and worth being explicit about.

      Decisions worth knowing about:

      1. **Resolved a doc conflict between `sample.rs` and `thermal.rs`
         before writing code against either.** `sample.rs` said the label
         fallback is bare `tempN`; `thermal.rs`'s module doc said "chip name
         plus the index". Went with the latter and updated `sample.rs` to
         match — a bare `temp1` is ambiguous once you know this machine has
         *two* separate `spd5118` chips, each with its own `temp1`.
      2. **Indices are found by listing, never by counting from 1.** This
         machine's own `k10temp` chip is the proof it matters: it reports
         `temp1`, `temp3`, `temp4` with no `temp2` at all. A counting loop
         that stopped at the first gap would have silently dropped `Tccd1`
         and `Tccd2` — not a hypothetical, this exact machine.
      3. **`sensor_indices` extracts `N` from `<prefix>N_input` via
         `strip_prefix`/`strip_suffix`, not manual byte slicing** — it can't
         panic regardless of how a pathologically short or overlapping name
         might interact with the prefix/suffix lengths, where manual index
         arithmetic could.
      4. **`ThermalCollector` caches chip directory names and only
         re-enumerates when a cached one has vanished** — checked directly
         (`reader.exists`) against every cached name each refresh, not
         inferred from a read failure deep inside chip parsing (which,
         because sensor reads are fail-soft per point 0 above, mostly
         *wouldn't* fail even for a vanished chip). This gives a narrower
         guarantee than "hwmon chips can appear on module load" alone
         suggests: a chip *disappearing* is always caught within one refresh;
         a chip *appearing on its own* is only picked up as a side effect of
         some other chip disappearing in the same refresh. Both directions
         are pinned down by name in the test file
         (`a_new_chip_is_not_picked_up_by_itself` and
         `a_new_chip_is_picked_up_when_another_disappears_in_the_same_refresh`)
         so a future change to this behaviour is a decision, not a silent
         regression.
      5. **`thermal_zone*` trip points (`trip_point_N_temp`/`_type`) are not
         implemented** — the fallback reports `max`/`crit` as `None`
         throughout, matching the design doc's framing of this interface as
         "the poorer one." Partially implementing trip-point parsing was
         judged worse than clearly not having it.
      6. **Chunk 2's `sysfs.rs` test fixture (`TempTree`) was widened from
         private to `pub(crate)`** so this chunk's tests could reuse it
         instead of duplicating ~80 lines of temp-directory scaffolding. Test
         infrastructure only — no production code changed by this.

- [x] **6. `collectors::disk` + `collectors::net`.** *(done 2026-08-07)*
      - `/proc/diskstats` (sectors are **512 bytes**, always, regardless of the
        device's real sector size — a classic wrong-by-8x bug), filtering out
        loop/ram/zram devices and partitions-of-a-listed-disk.
      - Mount capacity/used: parse `/proc/self/mounts`, skip pseudo-filesystems.
        **Free-space numbers need `statvfs`, which `std` has no access to** —
        that's chunk 8's blocker. Until then this chunk ships capacity as
        `None` and the throughput half works fine.
      - `/proc/net/dev` + `/sys/class/net/*/{operstate,mtu}`.
      Tests: fixture files; assert loop devices are filtered; assert an
      interface appearing mid-session (hotplug) doesn't corrupt rate tracking.

      **25 new tests (142 in the crate), and read against this machine's real
      `/proc/diskstats`, `/proc/self/mounts`, and `/proc/net/dev`** — every
      value cross-checked against direct reads of the same files. That run
      confirmed the collectors filter down to exactly what a human would
      expect: 5 real disks out of ~60 diskstats lines (loop devices, LVM
      `dm-*` volumes, and every partition of a listed disk correctly excluded),
      5 real mounts out of the machine's full mount table (every pseudo-fs
      correctly excluded), and 3 visible interfaces with `operstate`/`mtu`
      matching a direct `cat` on the underlying `/sys` files exactly.

      Decisions worth knowing about:

      1. **A malformed diskstats/net-dev line is a hard `Error::Parse` that
         fails the whole collector for that refresh — this is the opposite
         default from chunk 5's thermal collector.** `/proc/diskstats` and
         `/proc/net/dev` are each *one file* where every line is expected
         well-formed by the kernel; a malformed line means something is
         fundamentally wrong (a truncated read, a corrupted `/proc` entry),
         not "one device doesn't have this optional field" the way a missing
         `temp3_max` does. This mirrors chunk 3's `/proc/stat` reasoning, not
         chunk 5's per-sensor one — worth remembering that this crate has
         *two* different fail-soft postures depending on whether a source
         file's lines are independent optional records or one coherent table.
      2. **`read_link_state` (interface `operstate`/`mtu`) is the one
         exception, and is fail-soft per field** — same reasoning as
         thermal's trip points: these are decoration on top of the counters
         the collector exists for, read from a *different* file
         (`/sys/class/net/*`, not `/proc/net/dev` itself), so a garbled or
         absent value there doesn't indicate the counter data is suspect.
      3. **Two partition-naming conventions are recognised**: `sda`→`sda1`
         (bare digit) and `nvme0n1`→`nvme0n1p1` / `mmcblk0`→`mmcblk0p1`
         (`p`-then-digit). Missing the second would have left every NVMe
         partition on this exact machine showing up as a fake extra "disk".
      4. **Found and fixed a real bug during implementation, not review**:
         the first draft of `unescape_mount_field` walked `raw.as_bytes()`
         and cast individual non-escape bytes to `char` — correct for ASCII
         but silent *data corruption* for any multi-byte UTF-8 character in a
         mount label (each byte of a 3-byte character becomes a bogus
         separate codepoint). Fixed to walk `chars()` instead, with a test
         (`unescape_does_not_corrupt_multi_byte_utf8`) asserting a Japanese
         label survives intact. This is exactly the kind of bug the "port to
         Rust" framing doesn't catch by construction — it only shows up when
         you ask "what if the byte isn't ASCII," so it's recorded here as a
         reminder to ask that question of any future byte-indexed string
         code in this crate.
      5. **`unescape_mount_field` passes an unrecognised backslash through
         unchanged rather than erroring** — decoding is scoped to the four
         documented escapes; anything else is not this crate's business to
         validate.
      6. **The required hotplug test lives in `net.rs`, not `delta.rs`.**
         Chunk 4 already proves `RateTracker`'s name-matching logic in
         isolation with hand-built fixtures; this chunk's version instead
         proves `parse_net_dev`'s actual *output* is what flows correctly
         through that matching — a different, narrower claim, and the one
         this chunk was actually responsible for.

- [x] **7. `render/` + `cli.rs` + the `--once` binary.** *(done 2026-08-11)* Hand-rolled arg parsing
      over `std::env::args` (no `clap` — same reasoning as `rustlogger`'s
      hand-rolled timestamp formatting). Hand-rolled JSON writer with RFC 8259
      escaping, plus a plain-text table renderer.
      At the end of this chunk `rustmon --once --format json` is a **complete,
      useful, zero-external-dependency tool.** Everything after this is
      enhancement.
      Tests: JSON escaping of `"`, `\`, `\n`, ` `, and a device name
      containing an ANSI escape sequence; golden-file test of a full snapshot;
      round-trip parse check of the emitted JSON.

      `rustmon --once` is a real, working binary as of this chunk — 233
      tests in the crate, and verified against actual output, not just unit
      tests: `--once` (text), `--once --format json` (piped through
      Python's `json.load` to confirm it's genuinely valid JSON, then
      checked field by field against this machine's real hardware),
      `--help`, `--version`, an unknown flag, and a nonexistent
      `--sysfs-root` all behave correctly, under both `cargo build` and
      `cargo build --no-default-features`.

      Found and fixed a second real bug while wiring this chunk together,
      before any renderer code ran: `disk.rs`'s `parse_mounts` (chunk 6)
      never sanitised `source`/`mount_point`/`fs_type`. Every other
      kernel-supplied string in this crate is sanitised at the collector
      boundary specifically so no renderer has to remember to — but mount
      fields aren't charset-restricted by `is_safe_component` the way device
      names are (a real path can contain almost anything), and
      `unescape_mount_field` only decodes the four documented octal escapes,
      it doesn't strip control characters. A hostile mount source (a
      crafted FUSE daemon, an LVM volume name) could have carried a raw ESC
      sequence straight into JSON output. Fixed in `disk.rs`, not in the
      renderer, with a regression test
      (`a_hostile_mount_source_is_sanitised`) proving it now goes through
      `sanitize_kernel_string`.

      Decisions worth knowing about:

      1. `JsonWriter`'s comma logic needed a dedicated `after_key` flag, not
         just the per-container `needs_comma` stack the skeleton's field
         comment implied. A key and its value are one JSON member, but
         `key()` and the value-writing methods are separate calls, and
         those same value methods are also used bare as array elements,
         where they must insert a comma themselves. `after_key` is what
         lets the same method tell the two situations apart.
      2. The "round-trip parse check" the plan asked for is a hand-rolled
         structural walk (brace/bracket balance, string termination, no raw
         control byte inside a string), not a full JSON parser — adding a
         crate for it would need crate-checklist sign-off for a test-only
         dependency, and a real DOM model is more than proving a
         hand-rolled writer didn't drop a comma or mismatch a container
         needs. `assert_structurally_valid_json` in `render/json.rs`'s
         tests is reused across the golden-snapshot test and a second test
         that populates every optional section at once.
      3. Every write error in `render/json.rs` and `render/text.rs` uses
         the synthetic path `"<output>"` — `Error::Io` always carries a
         path, but the `&mut dyn Write` these modules write to is arbitrary
         (stdout in production, a `Vec<u8>` in tests), so there's no real
         file path to report.
      4. `render/json.rs`'s `format_rfc3339` is the third implementation of
         Howard Hinnant's `civil_from_days` algorithm in this repo (after
         `rustlogger/src/timestamp.rs`). Not shared across the two crates —
         a shared time-formatting module would be new public API surface
         for two call sites. Its tests reuse `rustlogger`'s exact reference
         dates, a direct parity proof rather than a spot check.
      5. `main.rs` walks the full `source()` chain when printing an error,
         not just the top-level message — `Error::Io`'s inner `io::Error`
         is genuinely useful context for a bad `--sysfs-root`.

- [x] **8. Mount capacity via `statvfs`.** *(done 2026-08-12)* `nix` (`fs`
      feature) approved same day, proposal "rustmon A" in
      `docs/crate-checklist.md`. `read_capacity` in `collectors/disk.rs` calls
      `nix::sys::statvfs::statvfs`, multiplying `f_frsize` (fragment size) by
      `f_blocks`/`f_bavail` — `f_bavail`, not `f_bfree`, since "available to
      an unprivileged user" is what a user actually cares about, the same
      choice `df` makes. `fs-capacity` is now in `default`, alongside `tui`.

      Verified against this machine's real mounts, byte-for-byte against
      `df -B1`: total matched exactly on all four real mounts (`/`, `/boot`,
      `/boot/efi`, and an external `sda1` mount); available matched exactly
      on three and was 40 KiB off on `/` (live disk activity between the two
      commands a few seconds apart, not a discrepancy in the math). 7 new
      tests (245 in the crate): pure arithmetic (`capacity_from_raw`,
      including overflow and a zero-fragment-size edge case) plus two tests
      against a real `statvfs("/")` call and a nonexistent path.

      Decisions worth knowing about:

      1. **`statvfs(3)` has no timeout parameter, and a stale/unreachable NFS
         mount can block it indefinitely** — the exact hang this module's own
         `todo!()` placeholder warned about before this chunk existed. There
         is no non-blocking or cancellable `statvfs` in `std` or `nix`, so
         `read_capacity` runs the call on a helper thread and stops waiting
         after 500 ms (`STATVFS_TIMEOUT`). A genuinely hung mount leaks that
         one thread rather than hanging the collector — there is no safe way
         to cancel a thread blocked inside a syscall short of a signal, which
         would race the syscall's own internal locking. This is a bounded,
         explicitly documented cost, not a fix for the underlying hang: a
         long-running live TUI hitting the same hung mount every refresh
         accumulates one leaked thread per refresh. Acceptable for now
         because `--once` (the only mode that exists as of this chunk) always
         exits regardless; worth revisiting if chunk 9's live TUI turns out
         to make repeated hangs on the same mount a realistic scenario rather
         than a rare one.
      2. **Capacity is decoration, fail-soft per mount** — same rule as
         `read_link_state` in `net.rs`. Permission denied, the mount
         vanishing mid-refresh, byte-math overflow, and the timeout above all
         degrade to `Ok(None)` for that one mount's `total`/`available`
         rather than failing the whole disk collector. Seen for real on this
         machine: `/run/user/1000/doc` (a `fuse.portal` mount) reports no
         capacity fields at all in the JSON output, while every other mount's
         numbers came through — exactly the fail-soft behaviour this was
         designed for, not a hole found after the fact.
      3. **The block-size arithmetic is split into its own pure function,
         `capacity_from_raw`**, specifically so it can be tested without a
         real filesystem, `nix`, or even the `fs-capacity` feature —
         `#[cfg(any(feature = "fs-capacity", test))]` keeps it out of a
         `--no-default-features` production binary (where it would be dead
         code, since nothing else calls it) while still compiling under
         `cargo test --no-default-features`.

- [x] **9. TUI frontend.** *(done 2026-08-12)* `ratatui` (`0.30.2`) +
      `crossterm` (`0.29.0`) approved same day, proposal "rustmon B" in
      `docs/crate-checklist.md`. `ui/layout.rs` (responsive panel placement),
      `ui/widgets.rs` (per-panel `ratatui` drawing), `ui/app.rs` (`App` state
      + keybindings), `ui/mod.rs` (event loop + terminal guard) are all
      implemented — bare `rustmon` now runs the live TUI.

      52 new tests (290 unit tests + the existing 8 integration tests = 298
      total in the crate): `layout::compute`'s responsive rules
      and its "never zero-size, never past bounds" invariant (swept across a
      grid of realistic terminal sizes, not just point examples);
      `widgets`' per-panel draw functions rendered into a `ratatui`
      `TestBackend` and asserted on actual screen content, including the
      "absent renders as `—`, never `0`" rule and a real stopped-fan-shows-
      `0`-not-`—` counter-case; `app::Panel`'s cycling/wrapping and
      `App::on_key`'s keybindings, all built and tested with **no terminal
      at all** — that's the point of keeping `KeyPress`/`Rect`/`Colour`
      backend-independent rather than using `crossterm`/`ratatui` types
      directly in `app.rs`/`layout.rs`.

      **Manually verified in a real interactive session**, not just unit
      tests — this project's established bar for anything UI-facing. Spawned
      the compiled binary attached to a real pty (Python's `pty`/`fcntl`
      modules, `TIOCSWINSZ` set to a real size, no `tmux`/`script` needed)
      and drove it with real keystrokes: the header showed this machine's
      actual hostname, kernel release, CPU model, and uptime; all six data
      panels rendered real values (per-core busy%/frequency for all 24
      cores, all 7 real hwmon chips, both real AMD GPUs, real disk/mount
      capacity numbers from chunk 8, live network interface state); `?`
      opened the help overlay with every documented keybinding legible;
      `space` toggled the `PAUSED` indicator; `q` produced a clean exit
      (code 0) with the alternate-screen-leave sequence confirmed present in
      the output, proving `TerminalGuard`'s `Drop` actually ran.

      Decisions worth knowing about:

      1. **Two chunk-0 scaffold gaps, found and resolved the same way earlier
         ones were** (`Gpu::freq_khz` in chunk 10, the label fallback in
         chunk 5): `ui/widgets.rs`'s own doc promised the header would show
         "hostname, kernel, uptime" but `Snapshot` had nowhere to carry
         hostname/kernel (they're static machine identity, not a
         per-refresh reading, so they don't belong on `Snapshot` the way
         CPU/memory do). Resolved by having `App::new` read
         `proc/sys/kernel/hostname`/`osrelease` once, through the same
         `SysfsReader` + `sanitize_kernel_string` every collector uses, and
         adding `hostname`/`kernel` fields to `App` — not a new collector,
         just two more reads through the existing filesystem boundary. And
         `draw_header`/`draw_cpu`/etc. needed a `&mut ratatui::Frame`
         parameter the `todo!()` stubs never had (there's no way to
         actually render a `ratatui` widget without one) — added to every
         `draw_*` signature, with the crate's own backend-independent `Rect`
         still the type callers pass in and convert internally.
      2. **`ratatui::try_init`/`restore` used instead of hand-rolled
         `crossterm` raw-mode/alternate-screen/panic-hook calls.** The
         chunk-0 skeleton's `TerminalGuard`/`enter_terminal` shape suggested
         hand-rolling this the way `rustlogger/src/terminal/` does for its
         own backend, but `ratatui` 0.30 ships this exact thing — raw mode,
         alternate screen, and a chained panic hook — as its own blessed
         `try_init`/`restore` pair. Re-implementing it by hand would just be
         a second, unpracticed version of code the dependency already gets
         right. `TerminalGuard` now wraps the `Terminal` `try_init` returns;
         its `Drop` calls `restore()` for the normal-exit and `Err` paths,
         while `try_init`'s own installed hook covers the panic-during-
         drawing case the module doc called out as this chunk's own
         responsibility.
      3. **`statvfs`'s hang risk (chunk 8) has a TUI-shaped consequence,
         noted there and worth restating here**: a long-running live TUI
         hitting the same hung/stale NFS mount every refresh accumulates one
         leaked thread per refresh, whereas `--once` only ever risks one.
         Not fixed in this chunk — flagged as a known cost, not a silent
         gap, same as chunk 8 already documented it.
      4. **`VecDeque::make_contiguous` is called once per `App::refresh`,
         not once per draw.** `draw_cpu`'s sparkline needs a contiguous
         `&[Snapshot]`, and calling `make_contiguous` on every single frame
         (up to dozens of times a second while a key is held) to serve a
         value that only changes once per refresh would be wasted work for
         no benefit — doing it exactly when the history buffer actually
         changes is the same "don't recompute what didn't change" principle
         chunk 5's chip-existence caching already established for this
         crate.
      5. **`App::on_resize` is a deliberate no-op**, not an oversight:
         `layout::compute` is called fresh from `frame.area()` on every
         single draw, so a resize is already picked up by the very next
         frame with no state in `App` to update. Kept as a real method
         (not deleted) to preserve the event loop's documented dispatch
         shape and leave room for a future feature that *does* need to
         react to a resize.

      `lib.rs::run` no longer has anything left `todo!()` to hit —
      `ui::run` is fully implemented, and the `Error::Unsupported` path
      under `--no-default-features` (or a default build run without
      `--once`, on a build that never compiled `ui` in) is the only
      remaining "not available" case, exactly as designed.

- [x] **10. GPU. Partly blocked — AMD/Intel done 2026-08-11, NVIDIA still
      blocked.**
      - AMD (`/sys/class/drm/card*/device/gpu_busy_percent`, `mem_info_vram_*`,
        `hwmon/` for temp/power/fan) and Intel (`/sys/class/drm/card*/`) are
        plain sysfs reads — **not blocked**, can be done any time after chunk 5.
      - NVIDIA needs NVML. 🔒 **BLOCKED — crate checklist** (`nvml-wrapper`),
        and note the separate security concern: NVML is a `dlopen` of a
        proprietary library into our address space. Gated behind a non-default
        `gpu-nvidia` feature.

      19 new tests, and the AMD half read against this machine's two real
      AMD GPUs (`card1`, `card2`) — busy%, VRAM total/used, temperature, and
      the `power1_average`-then-`power1_input` fallback all cross-checked
      against direct `cat` reads. No Intel GPU exists on this machine, so
      that half is fixture-built against the documented i915/xe sysfs layout
      instead — kernel ABI, not a guess, but not read against real Intel
      hardware the way every other collector in this crate has been.

      A real gap in `sample::Gpu` was found and fixed while writing this
      chunk, not before it. `collectors::gpu::intel`'s own module doc calls
      `gt_cur_freq_mhz` "the most useful signal this module can offer"
      (Intel has no utilisation percentage in sysfs at all) and declared
      `read_frequency` returning `Option<KiloHertz>` — but the `Gpu` struct
      chunk 0 scaffolded had nowhere to put that value. Added
      `Gpu::freq_khz: Option<KiloHertz>`, `None` for AMD/NVIDIA today,
      populated for Intel. Same category of fix as chunk 5's label-fallback
      doc conflict: a real inconsistency between two chunk-0 artifacts,
      found by implementing against both and noticing they didn't fit
      together.

      Decisions worth knowing about:

      1. AMD and Intel always return `Ok(Some(Gpu))`, never `None`, once
         dispatched. Unlike thermal's "a chip with nothing to report is
         indistinguishable from absent," a GPU's identity (`card0`) is
         already established by the time `amd::read`/`intel::read` is
         called — vendor detection already happened in `gpu::read_vendor`.
      2. `GpuCollector` caches `cardN` names but without thermal's
         staleness-recheck machinery. Hot-plug GPUs are far rarer than
         hwmon chips appearing on module load, so "enumerate once per
         process, done" is the honest trade-off — documented explicitly as
         a narrower guarantee than `ThermalCollector`'s, not an oversight.
      3. One card failing is fail-soft at the collector level (pushed to
         `Snapshot::errors`, other cards still reported) — this machine
         having two real AMD GPUs made that worth actually testing.
      4. `find_hwmon_dir` lives in `amd.rs`, and `intel.rs` calls into it
         directly (`super::amd::find_hwmon_dir`) rather than duplicating
         the identical hwmon-node-lookup logic — both drivers nest their
         hwmon node under `device/` the same way.
      5. Intel's `read_vram` derives `used` from `total - available`
         (`saturating_sub`) rather than reading it directly — the kernel
         exposes `lmem_avail_bytes`, not a used figure, the opposite of
         AMD's `mem_info_vram_used`. `used` stays `None` if either file is
         missing rather than treating a missing `available` as zero (which
         would falsely report 100% VRAM used).

- [x] **11. Integration tests + README.** *(done 2026-08-11)* End-to-end test
      running the compiled binary against a fixture `--sysfs-root` and
      asserting the JSON output; `README.md` covering usage, the output
      schema, what needs root and why, and the read-only guarantee.

      **8 tests in `rustmon/tests/integration.rs`**, the first tests in this
      crate that exercise the actual compiled binary via
      `std::process::Command` rather than calling functions directly — CLI
      parsing, config validation, collector dispatch, and rendering, all
      wired together the way a real invocation hits them. Covers
      `--once`/`--format json`/`--format text` against a hand-built fixture
      root (field-checked, not just "did it run"), `--only`, `--verbose`'s
      error list, `--help`/`--version`, an unknown flag's exit code, a
      nonexistent `--sysfs-root`'s exit code, and — the one this chunk exists
      to prove operationally, not just assert in docs — that a full `--once`
      run leaves every byte of the fixture tree unchanged (`before ==
      after` on a full recursive snapshot of file contents).

      Decisions worth knowing about:

      1. **The integration test's fixture-tree builder duplicates
         `sysfs::tests::TempTree` rather than reusing it.** This file
         compiles as a separate crate against `rustmon`'s public API (that's
         what makes it a true black-box test of the binary) and has no
         access to the library's `#[cfg(test)]`-only internals — unlike
         chunk 5's reuse of `TempTree` across collector test modules
         *within* the library, which could just widen visibility.
      2. **The JSON assertions go through a second, independent hand-rolled
         JSON reader**, not `render::json`'s own writer-side structural
         validator. Sharing one wouldn't actually prove anything: if the
         *writer* had a bug that happened to produce output its own
         validator still accepted, reusing that same validator here would
         validate it right back. A field-value reader built without
         reference to the writer's internals is a genuinely independent
         check, which is the actual point of an end-to-end test.
      3. **The `--verbose` test forces a real collector failure** (a
         garbled `/proc/stat`) rather than asserting the errors array is
         merely absent-or-present in the happy path — proving the
         fail-and-report path chunk 3 built, not just that `--verbose`
         doesn't crash.
      4. **The README's JSON schema example is real captured output** from
         this machine (field values included), not synthesised — same
         standard every other chunk's fixtures have held to.

---

## Second phase: connection visibility + shell-prompt summary

Planned 2026-08-12 (see the approved plan in that session for full
reasoning). Six more chunks. Chunk 17 is blocked on a crate-checklist
sign-off; the other five are not.

- [x] **12. Man page + doc pass.** *(done 2026-08-12)* New
      `rustmon/man/rustmon.1`, matching `rustlogger/man/rustlogger.1`'s
      exact groff structure and style (`.TH`, `.SH NAME/SYNOPSIS/
      DESCRIPTION/OPTIONS/KEY BINDINGS/SUMMARY MODE/EXIT STATUS/OUTPUT/
      EXAMPLES/LIMITATIONS/SEE ALSO/AUTHOR`), documenting every current
      flag and every TUI keybinding, plus `--summary` (chunk 13, written
      alongside it since the two chunks share this one file and doing it
      twice would mean re-deriving the same section structure). Renders
      clean under `groff -mandoc -T utf8 -ww` — zero warnings.

      One deviation worth noting: rustlogger's OPTIONS entries are all a
      single long flag, so `.BI`/`.BR` macro chaining was enough. rustmon's
      options are mostly `-x, --long VALUE` (short form + long form +
      placeholder together), which doesn't chain cleanly through those
      macros with more than two alternating fonts — used inline `\fB`/
      `\fI`/`\fR` font-escapes for those instead, keeping the rest of the
      file identical to rustlogger's macro style.

- [x] **13. `rustmon --summary`.** *(done 2026-08-12)* Fast, shell-prompt-
      friendly one-liner — intended to run before every shell prompt
      (oh-my-posh command segment), so it must be well under 100ms.
      `Config::summary: bool`, a deliberately narrow hardcoded
      `cpu`+`memory`+`thermal` collection via `Registry::from_collectors`
      (ignores `--only`/`--skip`; no `statvfs`, no `/proc/<pid>` walk —
      the only collectors with zero exposure to this project's own
      documented latency risks). `status`/`headline` computed from
      `TempSeverity` counts + `Snapshot::errors` in a new pure, testable
      `summary::summarise` function. Silent + exit 1 when `status == "ok"`
      (nothing worth surfacing); JSON via the existing `render/json.rs`
      `JsonWriter` otherwise. New top-level `summary` module, checked in
      `lib.rs::run()` before `once` (mutually exclusive in practice —
      whichever flag the CLI parser sets last wins).

      8 new tests (306 in the crate), plus real-machine verification:
      `rustmon --summary` measured at ~7–23ms across 30 runs on this
      machine (well under the 100ms budget, and that already includes
      process-spawn overhead from the Python harness doing the timing) —
      and, since this machine's own sensors are all currently normal, both
      the warning path (`{"status":"warning","headline":"1 warning"}`, a
      sensor fixture at 99°C against a 90°C max) and the critical path
      (105°C against a 100°C crit) were exercised against a hand-built
      fixture tree, not just unit-tested.

      Decisions worth knowing about:

      1. **`--summary` is not "restricted `--once`."** It never consults
         `--only`/`--skip` at all — it always runs exactly `cpu`+`memory`+
         `thermal`, full stop. Reusing `--only` semantics here would let
         `rustmon --summary --only disk` silently reintroduce the exact
         latency risk (`statvfs`) this mode exists to avoid.
      2. **No new severity thresholds invented.** `summarise` only reuses
         `TempSeverity`, the one severity concept this crate already has.
         Memory/CPU usage have no "this counts as a warning" definition
         anywhere else in the crate, and inventing one here — just to make
         the headline richer — would be exactly the kind of scope creep
         this module's own doc comment calls out and refuses.
      3. **`status == "ok"` prints nothing, not `{"status":"ok",...}`.**
         The empty-output + exit-1 combination is the actual point of this
         chunk: it's what lets a shell-prompt segment (oh-my-posh's command
         segment, specifically) disappear automatically with zero extra
         configuration on its end.

- [x] **14. `collectors::connections`.** *(done 2026-08-13)* Process ↔ port
      ↔ remote-endpoint mapping: `proc/net/{tcp,tcp6,udp,udp6}` parsing,
      filtered to internet-facing active connections only (`is_public_ip`,
      excludes RFC1918/loopback/link-local/multicast/listening sockets),
      correlated to PID + program name via the new
      `SysfsReader::read_link` primitive (`/proc/<pid>/fd/N` →
      `socket:[inode]`) and `proc/<pid>/comm`. `read_link` needed its own
      confinement-without-following logic — `resolve()`'s existing
      `canonicalize` would break on procfs's fake symlink targets — with
      its own symlink-escape test coverage matching chunk 2's original
      rigor for `resolve()`, including the exact parity check that
      matters: a symlink whose *own path* escapes the root is still
      rejected, while a symlink whose *target string* points outside the
      root is correctly returned as-is (never opened, so it can't be used
      to escape anything).

      41 new tests (339 in the crate: 331 unit + 8 integration), plus
      real-hardware verification on this machine, cross-checked against
      `ss -tp` — every attributable connection matched exactly, including
      fd-level detail (`claude-desktop`/pid 10791 → `140.82.121.6:443`,
      `claude`/pid 21794 → dozens of connections to `160.79.104.10:443`),
      and a real loopback connection (`steam` on `127.0.0.1`) was correctly
      excluded by `is_public_ip` without needing to special-case it by
      process name. `--summary` (chunk 13) was re-measured afterward and is
      unaffected (~10–15ms) — exactly the point of hardcoding its
      collector set independently of what other collectors exist.

      Decisions worth knowing about:

      1. **`read_link`'s confinement is deliberately asymmetric, and that
         asymmetry is the whole design.** The symlink's own *path* goes
         through the same full `resolve()` (component allowlist, `..`/
         absolute rejection, intermediate-symlink containment) every other
         method uses — implemented by resolving the path's *parent*
         normally and only leaving the final component unresolved. The
         symlink's *target string* is never checked against the root at
         all, on purpose: it's returned as plain data (parsed for the
         `socket:[N]` pattern), never reopened or re-resolved through this
         reader, so where it points is irrelevant to confinement — the same
         reasoning that already lets `read_to_string` return arbitrary file
         *contents* without those contents being a path-traversal risk.
      2. **The `/proc/<pid>` walk is demand-driven, not exhaustive.**
         `resolve_owners` computes the exact set of socket inodes this
         refresh actually needs *before* touching `/proc/<pid>` at all,
         then stops walking further PIDs the moment every needed inode is
         matched — avoiding the thousands-of-`readlink`-calls-a-second cost
         a naive "walk every process's every fd" approach would have on a
         busy machine, and the same reasoning `--summary` (chunk 13) uses
         to justify never running this collector at all.
      3. **A connection's `uid` always comes through, even when `pid`/
         `program` can't be resolved.** `/proc/net/{tcp,udp}*` reports the
         owning uid directly, with no `/proc/<pid>` access needed — so a
         connection owned by another user (the routine `EACCES` case)
         degrades to "owner uid known, process identity not" rather than
         disappearing or going fully anonymous. Verified for real on this
         machine: a root-owned SSH connection showed `uid: 0` with no
         `pid`/`program`, exactly as designed.
      4. **`/proc/net/{tcp,tcp6,udp,udp6}` is one coherent kernel table per
         file** (same reasoning as chunk 6's `/proc/diskstats`), so a
         malformed line is a hard `Error::Parse` for that file — but each
         of the four files is independently optional (a kernel built
         without IPv6 genuinely has no `tcp6`/`udp6` at all), matching
         chunk 6's per-file fail-soft/whole-file fail-hard split rather
         than chunk 5's thermal per-sensor one.
      5. **IPv6 "private" ranges are hand-checked by octet, not via `std`'s
         `Ipv6Addr` predicates** — `is_unique_local`/`is_unicast_link_local`
         and similar were nightly-only for a long time, and hand-checking
         `fe80::/10` and `fc00::/7` directly keeps this on stable `std`
         with no version-gating, consistent with this crate's general
         preference for hand-rolled logic over relying on less-certain
         standard-library surface.

- [x] **15. DNS resolution + the connections panel checkbox/cursor UX.**
      *(done 2026-08-13)* Hand-rolled UDP PTR-query client
      (`net_probe/dns.rs`, reads `/etc/resolv.conf` via the existing
      `SysfsReader`, zero new dependency — transparently benefits from
      `unbound` or any other locally-configured resolver). `App`-side
      `enrichment: HashMap<IpAddr, Enrichment>` cache +
      background-thread-per-lookup pattern (same thread+channel shape as
      chunk 8's `read_capacity`). New `Panel::Connections` (7th panel,
      appended so `1`-`6` stay exactly as tested), `Up`/`Down` to move a row
      cursor, `x` to toggle a checkbox, `Enter` to trigger DNS (and, once
      chunk 17 lands, traceroute) for every checked row's remote IP.

      36 new tests (371 in the crate: 363 unit + 8 integration), plus two
      rounds of real-world verification. The DNS client itself: resolved
      `8.8.8.8`→`dns.google` and `1.1.1.1`→`one.one.one.one` — both
      genuinely correct — in ~16ms round trips against this machine's real
      configured resolver, with the full RFC 1035 compression-pointer
      decoder exercised for real (not just the hand-built fixture in the
      unit tests). The checkbox/cursor UX: driven through a real pty
      session (the same technique chunk 9 used) against this machine's
      actual open connections — `7` focused the panel, `x` checked a row,
      `Enter` triggered resolution, and the panel correctly transitioned
      from `resolving...` to `no PTR record` once the real (genuinely
      NXDOMAIN) answer for `160.79.104.10` landed, with the result
      correctly applied to every other row sharing that same remote IP —
      exactly the point of keying `enrichment` by IP rather than by
      connection. Clean exit and terminal restore confirmed as usual.

      Decisions worth knowing about:

      1. **Enrichment is keyed by remote IP, not by connection**, and this
         was directly proven live, not just asserted: this machine had a
         dozen-plus separate `claude` connections all talking to the same
         `160.79.104.10`, and checking + resolving just one of them
         populated the domain for every other row sharing that address —
         the real-world case the design was built for, not a hypothetical.
      2. **`Enrichment`'s `EnrichState` distinguishes "queried and got
         nothing" (`Done(vec![])`, rendered as `no PTR record`) from
         "couldn't even ask" (`Failed`)** — an `RCODE` like `NXDOMAIN` is a
         complete, successful answer that happens to say "nothing here,"
         a materially different fact from a timeout or an unconfigured
         resolver, and the UI says which one happened rather than
         collapsing both into one generic "failed" state.
      3. **A `Failed` lookup is retriable by design, with no separate retry
         key.** `resolve_checked` only skips IPs already `Pending`/`Done` —
         a `Failed` (or never-requested) entry is fair game, so pressing
         `Enter` again on a still-checked row is the retry mechanism.
      4. **`connections_cursor` is clamped in a dedicated
         `clamp_connections_cursor` method, split out from `refresh` itself**
         specifically so the clamp-on-shrink behaviour is unit-testable
         without needing a real collector round trip to shrink the list —
         connections are the only panel whose backing list can plausibly
         shrink between refreshes (a socket closing), so this is new
         territory none of the other six panels needed.

- [x] **16. Crate-checklist proposal for chunk 17.** *(written 2026-08-14,
      awaiting sign-off)* Not a new crate — a feature addition to the
      already-approved `nix` (add `socket` to the existing `features =
      ["fs"]` line), following rustlogger's "chunk 4 nix feature additions"
      precedent — see "rustmon D" in `docs/crate-checklist.md`. Re-confirmed
      directly on this machine while writing the proposal: raw ICMP +
      `CAP_NET_RAW` is required (`net.ipv4.ping_group_range` is `1 0` —
      disabled — so unprivileged ping sockets are a dead end here; `man 7
      raw` confirms the `CAP_NET_RAW` requirement), and `nix` 0.31.3's
      `socket`/`setsockopt`/`sendto`/`recvfrom` (confirmed present in the
      vendored source) cover everything needed with zero new transitive
      dependencies (`socket` only pulls in `memoffset`, already transitive
      regardless). Chunk 17 does not start until this is signed off.

- [x] **17. Traceroute.** *(done 2026-08-14)* Hand-rolled ICMP Echo
      Request/Reply/Time-Exceeded construction and parsing (RFC 1071
      checksum, bounds-checked throughout via `slice::get`, no panics) over
      a raw socket, IPv4 only, one probe per TTL up to 30 hops with a
      1.5s-per-hop timeout and an 18s overall budget. `EPERM`/`EACCES` on
      socket creation (no `CAP_NET_RAW`) is fail-soft absence — `trace()`
      returns `None`, never an error — the same "some sensors need root,
      and that's fine" pattern as the rest of the crate, scaled up to a
      whole feature. New `traceroute` Cargo feature (in `default`,
      independent of `tui` — `tui` without `traceroute` stays a fully valid
      build with DNS resolution but no route tracing), pulling in `nix`'s
      `net` feature (approved as "rustmon D" in `docs/crate-checklist.md`).
      Triggered the same way as chunk 15's DNS lookup — one `Enter` on
      checked rows now spawns both a DNS thread and a traceroute thread per
      newly-triggered IP — never wired into `--once`/`--summary` (too slow
      — same "no rates in `--once`" precedent, just bigger).

11 new tests, all directly in this chunk's own modules (`net_probe::traceroute`'s
      10 plus one new `ui::widgets` route-rendering test) — 374 unit + 8
      integration = 382 total in the default build, up from 371 before this
      chunk. `cargo test`/`cargo clippy` also confirmed clean, separately,
      in two other feature combinations: `tui` without `traceroute` (361
      unit tests — this combination is now a real, independently valid
      build rather than an untested assumption) and
      `--no-default-features` (277 unit tests, unchanged — traceroute never
      touches the dependency-free build). Real-world verification, same bar as every
      other chunk: the checksum algorithm's self-verifying property (a
      correctly-checksummed buffer always re-sums to exactly zero) rather
      than a hand-picked reference value; the Time-Exceeded parser tested
      against a realistic embedded-original-packet payload, not a
      simplified stand-in; and — since this sandbox genuinely has no
      `CAP_NET_RAW` (confirmed and cited in the crate-checklist proposal
      itself) — the fail-soft "unavailable" path is exercised for real, not
      mocked, both directly (`trace()` returns `None` here, asserted) and
      through a live pty session showing the connections panel correctly
      transition `tracing…` → `unavailable` end to end, with a clean exit
      and terminal restore. **A true privileged real-network trace (with
      `CAP_NET_RAW` actually granted) was not run** — doing so needed
      `sudo`, which needs an interactive password this session cannot
      supply (and entering one is outside what this crate — or this
      assistant — does regardless of who's asking); this is recorded here
      as a known verification gap, not silently glossed over.

      Decisions worth knowing about:

      1. **Found and fixed a real crate-checklist gap during
         implementation, not review.** The approved proposal scoped `nix`'s
         `socket` feature as sufficient; the compiler disagreed on the
         first build attempt, naming exactly the missing type
         (`SockaddrIn`, needed to address the probe's destination) and
         exactly which feature actually gates it (`net`, not `socket`
         alone — though `net = ["socket"]` in `nix`'s own feature graph, so
         it's a strict superset with the identical zero-new-dependency
         footprint the approval was granted on). Corrected in both
         `rustmon/Cargo.toml` and the crate-checklist entry itself, with an
         explicit amendment note — a one-feature-name fix, not a re-review,
         but the kind of thing worth being honest about rather than quietly
         editing the original proposal as if it had always said `net`.
      2. **A whole traceroute per `Enter` press runs on exactly one
         background thread**, not one thread per hop. The plan's own
         sketch suggested a thread-per-hop shape mirroring `read_capacity`
         literally; the simpler "one thread walks all 30 possible hops
         sequentially, sends one final result" design was chosen instead —
         no incremental per-hop UI updates in this version, but
         meaningfully less complexity for a feature explicitly described as
         "doesn't have to be instant." Per-hop streaming is a reasonable
         later enhancement if the all-at-once wait proves annoying in
         practice, not something this chunk needed to solve pre-emptively.
      3. **`Enrichment::route` and the `EnrichmentUpdate::Route` variant
         are behind `#[cfg(feature = "traceroute")]`**, unlike `domains`
         which is unconditional — this is what makes `tui` without
         `traceroute` a real, independently buildable and testable
         configuration rather than an assumption nobody checked. Caught by
         building that exact combination deliberately, not by accident.
      4. **The connections panel doesn't distinguish "trace timed out"
         from "no `CAP_NET_RAW`"** — both collapse to `Failed` →
         `unavailable` in the UI. A more granular error channel was
         considered and rejected as premature: the user-facing action is
         identical either way (nothing to do about it from inside the
         TUI), so a finer distinction would be detail with no decision
         attached to it.

---

## Second phase, continued: connections panel process tree

- [x] **Chunk 18: connections panel — process tree (parent/child grouping +
      collapse), plus two small bugs found while testing the built
      binary.** Testing the just-shipped chunks 12–17 against the real
      binary surfaced two real problems: `draw_connections` rendered with a
      plain `render_widget(List::new(items), ...)` and no `ListState`, so
      ratatui always drew from the top and clipped anything past the
      panel's height — moving the cursor past the visible rows highlighted
      a row you couldn't see; and `--version`'s feature list never checked
      `traceroute`, an omission from chunk 17. Both fixed
      (`ui/widgets.rs`'s `draw_connections` now uses
      `render_stateful_widget` with a `ListState` selecting the cursor
      index every frame; `cli.rs::version_text` now checks
      `cfg!(feature = "traceroute")` too). With scrolling fixed, the full
      connections list became visible — long, because one real process was
      opening many concurrent connections to the same host (an HTTP/2
      connection pool). Not literal duplicate rows (checked: zero exact
      `protocol+local+remote` duplicates in a live 62-connection snapshot)
      but visually repetitive enough to ask for a collapsible process tree.
      Two design questions were resolved with the user via
      `AskUserQuestion` before writing code: group by real OS parent/child
      relationship (not name-matching), and checking a collapsed group's
      checkbox bulk-checks its entire subtree.

      Implementation: `Connection` gained a `ppid: Option<u32>` field
      (`sample.rs`), populated by a new `read_parent_pid` in
      `collectors/connections.rs` reading `proc/<pid>/status`'s `PPid:`
      line — same fail-soft story and same trust tier as the existing
      `pid`/`program` reads, no new privilege. `ui/app.rs` gained the tree
      machinery: `ProcNode`/`build_forest` groups connections by pid and
      links a pid to its parent only when that parent *also* owns a
      filtered connection (an internet-connected process whose parent
      isn't also internet-connected becomes its own root, not nested under
      a synthetic placeholder); `ConnRow`/`flatten_forest` produces the
      actual display/cursor row list, with `checked`/`collapsed` computed
      once there rather than re-derived by `draw_connections` (which now
      only renders what it's given). `connections_cursor` now indexes into
      this flattened row list instead of the raw connection array;
      `toggle_checked` branches on the cursor row — a `Connection` row
      toggles one `ConnKey` as before, a `Process` row does an
      all-or-nothing bulk toggle over its whole subtree (via new
      `subtree_indices`/`find_node` helpers), regardless of any nested
      process's own collapse state. New `Left`/`Right` keybindings
      (previously-unbound `KeyPress` variants) collapse/expand the cursor
      row's process group. `resolve_checked` needed no changes at all — it
      already just filters by `connections_checked.contains(...)`, which
      works identically no matter how those keys got checked.

      Tests: `read_parent_pid` unit tests (happy path, missing file,
      malformed value, missing field) plus updated `collect()` end-to-end
      fixtures proving `ppid` round-trips through the real collector; a
      dedicated `ui::app` test section covering flat unattributed rows,
      single-process grouping, real parent/child nesting, "parent doesn't
      own a connection so child becomes its own root", collapse hiding a
      whole subtree, expand restoring it, Left/Right being no-ops on a
      `Connection` row, bulk check/uncheck over a subtree, and — the one
      genuinely tricky case — a synthetic two-pid `ppid` cycle proven to
      terminate with both connections still visible rather than hanging or
      silently dropping data. `ui::widgets` tests rebuilt around
      hand-constructed `ConnRow` fixtures (since `draw_connections` no
      longer builds the tree itself) covering process-header rendering,
      collapse markers, nested vs. top-level connection rows, and every
      existing checkbox/domain/route rendering test carried forward.
      390 unit tests + 8 integration tests in the default build (up from
      374 + 8 after chunk 17), 379 unit tests with `tui` and no
      `traceroute`, 282 unit tests with `--no-default-features` — all
      three configurations clean under `cargo clippy --all-targets` too.

      Real-hardware verification: `rustmon`'s own `ppid` output for this
      machine's live connections was cross-checked against
      `ps -eo pid,ppid,comm` and matched exactly for every attributed pid.
      A real pty session (this time reconstructed through `pyte`'s screen
      emulator rather than raw substring search on ANSI-interleaved output
      — a naive `"text" in raw_bytes` check false-negatived on strings
      ratatui had legitimately split across cursor-positioning escape
      sequences) confirmed: the tree renders grouped correctly; `Left`
      collapses a process row and its connections disappear from view;
      `Right` restores them; `x` on a collapsed header checks every
      connection in its subtree even though none of them are currently
      drawn; `Enter` then resolves all of them, and — a nice confirmation
      that this composes correctly with chunk 15's existing per-IP
      enrichment cache — connections in *other*, unchecked groups sharing
      the same remote IP picked up the resolved domain too, with no extra
      code needed for that to happen. Clean exit and terminal restore
      confirmed. Route tracing still correctly showed `unavailable` (no
      `CAP_NET_RAW` granted to this build).

      Decisions worth knowing about:

      1. **Only a direct parent/child link nests a process, not the full
         OS ancestry chain.** The connections collector only ever walks
         `/proc/<pid>` for processes that themselves own a filtered
         connection (established back in chunk 14, for cost reasons) — so
         if an internet-connected process's immediate parent isn't itself
         internet-connected, there's no real ancestor in this dataset to
         nest it under, and it becomes its own root rather than nesting
         under a synthetic placeholder pid the user never asked to see.
         Verified this is the common case on this actual machine right
         now: several real parent/child pairs exist, but at snapshot time
         none of the immediate parents happened to hold a socket
         themselves, so real nesting is currently proven only by the
         synthetic unit test fixtures, not by this session's live pty
         capture — worth knowing if a future session wants to eyeball real
         nesting, not a gap in the feature itself.
      2. **Cycle safety was designed in, not bolted on after a hang.**
         Nothing about a real OS process tree can cycle, but nothing
         guarantees the *two separate* `/proc` reads behind a connection's
         `pid` and `ppid` stay consistent with each other (pid reuse
         mid-refresh, in principle) — `build_forest` tracks a `visited`
         set during its top-down walk and appends any never-reached pid as
         its own extra root afterward, so a cycle can neither hang the tree
         build nor silently drop a connection from the view. Covered by a
         dedicated test constructing a real two-pid mutual cycle.
      3. **The connections-panel tree is rebuilt from scratch on every
         cursor move, checkbox toggle, and collapse toggle — not cached.**
         At most a few hundred connections and a couple dozen distinct
         pids, cheap enough that caching would have been premature
         complexity, and rebuilding-on-demand means the row list, cursor
         position, and checkbox/collapse state can never drift out of sync
         with each other, which a cached-and-invalidated version would have
         had to get right by construction instead.
      4. **`draw_connections` no longer builds any tree structure itself**
         — it takes a pre-flattened `&[ConnRow]` (with `checked`/
         `collapsed` already resolved) and only renders. This is a bigger
         signature change than it looks (dropped the `checked:
         &HashSet<ConnKey>` parameter entirely), but keeps the
         "collectors/app resolve, widgets draw" split this crate has used
         since chunk 9 intact rather than letting rendering code start
         making structural decisions.

---

## Second phase, continued: generalized panel scrolling

- [x] **Chunk 19: generalize scrolling to every list/table panel.** Real
      usage against the built binary (post-chunk-18) surfaced the exact
      same missing-`ListState` bug in two more panels: CPU (a 24-thread
      machine, only 8 `core*` rows ever visible) and Thermal (a 7-chip
      machine, the last chip's sensors cut off entirely). Asked whether to
      patch just those two, the answer was "isn't there an abstraction to
      be made... do it once, fix everywhere" — so this chunk generalizes
      chunk 18's Connections-only cursor mechanism into one piece of
      `App` state and one pair of render helpers, applied to every panel
      whose content can in principle overflow its height: CPU, Thermal,
      GPU (`List`-based) and Disk's device table / Net's interface table
      (`Table`-based, confirmed `TableState` has the identical
      `select`-drives-auto-scroll shape as `ListState` by reading
      `ratatui-widgets-0.3.2`'s source directly).

      Implementation: `App.connections_cursor: usize` became
      `App.panel_cursor: HashMap<Panel, usize>` (`Panel` gained `Hash`),
      with `cursor(panel)`/`scrollable_len(panel)`/`move_cursor()`/
      `clamp_cursor(panel)` replacing the Connections-only versions.
      `on_key`'s `Up`/`Down` became unconditional (previously gated to
      `Panel::Connections`) — naturally inert on `Overview`/`Memory` since
      `scrollable_len` returns `0` there, no special-casing needed.
      `refresh()` now clamps every panel's cursor, not just the focused
      one, so a panel whose data shrank in the background never shows a
      stale out-of-range cursor once it's tabbed to. Thermal and GPU
      needed more than a `.len()` on their snapshot data — a chip expands
      into a header line plus one line per temp/fan, a GPU into a header
      plus conditional vram/temp/freq lines — so rather than duplicate
      that expansion logic between `App` (for the count) and `widgets.rs`
      (for the content), it was extracted into `ThermalRow`/`thermal_rows`
      and `GpuRow`/`gpu_rows` in `ui/app.rs`, the exact pattern chunk 18
      already established for `ConnRow`/`connections_rows` — both the
      length check and the renderer now call the same function, so they
      can't drift apart. CPU/Disk/Net stayed simple `.len()` checks (their
      data is already flat, one record per row). Two shared render
      helpers, `render_stateful_list`/`render_stateful_table` in
      `ui/widgets.rs`, replaced five separate inline `List`/`Table`
      constructions (including refactoring `draw_connections` to use the
      shared helper too, removing chunk 18's own duplicate `ListState`
      code). Every scrollable row now gets the same `REVERSED` cursor
      highlight Connections already used — a scroll with no visible
      indicator of position would be confusing.

      **Explicitly out of scope**: Disk's second sub-section (the
      mount-point list) renders through a `Paragraph`, which has no
      selection-based auto-scroll primitive — only a manual `.scroll`
      offset with no built-in clamping. Folding that in would need a
      genuinely different, hand-rolled mechanism, not a reuse of what this
      chunk built; mount counts are typically small enough that this
      hasn't been a real problem in practice. Flagged in the man page,
      README, and here, rather than silently building a third bespoke
      scrolling mechanism to close a gap nobody had hit.

      Tests: `every_scrollable_panel_wraps_against_its_own_real_row_count`
      (CPU/Thermal/Disk/Net/GPU each wrap correctly against their *real*
      row count — Thermal/GPU specifically prove `scrollable_len` is
      wired to `thermal_rows`/`gpu_rows`, not a naive chip/GPU count),
      `overview_and_memory_have_nothing_to_scroll`,
      `cursor_movement_only_affects_the_focused_panels_own_cursor`,
      `clamp_cursor_generalizes_to_every_panel_not_just_connections`,
      `refresh_clamps_every_panels_cursor_not_just_the_focused_one`, plus
      two `ui::widgets` tests that prove the actual bug is fixed rather
      than just that the code compiles: a CPU snapshot with 20 cores and a
      Thermal snapshot with 10 chips, each rendered into a small area
      first with the cursor at `0` (asserting the last core/chip is
      genuinely absent from the rendered text) and then with the cursor at
      the last row (asserting it's now present). 396 unit tests + 8
      integration tests in the default build (up from 390 + 8 after chunk
      18), 385 unit tests with `tui` and no `traceroute`, 282 unit tests
      with `--no-default-features` — all three clean under
      `cargo clippy --all-targets` too.

      Real-hardware verification: a real pty session (the `pyte`-based
      technique from chunk 18) against the actual built binary confirmed
      the exact reported bug is fixed live, not just in unit tests — 30
      `Down` presses on the CPU panel wrapped the cursor to `core6`
      (`30 mod 24`) with the visible window scrolled to keep it in frame,
      and scrolling the Thermal panel past the first six chips revealed
      the seventh (`amdgpu`)'s sensors (`junction`/`mem`/`fan1`) that were
      completely invisible before this chunk. The same session confirmed
      chunk 18's Connections tree/checkbox/collapse behavior is completely
      unaffected by this refactor (grouping, bulk-check, collapse/expand
      all still worked identically).

      Decisions worth knowing about:

      1. **Thermal/GPU row-building moved into `ui/app.rs`, not
         `ui/widgets.rs`.** The row *count* (needed by `App` for cursor
         bounds) and the row *content* (needed by `widgets.rs` for
         rendering) have to agree exactly, or the cursor could point at a
         row that isn't actually where the highlight lands. Rather than
         have `App` duplicate `widgets.rs`'s line-building logic (a real
         drift risk — a future change to how a chip renders would have to
         remember to update two places identically), both now call the
         same `thermal_rows`/`gpu_rows` functions, living next to `ConnRow`
         in `app.rs` per this crate's established "collectors/app resolve
         structure, widgets only render" split.
      2. **`panel_cursor` is a single `HashMap<Panel, usize>` covering all
         eight panels, not per-panel fields.** `Connections`'s entry in
         that map does everything `connections_cursor` used to; the tree/
         checkbox/collapse logic built on top of it in chunk 18 needed no
         redesign at all, only a search-and-replace from
         `self.connections_cursor` to `self.cursor(Panel::Connections)`.
      3. **The mount-list `Paragraph` gap is a known, permanent-for-now
         limitation, not an oversight** — see "Explicitly out of scope"
         above. Worth revisiting only if someone actually hits a machine
         with enough mounts to overflow the panel, which hasn't happened
         yet on any machine this crate has been run against.

---

## Cross-cutting rules for every chunk

- No `unwrap`/`expect`/panic in library code — the security model depends on it.
- Every collector must degrade to "absent" on a missing or unreadable file.
- Every parser gets a malformed-input test, not just a happy-path test.
- `cargo build --no-default-features` must stay working and dependency-free.
- Run `cargo test -p rustmon` yourself and see it green before ticking a box.
