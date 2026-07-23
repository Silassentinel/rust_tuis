import os from "node:os";
import path from "node:path";

/** Where session bookkeeping (not the tracked commands' own logs, which
 * live in each session's own `cwd`) and the persisted session index are
 * kept. */
export const STATE_DIR = path.join(os.homedir(), ".rustlogger-mcp-server");
export const STATE_FILE = path.join(STATE_DIR, "state.json");
export const SESSIONS_DIR = path.join(STATE_DIR, "sessions");

/** Cap on how much log text a single tool response returns. */
export const LOG_CHARACTER_LIMIT = 25000;

/** Default number of trailing lines `rustlogger_get_log` returns. */
export const DEFAULT_TAIL_LINES = 200;
