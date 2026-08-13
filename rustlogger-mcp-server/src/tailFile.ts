import fs from "node:fs";

/**
 * Reads only the trailing window of a file, instead of the whole thing.
 *
 * The previous implementation called `fs.readFileSync` on the entire log
 * and *then* applied `tail_lines` and the character limit
 * (`.security/findings.md` RT-mcp-2026-08-04-04). Neither bound limited
 * what was read from disk into memory, and a log with no newlines defeated
 * the tail slice entirely - a tracked process that streams (a chatty
 * build, `yes`, anything piping large output) grows its log without bound,
 * and a single `get_log` call then pulled all of it into this server's
 * RAM. Measured: a 200 MB log produced a 200 MB RSS jump even with
 * `tail_lines: 1`; a large enough log throws past Node's ~2 GiB string cap
 * and takes the server down.
 *
 * Reading backwards from the end bounds the work by what was actually
 * asked for, regardless of how large the file has grown.
 */

/** How much to read per backward step while looking for enough newlines. */
const CHUNK_SIZE = 64 * 1024;

export interface TailResult {
  /** The trailing text read (at most `maxBytes`, plus line alignment). */
  text: string;
  /** True if content before `text` was left unread. */
  truncated: boolean;
}

/**
 * Returns the last `maxLines` lines of `filePath`, reading at most
 * `maxBytes` from the end of the file.
 *
 * `maxBytes` is the hard stop and applies even when the file has no line
 * breaks at all - that specific case is what made `tail_lines` useless as
 * a bound before. A file smaller than `maxBytes` is returned whole, and
 * `truncated` is false only when nothing at all was skipped.
 */
export function tailFile(filePath: string, maxLines: number, maxBytes: number): TailResult {
  let fd: number;
  try {
    fd = fs.openSync(filePath, "r");
  } catch {
    return { text: "", truncated: false };
  }

  try {
    const size = fs.fstatSync(fd).size;
    if (size === 0) {
      return { text: "", truncated: false };
    }

    // Walk backwards from EOF until we've collected enough newlines or hit
    // the byte budget, whichever comes first.
    let start = size;
    let collected = "";
    let newlines = 0;
    const wantsLines = maxLines > 0;

    while (start > 0) {
      const remainingBudget = maxBytes - collected.length;
      if (remainingBudget <= 0) {
        break;
      }
      const step = Math.min(CHUNK_SIZE, remainingBudget, start);
      const readFrom = start - step;
      const buffer = Buffer.allocUnsafe(step);
      fs.readSync(fd, buffer, 0, step, readFrom);
      const chunk = buffer.toString("utf8");

      collected = chunk + collected;
      start = readFrom;

      if (wantsLines) {
        // Count newlines in what we just prepended; stop once the window
        // definitely contains `maxLines` complete lines.
        for (let i = 0; i < chunk.length; i += 1) {
          if (chunk[i] === "\n") {
            newlines += 1;
          }
        }
        if (newlines > maxLines) {
          break;
        }
      }
    }

    let text = collected;
    let truncated = start > 0;

    if (wantsLines) {
      const lines = text.split("\n");
      if (lines.length > maxLines) {
        text = lines.slice(-maxLines).join("\n");
        truncated = true;
      }
    }

    if (text.length > maxBytes) {
      text = text.slice(text.length - maxBytes);
      truncated = true;
    }

    return { text, truncated };
  } finally {
    fs.closeSync(fd);
  }
}
