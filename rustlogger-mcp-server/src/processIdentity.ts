import fs from "node:fs";

/**
 * Distinguishing "the process we started" from "whatever holds that pid
 * now."
 *
 * `process.kill(pid, 0)` only answers "does *a* process with this pid
 * exist." That is not enough here (`.security/findings.md`
 * RT-mcp-2026-08-04-03): tracked processes are detached and long-lived,
 * session records persist to `state.json` across restarts of this server,
 * and Linux's default `pid_max` (~32768) makes pid reuse routine on a
 * long-running host. So once a tracked rustlogger exits, its pid can be
 * recycled - after which `list_tracked` would report a dead session as
 * "running", and `stop_tracking` would deliver SIGTERM to an unrelated
 * process that happens to hold that pid now.
 *
 * The fix is to pin an identity at spawn time and re-check it before
 * trusting the pid: the kernel's own process start time, which is
 * monotonic per boot and not reused along with the pid.
 */

/**
 * Reads field 22 (`starttime`) of `/proc/<pid>/stat` - the process's start
 * time in clock ticks since boot.
 *
 * Parsing note: field 2 (`comm`) is the executable name wrapped in
 * parentheses and *may itself contain spaces or parentheses*, so splitting
 * the whole line on whitespace is wrong. Everything after the final `)` is
 * unambiguous, which is what this uses.
 *
 * Returns `null` on any platform without procfs, or if the process is gone.
 */
export function readProcessStartTime(pid: number): string | null {
  if (process.platform !== "linux") {
    return null;
  }
  try {
    const stat = fs.readFileSync(`/proc/${pid}/stat`, "utf8");
    const afterComm = stat.slice(stat.lastIndexOf(")") + 1).trim();
    // afterComm begins at field 3 (state), so starttime (field 22) is
    // index 19 counting from there.
    const fields = afterComm.split(/\s+/);
    const startTime = fields[19];
    return startTime ?? null;
  } catch {
    return null;
  }
}

/**
 * Whether `pid` is alive *and* is still the same process that was recorded.
 *
 * `expectedStartTime` of `null` means no identity was captured (a
 * pre-existing session record from before this check existed, or a
 * non-Linux host). In that case this degrades to the old liveness-only
 * behavior rather than refusing to work - it's no worse than before, and
 * failing closed would break every session recorded by an older build.
 */
export function isSameProcess(pid: number, expectedStartTime: string | null): boolean {
  let alive: boolean;
  try {
    process.kill(pid, 0);
    alive = true;
  } catch (error) {
    // EPERM means it exists but belongs to another user - which, for our
    // purposes, already proves it is *not* the process we spawned.
    alive = (error as NodeJS.ErrnoException).code !== "ESRCH";
  }
  if (!alive) {
    return false;
  }

  if (expectedStartTime === null) {
    return true;
  }

  const actual = readProcessStartTime(pid);
  if (actual === null) {
    // The pid vanished between the liveness check and this read, or procfs
    // is unreadable. Treat as "not our process": for `stop_tracking` that
    // means declining to signal, which is the safe direction.
    return false;
  }
  return actual === expectedStartTime;
}
