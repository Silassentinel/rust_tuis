# Red-team verification pass — 2026-09-05

Independent re-run of every RT-* finding's original exploit against the current
tree (after commit `886a685` "Harden rustlogger and rustlogger-mcp-server per
security review"). This is verification only; `.security/findings.md` was not
edited. rustlogger core built at `target/debug/rustlogger`; MCP server built to
`rustlogger-mcp-server/dist/` and driven through its compiled modules.

Snyk MCP tools are still gated in this session (`snyk_sca_scan` →
"folder '…/rust_tuis' is not trusted. Please run 'snyk_trust' first"); no
`snyk_trust`/`snyk_auth` tool is exposed, so no automated SAST/SCA ran — same
status the two prior passes recorded.

## Results

| ID | Claimed status | Verified result | Evidence (command + actual output) |
|----|----------------|-----------------|-------------------------------------|
| RT-core-01 | closed | PASS | `rustlogger sh -c 'echo AWS_SECRET…'`; `ls -l *.log` → `-rw------- … rustlogger-20260905-100040.log` (mode 0600, not world-readable). |
| RT-core-02 | closed | PASS | Pre-planted 6 symlinks at predicted names → victim; `rustlogger --log-dir shared …`. Victim file unchanged (`MY IMPORTANT VICTIM FILE`), real log written to disambiguated `rustlogger-20260905-100049-170729-1.log`; symlinks left untouched. |
| RT-core-03 | closed | PASS | In-session `ls -l /proc/self/fd` shows `3 -> /proc/<pid>/fd` (a transient, not the pty master); `cat <&3` → `Bad file descriptor`. No writable pty master inherited → no keystroke-injection primitive. |
| RT-core-04 | closed | PASS | Same session tried `( cat <&3 ) &` then `echo SECRET-EXFIL-abcdef123456`; log contains the SECRET line (audit intact). fd-3 steal fails because master is O_CLOEXEC. |
| RT-core-05 | closed | PASS | `rustlogger sh -c 'trap "" HUP; echo READY; while :; do sleep 1; done'`, then `kill -TERM` the rustlogger. It exited after ~3s (SIGHUP grace → SIGTERM) and wrote footer `reason: caught signal SIGTERM / exit code: (none)`. No hang. |
| RT-core-06 | accepted | CONFIRMED-ACCEPTED | `printf` OSC/CSI/`\r`/forged-footer payload lands byte-exact in the log (`cat -v` shows `^[]0;… ^[[2J … ^M`), as designed. Compensating controls both verified: README:83-84 / HOWTO:78-174 now tell reviewers to use `cat -v`/plain `less` (not `cat`/`less -R`), and the MCP sanitize layer strips the same escapes (see RT-mcp-02). |
| RT-core-07 | closed | PASS | Two `rustlogger` sessions started in the same second → two distinct files (`…100058.log` + `…100058-170761-1.log`); both transcripts intact (SESSION-ONE-DATA and SESSION-TWO-DATA each preserved). |
| RT-core-08 | closed | PASS | `trigger_dos.py` drove rustlogger under a pty running `stty raw; cat`, sent 400 MiB newline-free: `before: VmRSS 2524 kB` → `after: VmRSS 2524 kB` (flat; 512-byte cap holds). |
| RT-core-09 | accepted | CONFIRMED-ACCEPTED | Interactive session, ran `cat > notes.txt`, typed `stoplogger\n` as data → session ended with footer `reason: stoplogger command`. Behavior unchanged; documented as "known limitation 1" in stop_trigger.rs:10-22. |
| RT-core-10 | open | CONFIRMED-OPEN | `grep name/version Cargo.lock`: `portable-pty 0.9.0`, `shared_library 0.1.9` (RUSTSEC-2020-0128), `winapi 0.3.9`, `winreg 0.10.1`, second `nix 0.28.0` all still present. crate-checklist.md has no entry. Untouched, as stated. |
| RT-mcp-01 | mitigated | PASS (both halves) | (a) env unset: `startTracking({command:"bash",args:["-c","id > PWNED"]})` → PWNED file created with `uid=1000(...)` — original exploit still works by design. (b) `RUSTLOGGER_MCP_ALLOWED_COMMANDS=npm,git`: same call throws `command "bash" is not permitted…`, no PWNED file. |
| RT-mcp-02 | closed | PASS | Real path: tracked `sh -c printf` emits OSC/CSI/`\r`/forged footer/injection; `getLog(id,0)` → returned text has raw ESC/BEL stripped (`includes(0x1b)=false`), forged footer lines tagged `[forged-marker]`, `\r` split to a visible line, wrapped in BEGIN/END UNTRUSTED boundary. |
| RT-mcp-03 | closed | PASS | Spawned a real `sleep 30` bystander; `isSameProcess(pid, correctStart)=true`, `isSameProcess(pid, bogusStart)=false`; `stopTracking` on a session record with bogus `pidStartTime` returned without signaling — bystander still alive afterward. |
| RT-mcp-04 | closed | PASS | 200 MB single-line file; instrumented `fs.readSync` → `tailFile(f,1,25000)` read exactly 25000 bytes from disk, RSS delta 0.0 MB. No whole-file read. |
| RT-mcp-05 | mitigated | PASS (both halves) | (a) `RUSTLOGGER_MCP_ALLOWED_CWD_ROOTS` unset: `/tmp` and `/` both allowed (original behavior). (b) set to `allowed/`: root+subdir allowed; sibling `outside/` blocked; **symlink `allowed/escape → outside/` blocked** via realpath; prefix-trick `allowed-evil` blocked. |
| RT-mcp-06 | closed | PASS | 8 concurrent `startTracking` via `Promise.all` → all 8 in state.json, `started-but-lost (clobbered): 0`. withStateLock serializes the read-modify-write. |

## Notes where result nuances the claimed status

- **RT-mcp-01 and RT-mcp-05 are "mitigated", not "fixed", and the mitigation is
  opt-in and OFF by default.** Under the default configuration (no env vars set)
  both original exploits work exactly as originally reported: any MCP client /
  prompt-injected agent can run an arbitrary binary with arbitrary args
  (`bash -c 'id > …'` confirmed executing) in any writable directory. The
  allowlist / cwd-confinement only exist if an operator sets
  `RUSTLOGGER_MCP_ALLOWED_COMMANDS` / `RUSTLOGGER_MCP_ALLOWED_CWD_ROOTS`. This
  matches the documented design decision, but it means the high-severity RCE
  exposure of RT-mcp-01 is unchanged for anyone running the server as shipped.

- **RT-core-06 / RT-core-09 are genuine accepted-risk items, not silently
  dropped.** Both are documented in code and user docs, and RT-core-06's stated
  compensating control (MCP sanitize) was independently confirmed to strip the
  escapes and tag the forged footers. Residual for RT-core-06: sanitize cannot
  remove *meaning* — a plain-English injected sentence still reaches the model
  as (framed, untrusted) text; that is explicitly acknowledged in sanitize.ts.

- **RT-core-10 remains genuinely open and untouched**, affecting only the
  never-built Windows path; no regression, no silent change.

## Totals

- PASS: 12
- CONFIRMED-ACCEPTED: 2 (RT-core-06, RT-core-09)
- CONFIRMED-OPEN: 1 (RT-core-10)
- FAIL: 0

No finding's claimed status was contradicted. Every "closed" item's original
exploit no longer works; every "mitigated"/"accepted"/"open" item behaves
exactly as its status claims.
