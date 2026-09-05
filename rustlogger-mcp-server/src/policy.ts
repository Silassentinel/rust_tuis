import fs from "node:fs";
import path from "node:path";

/**
 * Restrictions on what this server may execute and where.
 *
 * Running an arbitrary program *is* this server's job, so there is no fix
 * for `.security/findings.md` RT-mcp-2026-08-04-01 that doesn't change what
 * the tool is allowed to do. The security-relevant point is the stated
 * threat model: an agent that relays untrusted content can be prompt-
 * injected into a single `start_tracking` call and get arbitrary code
 * execution - detached, and outliving both the injection turn and this
 * server (see `sessionManager.ts` on why tracked processes are detached).
 *
 * The command allowlist is **required by default** (revised from the
 * original opt-in chunk 9 decision, see `.security/mitigation-plan.md`):
 * with nothing configured, `assertCommandAllowed` refuses everything,
 * because a silent fail-open default is exactly the exposure the finding
 * was about. An operator who wants the old unrestricted behavior back
 * (e.g. a single-user local setup where "the agent" is just the operator
 * themselves) sets `RUSTLOGGER_MCP_ALLOW_ALL_COMMANDS` explicitly - that
 * choice is then visible in the server's own config rather than being
 * true by omission. Configuration lives in environment variables rather
 * than tool inputs on purpose - the client (and therefore anything that
 * injected the client) must not be able to widen its own permissions.
 *
 * The cwd-roots restriction (RT-mcp-2026-08-04-05, low severity, only a
 * contributing factor to RT-core-2026-07-30-02's symlink attack) is
 * unchanged and stays opt-in: unset means any directory is allowed.
 *
 *   RUSTLOGGER_MCP_ALLOWED_COMMANDS
 *     Comma-separated. Each entry matches either a bare command name
 *     ("npm") or an absolute path ("/usr/bin/python3"). A request whose
 *     `command` is neither an exact match nor a path whose basename
 *     matches is refused. Unset/empty = fall through to the check below.
 *
 *   RUSTLOGGER_MCP_ALLOW_ALL_COMMANDS
 *     Explicit opt-out of the allowlist requirement. Only consulted when
 *     RUSTLOGGER_MCP_ALLOWED_COMMANDS is unset/empty. Truthy values ("1",
 *     "true", "yes", case-insensitive) restore the original
 *     run-anything behavior; anything else (including unset) does not.
 *
 *   RUSTLOGGER_MCP_ALLOWED_CWD_ROOTS
 *     Comma-separated absolute directories. A request's `cwd` must resolve
 *     to one of them or something beneath it. This is also what bounds
 *     where predictably-named log files can be dropped
 *     (RT-mcp-2026-08-04-05). Unset/empty = allow any directory.
 */

const COMMANDS_ENV = "RUSTLOGGER_MCP_ALLOWED_COMMANDS";
const ALLOW_ALL_COMMANDS_ENV = "RUSTLOGGER_MCP_ALLOW_ALL_COMMANDS";
const CWD_ROOTS_ENV = "RUSTLOGGER_MCP_ALLOWED_CWD_ROOTS";

function parseList(value: string | undefined): string[] {
  if (!value) {
    return [];
  }
  return value
    .split(",")
    .map((entry) => entry.trim())
    .filter((entry) => entry.length > 0);
}

function parseBoolEnv(value: string | undefined): boolean {
  if (!value) {
    return false;
  }
  return ["1", "true", "yes"].includes(value.trim().toLowerCase());
}

/** Read fresh on each call rather than cached at import, so a test (or an
 * operator restarting under a new environment) sees the current value. */
export function allowedCommands(): string[] {
  return parseList(process.env[COMMANDS_ENV]);
}

export function allCommandsExplicitlyAllowed(): boolean {
  return parseBoolEnv(process.env[ALLOW_ALL_COMMANDS_ENV]);
}

export function allowedCwdRoots(): string[] {
  return parseList(process.env[CWD_ROOTS_ENV]);
}

/**
 * Throws unless `command` is permitted. With `RUSTLOGGER_MCP_ALLOWED_COMMANDS`
 * configured, only entries on that list pass - a bare name ("npm") also
 * authorizes an absolute path whose basename matches ("/usr/bin/npm"),
 * since those are the same program from the operator's point of view; the
 * reverse is not true, an allowlist entry that is an absolute path
 * authorizes only that exact path. With nothing configured, every command
 * is refused unless `RUSTLOGGER_MCP_ALLOW_ALL_COMMANDS` explicitly opts out
 * of the allowlist requirement.
 */
export function assertCommandAllowed(command: string): void {
  const allowed = allowedCommands();

  if (allowed.length === 0) {
    if (allCommandsExplicitlyAllowed()) {
      return;
    }
    throw new Error(
      `no command allowlist is configured, so this server refuses to run ` +
        `anything by default. Set ${COMMANDS_ENV} to a comma-separated list ` +
        `of permitted commands (e.g. "npm,cargo,make"), or set ` +
        `${ALLOW_ALL_COMMANDS_ENV}=1 to explicitly run without an ` +
        `allowlist - only do that if nothing driving this server can ever ` +
        `relay untrusted content.`,
    );
  }

  const permitted = allowed.some((entry) => {
    if (entry === command) {
      return true;
    }
    // A bare-name entry matches the basename of an absolute request.
    if (!entry.includes(path.sep) && path.basename(command) === entry) {
      return true;
    }
    return false;
  });

  if (!permitted) {
    throw new Error(
      `command "${command}" is not permitted by this server's allowlist. ` +
        `Allowed: ${allowed.join(", ")}. ` +
        `(Set ${COMMANDS_ENV} in the server's environment to change this; ` +
        `it cannot be changed through a tool call.)`,
    );
  }
}

/**
 * Throws unless `cwd` resolves to a configured root or something beneath
 * it. Uses `fs.realpathSync` so a symlink can't be used to point outside a
 * permitted root while still looking like it's inside one.
 */
export function assertCwdAllowed(cwd: string): void {
  const roots = allowedCwdRoots();
  if (roots.length === 0) {
    return;
  }

  const resolved = fs.realpathSync(path.resolve(cwd));
  const permitted = roots.some((root) => {
    let resolvedRoot: string;
    try {
      resolvedRoot = fs.realpathSync(path.resolve(root));
    } catch {
      // A configured root that doesn't exist can't contain anything.
      return false;
    }
    const withSep = resolvedRoot.endsWith(path.sep)
      ? resolvedRoot
      : resolvedRoot + path.sep;
    // Equal to the root, or genuinely beneath it. The separator check
    // stops "/srv/data-evil" from being accepted for a "/srv/data" root.
    return resolved === resolvedRoot || resolved.startsWith(withSep);
  });

  if (!permitted) {
    throw new Error(
      `cwd "${cwd}" is outside every directory this server is allowed to ` +
        `run in. Allowed roots: ${roots.join(", ")}. ` +
        `(Set ${CWD_ROOTS_ENV} in the server's environment to change this; ` +
        `it cannot be changed through a tool call.)`,
    );
  }
}
