import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const __dirname = path.dirname(fileURLToPath(import.meta.url));

/**
 * Locates the rustlogger binary. Checked in order:
 * 1. `RUSTLOGGER_BIN` env var, if set - an explicit override.
 * 2. A release or debug build sitting in this same repo. `rustlogger` is
 *    a member of a Cargo *workspace* rooted at `rust_tuis/`, so its build
 *    output lands in the workspace root's `target/` (`rust_tuis/target/`),
 *    not `rustlogger/target/` - a common trip-up for anyone expecting
 *    per-crate target dirs.
 * 3. Bare `rustlogger`, relying on `$PATH` - if the binary has been
 *    installed there separately.
 *
 * Doesn't validate the result beyond checking file existence for options
 * 2; a bad `$PATH` entry (option 3) surfaces as a normal ENOENT from
 * `spawn`, which callers already handle.
 */
export function resolveRustloggerBin(): string {
  const override = process.env.RUSTLOGGER_BIN;
  if (override) {
    return override;
  }

  const workspaceSiblingCandidates = [
    path.resolve(__dirname, "..", "..", "target", "release", "rustlogger"),
    path.resolve(__dirname, "..", "..", "target", "debug", "rustlogger"),
  ];
  for (const candidate of workspaceSiblingCandidates) {
    if (fs.existsSync(candidate)) {
      return candidate;
    }
  }

  return "rustlogger";
}
