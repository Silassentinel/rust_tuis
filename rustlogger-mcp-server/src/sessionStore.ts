import fs from "node:fs";

import { STATE_DIR, STATE_FILE } from "./constants.js";
import type { PersistedState, TrackedSession } from "./types.js";

/**
 * Session metadata persisted to disk (not just kept in memory) so that
 * `rustlogger_list_tracked` and `rustlogger_get_log` still work correctly
 * even if this MCP server process itself gets restarted - the tracked
 * rustlogger processes are spawned detached specifically so they outlive
 * it (see sessionManager.ts), and there's no point tracking a program in
 * the background if losing the server's memory also loses track of it.
 *
 * Mutations are serialized through `withStateLock` below. A plain
 * read-modify-write was a real bug, not just a theoretical one
 * (`.security/findings.md` RT-mcp-2026-08-04-06): two `start_tracking`
 * calls interleaving between their read and their write meant the second
 * `writeState` dropped the first's new session. The lost session's
 * rustlogger process is already spawned, detached and `unref`ed by then,
 * so it keeps running and logging while becoming impossible to list or
 * stop through the tools - silently losing the only handle to a live
 * detached process.
 *
 * An in-process lock is sufficient here and a cross-process file lock is
 * not needed: this state belongs to one server process, and the tracked
 * processes themselves never touch it.
 */

/**
 * Tail of the serialized mutation chain. Each `withStateLock` call appends
 * itself, so read-modify-write sections run strictly one at a time even
 * though the surrounding tool handlers are async and interleave freely.
 */
let stateLock: Promise<unknown> = Promise.resolve();

export function withStateLock<T>(mutation: () => T): Promise<T> {
  const result = stateLock.then(mutation, mutation);
  // Keep the chain alive regardless of whether this mutation threw, so one
  // failure doesn't wedge every later write.
  stateLock = result.catch(() => undefined);
  return result;
}

function readState(): PersistedState {
  try {
    const raw = fs.readFileSync(STATE_FILE, "utf8");
    return JSON.parse(raw) as PersistedState;
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code === "ENOENT") {
      return { sessions: {} };
    }
    throw error;
  }
}

function writeState(state: PersistedState): void {
  fs.mkdirSync(STATE_DIR, { recursive: true });
  fs.writeFileSync(STATE_FILE, JSON.stringify(state, null, 2), "utf8");
}

export function listSessions(): TrackedSession[] {
  return Object.values(readState().sessions);
}

export function getSession(id: string): TrackedSession | undefined {
  return readState().sessions[id];
}

export function saveSession(session: TrackedSession): void {
  const state = readState();
  state.sessions[session.id] = session;
  writeState(state);
}

export function updateSession(
  id: string,
  patch: Partial<TrackedSession>,
): TrackedSession | undefined {
  const state = readState();
  const existing = state.sessions[id];
  if (!existing) {
    return undefined;
  }
  const updated = { ...existing, ...patch };
  state.sessions[id] = updated;
  writeState(state);
  return updated;
}
