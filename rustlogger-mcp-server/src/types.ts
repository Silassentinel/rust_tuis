/** A program being tracked via rustlogger's headless mode. */
export interface TrackedSession {
  id: string;
  command: string;
  args: string[];
  /** Where the tracked command actually runs - and, per rustlogger's own
   * convention, where its log file lands too. */
  cwd: string;
  label?: string;
  pid: number;
  /** The kernel's start time for `pid`, captured at spawn, used to tell
   * this process apart from an unrelated one that later inherits the same
   * recycled pid (see processIdentity.ts). `null` on platforms without
   * procfs, and on session records written before this field existed. */
  pidStartTime?: string | null;
  startedAt: string;
  /** Absolute path to the rustlogger-*.log file, once known - briefly
   * absent immediately after spawning, until the startup line announcing
   * it has been read back (see sessionManager.ts). */
  logPath: string | null;
  /** Internal bookkeeping directory holding rustlogger's own redirected
   * stdout/stderr (not the tracked command's log - that's `logPath`,
   * living in `cwd` per rustlogger's own convention). Only used to
   * recover `logPath` if it wasn't captured at start time. */
  bookkeepingDir: string;
}

export interface PersistedState {
  sessions: Record<string, TrackedSession>;
}
