# rustlogger-mcp-server

An MCP server that lets an agent (Claude) track a program running in the
background using [rustlogger](../rustlogger/)'s headless tracking mode, check
its log later, and stop it — without blocking on long-running or
fire-and-forget commands.

## Why this exists

rustlogger's ordinary mode wraps a live human's terminal session. Headless
mode (`rustlogger <command> [args...]`, added specifically for this) instead
runs one command in the background with no terminal attached, logging its
output. This server wraps that in four tools an agent can call directly.

Tracked processes are spawned detached and outlive this server's own process
— stopping or restarting the MCP server doesn't kill what's being tracked.
Session metadata is persisted to `~/.rustlogger-mcp-server/state.json`, so
`rustlogger_list_tracked` and `rustlogger_get_log` keep working correctly
across restarts too.

## Tools

- **`rustlogger_start_tracking`** — start a command running in the
  background under rustlogger, given `command`, `args`, and a required
  `cwd` (also where its log file lands, per rustlogger's own convention).
  Returns a `session_id`.
- **`rustlogger_get_log`** — read back a tracked session's log (with
  `tail_lines` control) and whether it's still running.
- **`rustlogger_list_tracked`** — list every known session and its status.
- **`rustlogger_stop_tracking`** — stop a tracked session (sends rustlogger
  `SIGTERM`, which makes it clean up the tracked command with `SIGHUP` and
  close the log with a proper footer, same as any other rustlogger exit).

Full argument/return details are in the description embedded on each tool
(visible to any MCP client, including this one) — see `src/index.ts`.

## Setup

```bash
npm install
npm run build
```

Requires a `rustlogger` binary. Resolved in order:

1. `RUSTLOGGER_BIN` env var, if set.
2. A release or debug build in this same repo's Cargo workspace output
   (`rust_tuis/target/{release,debug}/rustlogger`) — build it with
   `cargo build --release -p rustlogger` from the repo root if it's missing.
3. Bare `rustlogger` on `$PATH`.

## Registering it with Claude Code

This repo's `.mcp.json` already registers it for this project. If you need
to do it elsewhere:

```json
{
  "mcpServers": {
    "rustlogger": {
      "command": "node",
      "args": ["/absolute/path/to/rustlogger-mcp-server/dist/index.js"]
    }
  }
}
```

## Testing

```bash
npm run build
node scripts/smoke-test.mjs
```

Connects to the built server exactly like a real MCP client (over stdio)
and drives all four tools against real tracked processes: a short command
whose progress is checked mid-run and after it finishes, a long-running one
that gets stopped early, and a couple of error-handling cases (unknown
session id, stopping something already stopped). Not a formal test suite —
a direct functional check, proportionate to a small local tool wrapping
four operations rather than a full third-party API.

## Known limitations

- Each tracked session leaves a small bookkeeping directory under
  `~/.rustlogger-mcp-server/sessions/<id>/` (rustlogger's own redirected
  stdout/stderr — a few KB). Nothing prunes these automatically yet.
- No concurrency control on the persisted state file — fine for
  single-user, low-frequency tool calls, not for many simultaneous writers.
