#!/usr/bin/env node
/**
 * MCP server for rustlogger's headless tracking mode: lets an agent start
 * a program running in the background, check its log later, list what's
 * currently tracked, and stop it - without blocking on long-running or
 * fire-and-forget commands. See rustlogger/docs/rustlogger-design.md's
 * chunk 7 notes for how headless mode itself works, and this project's
 * own README for the tools below.
 */

import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js";
import { z } from "zod";

import { DEFAULT_TAIL_LINES } from "./constants.js";
import {
  getLog,
  listTracked,
  startTracking,
  stopTracking,
} from "./sessionManager.js";

const server = new McpServer({
  name: "rustlogger-mcp-server",
  version: "1.0.0",
});

function errorResult(error: unknown) {
  return {
    isError: true,
    content: [
      {
        type: "text" as const,
        text: `Error: ${error instanceof Error ? error.message : String(error)}`,
      },
    ],
  };
}

const StartTrackingInputSchema = z
  .object({
    command: z.string().min(1).describe('The program to run, e.g. "npm" or "/usr/bin/python3".'),
    args: z
      .array(z.string())
      .default([])
      .describe('Arguments to pass to the command, e.g. ["run", "build"].'),
    cwd: z
      .string()
      .min(1)
      .describe(
        "Absolute path to the directory the command should run in. Required: this is also where the resulting rustlogger-*.log file lands, per rustlogger's own convention of logging into its launch directory.",
      ),
    label: z
      .string()
      .optional()
      .describe('Optional human-readable note to help recognize this session later, e.g. "frontend build".'),
  })
  .strict();

server.registerTool(
  "rustlogger_start_tracking",
  {
    title: "Start Tracking a Program",
    description: `Start a program running in the background under rustlogger's headless tracking mode, and begin logging everything it prints.

Use this instead of running a long or fire-and-forget command directly when progress or output will need checking later rather than blocking on it now. The program keeps running - and being logged - after this call returns, independent of this MCP server's own lifetime.

Args:
  - command (string): the program to run
  - args (string[]): its arguments (default: [])
  - cwd (string): absolute path to the directory it should run in - required, and also where its log file lands
  - label (string, optional): a human-readable note to help recognize this session later

Returns (JSON):
  {
    "session_id": string,      // pass to rustlogger_get_log / rustlogger_stop_tracking
    "command": string,
    "args": string[],
    "cwd": string,
    "label": string | null,
    "pid": number,             // the rustlogger process's own pid
    "started_at": string,      // ISO 8601
    "log_path": string | null  // absolute path; may briefly be null right after starting
  }

Examples:
  - Use when: "kick off the build and let me know when it's done" -> start_tracking, then check back later with rustlogger_get_log
  - Use when: "track this data import script" -> command="python3", args=["import_data.py"]
  - Don't use when: a quick command's output is needed right now and waiting is fine - just run it directly instead

Error Handling:
  - Throws if cwd doesn't exist or isn't a directory
  - Throws if rustlogger itself couldn't be found or failed to start (checked in order: $RUSTLOGGER_BIN, a sibling build in the rust_tuis repo, then $PATH)`,
    inputSchema: StartTrackingInputSchema,
    annotations: {
      readOnlyHint: false,
      destructiveHint: false,
      idempotentHint: false,
      openWorldHint: true,
    },
  },
  async (params) => {
    try {
      const session = await startTracking(params);
      const output = {
        session_id: session.id,
        command: session.command,
        args: session.args,
        cwd: session.cwd,
        label: session.label ?? null,
        pid: session.pid,
        started_at: session.startedAt,
        log_path: session.logPath,
      };
      const commandLine = [session.command, ...session.args].join(" ");
      const logNote = session.logPath
        ? `Logging to ${session.logPath}.`
        : "Log path not yet known - call rustlogger_get_log shortly to pick it up.";
      return {
        content: [
          {
            type: "text" as const,
            text: `Started tracking \`${commandLine}\` (session ${session.id}, pid ${session.pid}). ${logNote}`,
          },
        ],
        structuredContent: output,
      };
    } catch (error) {
      return errorResult(error);
    }
  },
);

const GetLogInputSchema = z
  .object({
    session_id: z
      .string()
      .min(1)
      .describe("The session id returned by rustlogger_start_tracking, or shown by rustlogger_list_tracked."),
    tail_lines: z
      .number()
      .int()
      .min(0)
      .max(5000)
      .default(DEFAULT_TAIL_LINES)
      .describe(
        `How many trailing lines of the log to return. 0 returns the whole file (subject to a response size cap). Default ${DEFAULT_TAIL_LINES}.`,
      ),
  })
  .strict();

server.registerTool(
  "rustlogger_get_log",
  {
    title: "Get a Tracked Program's Log",
    description: `Read back the log of a program started with rustlogger_start_tracking, along with whether it's still running.

Args:
  - session_id (string): which tracked session to read
  - tail_lines (number): how many trailing lines to return, 0 for the whole file (default ${DEFAULT_TAIL_LINES})

Returns (JSON):
  {
    "session_id": string,
    "status": "running" | "exited",
    "log_path": string | null,
    "log": string,           // the requested slice of the log's text
    "truncated": boolean     // true if tail_lines or the response size cap cut anything off
  }

Examples:
  - Use when: "how's the build going?" -> get_log with a small tail_lines to see recent output
  - Use when: "did the import script finish, and what did it print?" -> get_log, check status and log
  - Don't use when: the session id is unknown - call rustlogger_list_tracked first

Error Handling:
  - Throws "no tracked session with id ..." if session_id isn't recognized - call rustlogger_list_tracked to see valid ids`,
    inputSchema: GetLogInputSchema,
    annotations: {
      readOnlyHint: true,
      destructiveHint: false,
      idempotentHint: true,
      openWorldHint: true,
    },
  },
  async ({ session_id, tail_lines }) => {
    try {
      const result = getLog(session_id, tail_lines);
      const output = {
        session_id,
        status: result.status,
        log_path: result.logPath,
        log: result.log,
        truncated: result.truncated,
      };
      const header = `Session ${session_id} (${result.status}${result.truncated ? ", log truncated" : ""}):\n`;
      return {
        content: [{ type: "text" as const, text: header + result.log }],
        structuredContent: output,
      };
    } catch (error) {
      return errorResult(error);
    }
  },
);

const ListTrackedInputSchema = z
  .object({
    include_exited: z
      .boolean()
      .default(true)
      .describe("Whether to include sessions whose tracked command has already exited (default true)."),
  })
  .strict();

server.registerTool(
  "rustlogger_list_tracked",
  {
    title: "List Tracked Programs",
    description: `List every program started with rustlogger_start_tracking that this MCP server knows about, with its current status.

Args:
  - include_exited (boolean): include sessions that have already finished (default true)

Returns (JSON):
  {
    "sessions": [
      {
        "session_id": string,
        "command": string,
        "args": string[],
        "cwd": string,
        "label": string | null,
        "status": "running" | "exited",
        "pid": number,
        "started_at": string,
        "log_path": string | null
      }
    ]
  }

Examples:
  - Use when: "what am I tracking right now?" -> list_tracked with include_exited=false
  - Use when: figuring out a session_id to pass to rustlogger_get_log or rustlogger_stop_tracking

Error Handling:
  - Never errors; returns an empty "sessions" array if nothing has been tracked yet`,
    inputSchema: ListTrackedInputSchema,
    annotations: {
      readOnlyHint: true,
      destructiveHint: false,
      idempotentHint: true,
      openWorldHint: true,
    },
  },
  async ({ include_exited }) => {
    const sessions = listTracked()
      .filter((session) => include_exited || session.status === "running")
      .map((session) => ({
        session_id: session.id,
        command: session.command,
        args: session.args,
        cwd: session.cwd,
        label: session.label ?? null,
        status: session.status,
        pid: session.pid,
        started_at: session.startedAt,
        log_path: session.logPath,
      }));

    const summary =
      sessions.length === 0
        ? "No tracked sessions."
        : sessions
            .map(
              (s) =>
                `- ${s.session_id} [${s.status}] ${[s.command, ...s.args].join(" ")}${s.label ? ` ("${s.label}")` : ""}`,
            )
            .join("\n");

    return {
      content: [{ type: "text" as const, text: summary }],
      structuredContent: { sessions },
    };
  },
);

const StopTrackingInputSchema = z
  .object({
    session_id: z.string().min(1).describe("The session id to stop tracking."),
  })
  .strict();

server.registerTool(
  "rustlogger_stop_tracking",
  {
    title: "Stop Tracking a Program",
    description: `Stop a program started with rustlogger_start_tracking. Sends rustlogger itself SIGTERM, which makes it send the tracked command SIGHUP (so it isn't left running detached), reap it, and write the log's closing footer before exiting.

Args:
  - session_id (string): which tracked session to stop

Returns (JSON):
  {
    "session_id": string,
    "stopped": boolean,         // true if this call is what ended it
    "already_exited": boolean   // true if it had already ended on its own
  }

Examples:
  - Use when: "stop tracking that build, I don't need it anymore"
  - Use when: cleaning up a session after rustlogger_get_log shows it's done (already_exited will be true; calling this is harmless either way)

Error Handling:
  - Throws "no tracked session with id ..." if session_id isn't recognized - call rustlogger_list_tracked to see valid ids
  - Does not error if the process had already exited; reports already_exited instead`,
    inputSchema: StopTrackingInputSchema,
    annotations: {
      readOnlyHint: false,
      destructiveHint: true,
      idempotentHint: true,
      openWorldHint: true,
    },
  },
  async ({ session_id }) => {
    try {
      const result = await stopTracking(session_id);
      const output = {
        session_id,
        stopped: result.stopped,
        already_exited: result.alreadyExited,
      };
      const text = result.alreadyExited
        ? `Session ${session_id} had already exited.`
        : result.stopped
          ? `Stopped session ${session_id}.`
          : `Sent session ${session_id} a stop signal, but it hadn't exited within the wait window - it may still be shutting down.`;
      return {
        content: [{ type: "text" as const, text }],
        structuredContent: output,
      };
    } catch (error) {
      return errorResult(error);
    }
  },
);

async function main(): Promise<void> {
  const transport = new StdioServerTransport();
  await server.connect(transport);
  console.error("rustlogger-mcp-server running via stdio");
}

main().catch((error) => {
  console.error("rustlogger-mcp-server failed to start:", error);
  process.exit(1);
});
