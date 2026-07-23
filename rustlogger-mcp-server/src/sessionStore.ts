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
 * Single-user, single-machine, low-frequency tool calls - a plain
 * read-modify-write on each mutation is all this needs; no file locking.
 */
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
