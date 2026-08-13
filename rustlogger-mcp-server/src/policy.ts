import fs from "node:fs";
import path from "node:path";

/**
 * Opt-in restrictions on what this server may execute and where.
 *
 * Running an arbitrary program *is* this server's job, so there is no fix
 * for `.security/findings.md` RT-mcp-2026-08-04-01 that doesn't change what
 * the tool is allowed to do. The security-relevant point is the stated
 * threat model: an agent that relays untrusted content can be prompt-
 * injected into a single `start_tracking` call and get arbitrary code
 * execution - detached, and outliving both the injection turn and this
 * server (see `sessionManager.ts` on why tracked processes are detached).
 *
 * The chosen shape (chunk 9 decision in `.security/mitigation-plan.md`) is
 * an **opt-in allowlist**: with nothing configured, behavior is exactly
 * what it has always been, so no existing setup breaks; whoever runs the
 * server can then narrow it without touching code. Configuration lives in
 * environment variables rather than tool inputs on purpose - the client
 * (and therefore anything that injected the client) must not be able to
 * widen its own permissions.
 *
 *   RUSTLOGGER_MCP_ALLOWED_COMMANDS
 *     Comma-separated. Each entry matches either a bare command name
 *     ("npm") or an absolute path ("/usr/bin/python3"). A request whose
 *     `command` is neither an exact match nor a path whose basename
 *     matches is refused. Unset/empty = allow anything.
 *
 *   RUSTLOGGER_MCP_ALLOWED_CWD_ROOTS
 *     Comma-separated absolute directories. A request's `cwd` must resolve
 *     to one of them or something beneath it. This is also what bounds
 *     where predictably-named log files can be dropped
 *     (RT-mcp-2026-08-04-05). Unset/empty = allow any directory.
 */

const COMMANDS_ENV = "RUSTLOGGER_MCP_ALLOWED_COMMANDS";
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

/** Read fresh on each call rather than cached at import, so a test (or an
 * operator restarting under a new environment) sees the current value. */
export function allowedCommands(): string[] {
  return parseList(process.env[COMMANDS_ENV]);
}

export function allowedCwdRoots(): string[] {
  return parseList(process.env[CWD_ROOTS_ENV]);
}

/**
 * Throws unless `command` is permitted by the configured allowlist. A
 * bare name in the allowlist ("npm") also authorizes an absolute path
 * whose basename matches ("/usr/bin/npm"), since those are the same
 * program from the operator's point of view; the reverse is not true - an
 * allowlist entry that is an absolute path authorizes only that exact
 * path.
 */
export function assertCommandAllowed(command: string): void {
  const allowed = allowedCommands();
  if (allowed.length === 0) {
    return;
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
