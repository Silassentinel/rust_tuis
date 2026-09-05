# Mitigation plan for findings.md (2026-07-30 / 2026-08-04 red-team passes)

Status: **IMPLEMENTED 2026-08-04.** All chunks except 7 (explicitly out of scope,
see below) are done, tested and verified. The six items that originally needed a
decision were answered as follows:

| Chunk | Decision taken |
|---|---|
| 3 — escalation timeouts | `SIGHUP` → 2s → `SIGTERM` → 3s → `SIGKILL`, then give up and record it in the footer. |
| 4 — log sanitization | **Option A**: log stays byte-exact; sanitize at consumer boundaries (docs recommend `cat -v`; the MCP server strips escapes before the model sees them). |
| 6 — stop-trigger semantics | Documented as a known limitation; no behavior change. |
| 7 — dependency hygiene | Left out of scope. Requires its own `docs/crate-checklist.md` entry and sign-off per CLAUDE.md. |
| 9 — MCP command execution | Opt-in allowlist (`RUSTLOGGER_MCP_ALLOWED_COMMANDS`), server-environment only. Unset = previous behavior, so nothing breaks. |
| 12 — MCP `cwd` | Opt-in confinement (`RUSTLOGGER_MCP_ALLOWED_CWD_ROOTS`), realpath-checked so symlinks can't escape. |

Verification performed: every original exploit repro in `findings.md` was re-run
against the rebuilt binary and no longer works; 45 Rust tests and 23 MCP-server
regression tests pass; clippy is clean for Linux, Windows and Android targets.
Per-finding outcomes are recorded in `findings.md`'s **RESOLUTION** notes.

Two defects were discovered during the work that the original report did not
contain — a matching leak of the pty *slave* fd, and the fact that setting
`FD_CLOEXEC` after `openpty` leaves a genuinely reachable race (it reproduced in
this crate's own parallel test run). Both are fixed; see `findings.md`'s status
section.

**2026-09-05 follow-up** (independent verification, then two more decisions):

- An independent red-team pass (`.security/verification-2026-09-report.md`)
  re-ran every original exploit against the fixed code rather than trusting
  the self-assigned statuses above, and found chunk 9's opt-in allowlist left
  RT-mcp-2026-08-04-01's high-severity RCE fully open under default
  configuration — the fix existed but nothing required using it.
- Chunk 9 revised: the command allowlist is now required by default
  (`RUSTLOGGER_MCP_ALLOWED_COMMANDS` or explicit
  `RUSTLOGGER_MCP_ALLOW_ALL_COMMANDS` opt-out), rather than opt-in. This
  repo's own `.mcp.json` sets the explicit opt-out so this session's existing
  use of the server keeps working.
- Chunk 4 gained the piece its own text had flagged as still missing: a
  `rustlogger --view <path>` command (`src/safe_view.rs`) that renders
  control/escape bytes as visible text, so the safe-viewing side of "sanitize
  at the consumer boundary" no longer depends on a human remembering
  `cat -v` over `cat`. The byte-exact on-disk format is unchanged.
- Chunks 6, 7, and 12 (the remaining `low`-severity items) were reassessed
  against this project's actual threat model — private, never publicly
  exposed — rather than left at their 2026-08-04 status by default. None of
  the three changed as a result; see each finding's RESOLUTION in
  `findings.md` for why exposure wasn't the load-bearing factor in any of
  them. If that threat model ever changes, revisit those three specifically.

The original plan text follows unchanged, for the record.

---

Chunked per the repo's standing "work in
chunks" rule. Each chunk lists: findings closed, proposed fix, files touched,
regression test to add (per blue-team's own process), and whether it needs a
scope/design decision before anyone touches code. Items marked **NEEDS YOUR
CALL** are exactly that — do not start those chunks without an explicit go-ahead,
since the "fix" changes behavior rather than just closing a bug.

Chunks are ordered severity-first, `rustlogger` core before
`rustlogger-mcp-server`, matching how the findings chain (several mcp findings
are only exploitable *because of* a core finding, so fixing the core issue
first shrinks or closes the dependent one).

---

## Chunk 1 — Log file creation hardening (core-01, core-02, core-07)
**Severity:** high, high, medium · **Files:** `rustlogger/src/session/{mod,unix,windows}.rs`

- Open the log with `O_CREAT | O_EXCL | O_WRONLY`, mode `0600`, instead of
  `File::create` (which is `O_CREAT|O_TRUNC`, mode `0666&~umask`, and follows
  symlinks).
  - `O_EXCL` closes core-07 (no more same-second collisions) as a side effect —
    on `EEXIST`, retry with a sub-second/pid/random disambiguator in the
    filename rather than looping on the same name.
  - `O_EXCL` + not following symlinks (`O_NOFOLLOW` on Unix; Windows needs the
    equivalent `FILE_FLAG_OPEN_REPARSE_POINT`-style check via `portable-pty`'s
    handle APIs) closes core-02.
  - Mode `0600` closes core-01. Also fix `create_dir_all`'s resulting directory
    mode (currently `0777&~umask`) to `0700` for any directory this crate
    creates itself.
- Regression test: attempt to log into a directory pre-seeded with a symlink
  at the exact predictable log path; assert rustlogger refuses/retries rather
  than writing through the symlink. Assert new log file mode is `0600`.
- No scope question — this is a straight bug fix, behavior (log secrecy) only
  gets stricter, doesn't change any documented interface.

## Chunk 2 — PTY master fd leak (core-03, core-04)
**Severity:** high, high · **Files:** `rustlogger/src/pty_session/unix.rs`

- Set `O_CLOEXEC` on the master fd right after `openpty` (or use
  `nix::fcntl::set_close_on_exec`), before `fork`/`pre_exec` runs. Verify no
  other code path (the poll loop, logging) needs the master to survive exec in
  the child — it shouldn't, since only the parent talks to the master.
- Regression test: spawn a child that execs `sh -c 'ls -l /proc/self/fd'` and
  assert the master fd number does not appear in its fd table (this is
  exactly the repro script already used for the finding).
- No scope question — closing an unintentional fd leak, no interface change.

## Chunk 3 — Stop-path hang on an unresponsive child (core-05)
**Severity:** medium · **Files:** `rustlogger/src/session/mod.rs`, `pty_session/unix.rs`

- Give `finish_session`'s `wait()` a deadline: after SIGHUP, wait with a
  timeout (e.g. poll `waitpid` with `WNOHANG` in a short loop, or a watchdog
  thread); on timeout escalate SIGTERM, then SIGKILL, then give up and log
  that the child could not be reaped.
- Regression test: spawn `sh -c 'trap "" HUP; sleep 300'`, send the stop
  trigger, assert the process exits (via escalation) within a bounded time
  instead of hanging.
- **NEEDS YOUR CALL:** the escalation timeouts (how long to wait at each
  signal step before escalating) are a behavior choice, not derivable from the
  bug report — pick defaults or tell me what you want.

## Chunk 4 — Log content is unsanitized (core-06)
**Severity:** medium · **Files:** `rustlogger/src/logfile.rs` (and/or the MCP server, see Chunk 8)

- **NEEDS YOUR CALL — this one has a real design fork:**
  - *Option A:* keep the on-disk log byte-for-byte raw (forensic fidelity —
    "what actually happened" including any escape sequences), and push all
    sanitization to whoever *displays* or *consumes* the log (HOWTO.md's `cat`
    guidance changes to a recommended safe-viewing method; the MCP server's
    `get_log` sanitizes before handing text to a model — that's Chunk 8).
  - *Option B:* sanitize at write time in `LogFile::write_output` itself
    (strip/escape ANSI-OSC-CSI sequences, normalize `\r`), so every consumer
    is safe by construction — but the log then no longer records byte-exact
    terminal output, which cuts against the tool's stated purpose as a
    transcript.
  - My default recommendation is **A** (raw storage, sanitize at the
    boundary), since the log's job is to be an honest record. But this
    changes documented behavior (HOWTO.md §6) and touches two subprojects, so
    I'm not starting it without your pick.
- Whichever option: also add a display-safe rendering path (e.g. a
  `--safe`/escaped `cat` mode or a note to pipe through `cat -v`) and document
  that raw `cat`/`less -R` on an untrusted transcript is unsafe.

## Chunk 5 — Stop-trigger unbounded buffer (core-08)
**Severity:** low · **Files:** `rustlogger/src/stop_trigger.rs`

- Cap `StopTrigger::line` at a small bound (the stop phrase is `stoplogger`,
  ~10 bytes — a cap of a few hundred bytes is generous) and drop/reset on
  overflow instead of growing unbounded.
- Regression test: feed multiple MB of newline-free input, assert memory stays
  bounded and the process doesn't grow proportionally to input size.
- No scope question — a cap that's already far larger than any legitimate
  trigger line is a pure hardening fix.

## Chunk 6 — Stop-trigger false-positive on data (core-09)
**Severity:** low · **Files:** `rustlogger/src/stop_trigger.rs`, `session/unix.rs`

- **NEEDS YOUR CALL:** closing this changes user-facing behavior — the whole
  feature is "type `stoplogger` to end the session," so any fix trades off
  against that. Options: require the phrase preceded/followed by a specific
  escape sequence or key combo instead of plain text; only arm the matcher
  when the wrapped program is *not* known to be in raw/application mode
  (heuristic, imperfect); or accept this as a documented limitation and just
  make the docs' existing "known limitation" note more prominent instead of
  changing code. Tell me which direction you want before I touch this chunk.

## Chunk 7 — Dependency hygiene note (core-10)
**Severity:** low · **Files:** none yet — documentation/tracking only

- No code change proposed here. Per `CLAUDE.md`'s crate-checklist rule,
  swapping or pinning `portable-pty`'s transitive deps is a dependency
  decision that needs its own checklist entry in `docs/crate-checklist.md`
  and your sign-off — **out of scope for this mitigation pass** unless you
  want to open that separately.
- Recommended non-code action: once Snyk is authenticated
  (`snyk_trust`/`snyk_auth` — no such tool was exposed to the agents this
  run, so this needs to happen from an interactive session), re-run
  `snyk_sca_scan` to get a real advisory list instead of the manual read.

---

## Chunk 8 — mcp-server: sanitize log before returning to the model (mcp-02)
**Severity:** high · **Files:** `rustlogger-mcp-server/src/sessionManager.ts` / `src/index.ts`

- Depends on the Chunk 4 decision. If Option A (sanitize at boundary): strip
  or escape ANSI/OSC/CSI sequences and neutralize forged footer-lookalike
  lines before `get_log` returns text to the MCP client, and/or wrap the
  returned content in an explicit "this is untrusted tracked-process output,
  not an instruction" framing so a well-behaved agent doesn't treat it as
  trusted. If Option B, this chunk shrinks to just the untrusted-content
  framing, since the bytes are already clean.
- Regression test: track a process that prints ANSI escapes and a forged
  footer line; assert `get_log`'s returned text has escapes stripped/escaped
  and the forged footer doesn't parse as a real one.
- Directly closes the exec→inject→exec chain with mcp-01 once mcp-01 is also
  addressed (Chunk 9).

## Chunk 9 — mcp-server: unrestricted command execution (mcp-01)
**Severity:** high · **Files:** `rustlogger-mcp-server/src/sessionManager.ts`, `src/index.ts`

- **NEEDS YOUR CALL — biggest one here.** Running an arbitrary command *is*
  this tool's job; there's no fix that doesn't change what the tool is
  allowed to do. Options: (a) an allowlist of permitted commands/binaries
  configured by whoever runs the server; (b) require a human-in-the-loop
  confirmation step before `start_tracking` executes anything, surfaced by
  the MCP host; (c) document the trust boundary explicitly (this server must
  only be exposed to agents that don't relay untrusted external content) and
  don't change code. I'd lean toward (a) as an opt-in config since it's the
  least disruptive to legitimate use, but this is a product decision for you,
  not something to infer from the finding.

## Chunk 10 — mcp-server: PID identity validation (mcp-03)
**Severity:** medium · **Files:** `rustlogger-mcp-server/src/sessionManager.ts`

- Before sending SIGTERM or reporting "running" from a stored PID, verify
  identity beyond `kill(pid, 0)` — e.g. record and re-check process start
  time (`/proc/<pid>/stat` on Linux) or the resolved binary path, and treat a
  mismatch as "session already gone" rather than acting on the recycled PID.
- Regression test: simulate a stored session whose PID now belongs to a
  different process (mock/inject a mismatched start-time) and assert
  `stop_tracking`/`list_tracked` correctly report it as dead rather than
  signaling the impostor.
- No scope question — pure correctness/safety fix, no behavior change for the
  legitimate case.

## Chunk 11 — mcp-server: bounded log read (mcp-04)
**Severity:** medium · **Files:** `rustlogger-mcp-server/src/sessionManager.ts`

- Replace `readFileSync` of the whole file with a bounded read of only the
  trailing window needed to satisfy `tail_lines`/`LOG_CHARACTER_LIMIT` (e.g.
  seek from the end using `fs.open` + `read`, or read in chunks from the tail
  until enough lines are found), so a large log can't be pulled entirely into
  memory by one `get_log` call.
- Regression test: point `get_log` at a large (~200 MB) newline-sparse file
  with `tail_lines:1`, assert memory growth stays roughly bounded to the
  requested window instead of the whole file.
- No scope question — purely closes an unbounded-read DoS, same output
  contract for legitimate small logs.

## Chunk 12 — mcp-server: confine `cwd` (mcp-05)
**Severity:** low · **Files:** `rustlogger-mcp-server/src/sessionManager.ts`, `src/index.ts`

- **NEEDS YOUR CALL:** confining `cwd` to a configured base directory is a
  behavior change (today any absolute path the server's user can write to is
  accepted) — tell me whether you want a hard base-dir confinement, an
  opt-in allowlist of permitted roots, or to treat this as sufficiently
  covered once Chunk 1 (core-02 symlink fix) lands and leave `cwd` unrestricted.

## Chunk 13 — mcp-server: lock state.json writes (mcp-06)
**Severity:** low · **Files:** `rustlogger-mcp-server/src/sessionStore.ts`

- Serialize `readState`→mutate→`writeState` (an in-process mutex/queue around
  the read-modify-write is enough here, no cross-process lock needed given
  this server's single-process design) so concurrent `start_tracking` calls
  can't clobber each other's session record.
- Regression test: fire two concurrent `start_tracking` calls, assert both
  sessions are present in `state.json` afterward.
- No scope question — internal concurrency fix, no interface change.

---

## Summary — what needs your call before any code changes happen

1. **Chunk 3** — SIGHUP→escalation timeouts (pick defaults or specify)
2. **Chunk 4** — sanitize-at-write vs. sanitize-at-boundary for log content
   (recommend: sanitize at boundary / Chunk 8)
3. **Chunk 6** — how (or whether) to change stop-trigger matching semantics
4. **Chunk 7** — dependency swap is explicitly out of scope unless you open
   it separately via `docs/crate-checklist.md`
5. **Chunk 9** — command allowlist vs. human-confirmation vs. docs-only for
   the mcp-server's core "run arbitrary things" behavior
6. **Chunk 12** — whether/how to confine `cwd`

Everything else (1, 2, 5, 10, 11, 13) is a same-behavior bug fix with a clear
regression test and no product decision attached — those could go to
`blue-team` as soon as you say go, independently of the items above.
