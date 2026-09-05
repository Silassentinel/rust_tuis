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

## Restricting what can be run

This server's job is to run programs — but if the agent using it ever relays
untrusted content (a web page, an issue, a file someone else wrote), a prompt
injection becomes arbitrary code execution on this machine, detached and
outliving the conversation. Because of that, **a command allowlist is
required by default**: with nothing configured, every `start_tracking` call
is refused. `cwd` confinement stays opt-in (see table below), since on its
own it's a lower-severity, defense-in-depth control.

Both variables are read from the *server's* environment, never from tool
inputs, so a client can't widen its own permissions.

| Variable | Effect |
|---|---|
| `RUSTLOGGER_MCP_ALLOWED_COMMANDS` | **Required unless `RUSTLOGGER_MCP_ALLOW_ALL_COMMANDS` is set.** Comma-separated list of permitted commands. A bare name (`npm`) also permits an absolute path with that basename (`/usr/bin/npm`); an absolute-path entry permits only that exact path. Anything else is refused. |
| `RUSTLOGGER_MCP_ALLOW_ALL_COMMANDS` | Explicit opt-out of the allowlist requirement — set to `1`/`true`/`yes` to restore run-anything behavior. Only do this if nothing driving this server can ever relay untrusted content (e.g. a single-user local setup where you are the only thing calling it). Ignored if `RUSTLOGGER_MCP_ALLOWED_COMMANDS` is also set. |
| `RUSTLOGGER_MCP_ALLOWED_CWD_ROOTS` | Optional. Comma-separated absolute directories. A request's `cwd` must resolve (symlinks included) to one of them or below. Also bounds where predictably-named log files can be written. Unset = any directory. |

Narrowed to specific commands (recommended for anything agent-driven):

```json
{
  "mcpServers": {
    "rustlogger": {
      "command": "node",
      "args": ["/absolute/path/to/rustlogger-mcp-server/dist/index.js"],
      "env": {
        "RUSTLOGGER_MCP_ALLOWED_COMMANDS": "npm,cargo,make",
        "RUSTLOGGER_MCP_ALLOWED_CWD_ROOTS": "/home/you/code"
      }
    }
  }
}
```

Explicitly unrestricted (only for a trusted single-user local setup):

```json
{
  "mcpServers": {
    "rustlogger": {
      "command": "node",
      "args": ["/absolute/path/to/rustlogger-mcp-server/dist/index.js"],
      "env": { "RUSTLOGGER_MCP_ALLOW_ALL_COMMANDS": "1" }
    }
  }
}
```

## Log output is treated as untrusted

`rustlogger_get_log` returns text a tracked process wrote, so that text is
attacker-controlled whenever the tracked process is. rustlogger itself stores
logs byte-for-byte on purpose (an honest transcript), so this server
sanitizes at the boundary instead: terminal escape sequences are stripped,
carriage-return overwrites are expanded into visible lines, Unicode bidi
overrides are removed, lines forging rustlogger's own session markers are
tagged `[forged-marker]`, and the whole body is wrapped in an explicit
untrusted-content boundary. See `src/sanitize.ts`.

That closes the mechanical tricks, not the semantic one — a tracked process
can still print a sentence that reads like an instruction. Treat log content
as data to report on, never as instructions.

## Testing

```bash
npm test                     # type-check + unit/regression tests
node scripts/smoke-test.mjs  # end-to-end against real tracked processes
```

`npm test` runs the security regression suite in `test/` (Node's built-in
test runner — no extra dependency), covering log sanitization, the bounded
log read, the command/cwd allowlists, pid-identity validation, and
serialized state writes.

The smoke test:

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
- State-file writes are serialized in-process, so concurrent tool calls no
  longer clobber each other's session records. There is still no
  *cross-process* lock: running two copies of this server against the same
  `~/.rustlogger-mcp-server/state.json` is unsupported.
- Pid-identity validation (which stops a recycled pid from being reported as
  "running", or signalled by `stop_tracking`) uses `/proc` and therefore
  only applies on Linux. On other platforms the check degrades to a plain
  liveness test, as does any session recorded before this field existed.
