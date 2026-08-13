import { randomUUID } from "node:crypto";
import { spawn } from "node:child_process";
import fs from "node:fs";
import path from "node:path";

import { DEFAULT_TAIL_LINES, LOG_CHARACTER_LIMIT, SESSIONS_DIR } from "./constants.js";
import { assertCommandAllowed, assertCwdAllowed } from "./policy.js";
import { isSameProcess, readProcessStartTime } from "./processIdentity.js";
import { resolveRustloggerBin } from "./rustloggerBin.js";
import { sanitizeLogForModel } from "./sanitize.js";
import {
  getSession,
  listSessions,
  saveSession,
  updateSession,
  withStateLock,
} from "./sessionStore.js";
import { tailFile } from "./tailFile.js";
import type { TrackedSession } from "./types.js";

/**
 * How tracked processes are made to outlive this MCP server: spawned
 * `detached` (own process group, so a signal delivered to this server's
 * process group doesn't also hit them), with stdout/stderr redirected to
 * real files rather than pipes. Pipes would need this server to keep
 * draining them forever (an OS pipe buffer fills and blocks the writer
 * once nobody reads it) and would sever if this server's own fds close
 * when it exits - a real file has neither problem, since disk writes
 * never block and the underlying file description stays open as long as
 * the tracked process holds its own copy, independent of this server.
 *
 * Note this redirects rustlogger's *own* stdout/stderr (which mirrors the
 * tracked command's output for convenience when run directly - see
 * rustlogger's README) into per-session bookkeeping files under
 * `SESSIONS_DIR`. That is separate from the tracked command's actual log
 * file, which rustlogger writes into the given `cwd` per its own
 * documented convention - `logPath` below refers to that one.
 */

const LOG_PATH_PATTERN = /rustlogger: tracking `.*`, logging to (\S+)/;

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

function safeReadFile(filePath: string): string {
  try {
    return fs.readFileSync(filePath, "utf8");
  } catch {
    return "";
  }
}

function extractLogPath(stderrText: string, cwd: string): string | null {
  const match = LOG_PATH_PATTERN.exec(stderrText);
  return match ? path.resolve(cwd, match[1]) : null;
}

async function waitForLogPath(
  stderrLogFile: string,
  cwd: string,
  timeoutMs: number,
): Promise<string | null> {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    const found = extractLogPath(safeReadFile(stderrLogFile), cwd);
    if (found) {
      return found;
    }
    await sleep(50);
  }
  return null;
}

/**
 * Whether a recorded session's process is still running *and* still the
 * same process. Takes the whole session rather than a bare pid so the
 * recorded identity is always consulted - a bare-pid check is exactly the
 * bug this replaces (see processIdentity.ts).
 */
export function isSessionAlive(session: TrackedSession): boolean {
  return isSameProcess(session.pid, session.pidStartTime ?? null);
}

export interface StartTrackingInput {
  command: string;
  args: string[];
  cwd: string;
  label?: string;
}

export async function startTracking(input: StartTrackingInput): Promise<TrackedSession> {
  // Policy first, before anything is created or spawned: a refused request
  // must leave no trace (no bookkeeping directory, no process).
  assertCommandAllowed(input.command);

  const cwdStat = fs.statSync(input.cwd); // throws clearly (ENOENT etc.) on a bad path
  if (!cwdStat.isDirectory()) {
    throw new Error(`cwd is not a directory: ${input.cwd}`);
  }
  // After the isDirectory check, so a nonexistent path still reports the
  // clearer ENOENT rather than a confusing policy error.
  assertCwdAllowed(input.cwd);

  const id = randomUUID();
  const bookkeepingDir = path.join(SESSIONS_DIR, id);
  fs.mkdirSync(bookkeepingDir, { recursive: true });
  const stdoutLogFile = path.join(bookkeepingDir, "rustlogger-stdout.log");
  const stderrLogFile = path.join(bookkeepingDir, "rustlogger-stderr.log");

  const bin = resolveRustloggerBin();
  const stdoutFd = fs.openSync(stdoutLogFile, "a");
  const stderrFd = fs.openSync(stderrLogFile, "a");
  const child = spawn(bin, [input.command, ...input.args], {
    cwd: input.cwd,
    detached: true,
    stdio: ["ignore", stdoutFd, stderrFd],
  });
  // The child received its own duped copies when spawned; these are ours.
  fs.closeSync(stdoutFd);
  fs.closeSync(stderrFd);

  const spawnError = await new Promise<Error | null>((resolve) => {
    child.once("error", resolve);
    setTimeout(() => resolve(null), 200);
  });
  if (spawnError) {
    throw new Error(`failed to start rustlogger ("${bin}"): ${spawnError.message}`);
  }

  const pid = child.pid;
  if (pid === undefined) {
    throw new Error(`failed to start rustlogger ("${bin}"): no pid was assigned`);
  }
  child.unref();

  // Captured as early as possible: this is what later distinguishes our
  // process from an unrelated one that inherits the pid after ours exits.
  const pidStartTime = readProcessStartTime(pid);

  const logPath = await waitForLogPath(stderrLogFile, input.cwd, 3000);
  if (logPath === null && !isSameProcess(pid, pidStartTime)) {
    const stderrText = safeReadFile(stderrLogFile).trim();
    throw new Error(
      `rustlogger exited immediately without announcing a log path.` +
        (stderrText ? ` stderr: ${stderrText}` : " (its stderr was empty)"),
    );
  }

  const session: TrackedSession = {
    id,
    command: input.command,
    args: input.args,
    cwd: input.cwd,
    label: input.label,
    pid,
    pidStartTime,
    startedAt: new Date().toISOString(),
    logPath,
    bookkeepingDir,
  };
  await withStateLock(() => saveSession(session));
  return session;
}

export type SessionStatus = "running" | "exited";

export interface GetLogResult {
  session: TrackedSession;
  status: SessionStatus;
  logPath: string | null;
  log: string;
  truncated: boolean;
}

export function getLog(sessionId: string, tailLines: number = DEFAULT_TAIL_LINES): GetLogResult {
  const session = requireSession(sessionId);

  let logPath = session.logPath;
  if (logPath === null) {
    const stderrLogFile = path.join(session.bookkeepingDir, "rustlogger-stderr.log");
    const recovered = extractLogPath(safeReadFile(stderrLogFile), session.cwd);
    if (recovered) {
      logPath = recovered;
      updateSession(sessionId, { logPath });
    }
  }

  const status: SessionStatus = isSessionAlive(session) ? "running" : "exited";

  if (logPath === null) {
    return { session, status, logPath: null, log: "", truncated: false };
  }

  // Bounded read: only the trailing window is pulled off disk, so a log
  // that has grown to gigabytes can't be loaded whole into this server's
  // memory by one call (see tailFile.ts).
  const { text, truncated: readTruncated } = tailFile(
    logPath,
    tailLines,
    LOG_CHARACTER_LIMIT,
  );

  // Sanitize at the boundary: the file itself stays byte-exact, but what
  // crosses into the model's context has escapes stripped and forged
  // rustlogger markers tagged (see sanitize.ts).
  const log = sanitizeLogForModel(text);

  return { session, status, logPath, log, truncated: readTruncated };
}

export function listTracked(): Array<TrackedSession & { status: SessionStatus }> {
  return listSessions().map((session) => ({
    ...session,
    status: isSessionAlive(session) ? "running" : "exited",
  }));
}

export interface StopTrackingResult {
  session: TrackedSession;
  stopped: boolean;
  alreadyExited: boolean;
}

export async function stopTracking(sessionId: string): Promise<StopTrackingResult> {
  const session = requireSession(sessionId);

  // Identity-checked, not just liveness-checked. If this pid has been
  // recycled to an unrelated process, the session is over as far as we're
  // concerned - and signaling that pid would kill a bystander.
  if (!isSessionAlive(session)) {
    return { session, stopped: false, alreadyExited: true };
  }

  process.kill(session.pid, "SIGTERM");

  const deadline = Date.now() + 2000;
  while (Date.now() < deadline) {
    if (!isSessionAlive(session)) {
      return { session, stopped: true, alreadyExited: false };
    }
    await sleep(50);
  }

  return { session, stopped: false, alreadyExited: false };
}

function requireSession(sessionId: string): TrackedSession {
  const session = getSession(sessionId);
  if (!session) {
    throw new Error(
      `no tracked session with id "${sessionId}" - use rustlogger_list_tracked to see known session ids`,
    );
  }
  return session;
}
