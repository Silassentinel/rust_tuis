#!/usr/bin/env node
/**
 * Functional smoke test for rustlogger-mcp-server: connects to the built
 * server exactly like a real MCP client would (over stdio), and drives
 * all four tools against real tracked processes. Not a unit test suite -
 * this is deliberately a small, direct end-to-end check rather than the
 * full "10 evaluation questions" ceremony the mcp-builder skill describes
 * for wrapping a third-party API; there's no external service here to
 * evaluate against, just this server's own four tools working correctly
 * together. Run after `npm run build`:
 *
 *   node scripts/smoke-test.mjs
 */

import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { StdioClientTransport } from "@modelcontextprotocol/sdk/client/stdio.js";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const SERVER_JS = path.resolve(__dirname, "..", "dist", "index.js");
const TEST_CWD = fs.mkdtempSync(path.join(os.tmpdir(), "rustlogger-mcp-smoke-"));

function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

function structured(result) {
  return result.structuredContent;
}

function assertTrue(condition, message) {
  if (!condition) {
    throw new Error(`FAIL: ${message}`);
  }
  console.log(`PASS: ${message}`);
}

async function main() {
  if (!fs.existsSync(SERVER_JS)) {
    throw new Error(`${SERVER_JS} not found - run "npm run build" first`);
  }

  // This test exercises "bash" and "sleep" below. Since RUSTLOGGER_MCP_ALLOWED_COMMANDS
  // is required by default (see src/policy.ts), pass an explicit allowlist covering
  // exactly those rather than opting out of the allowlist entirely - the smoke test
  // should demonstrate the recommended (restricted) configuration, not the escape hatch.
  const transport = new StdioClientTransport({
    command: "node",
    args: [SERVER_JS],
    env: { ...process.env, RUSTLOGGER_MCP_ALLOWED_COMMANDS: "bash,sleep" },
  });
  const client = new Client({ name: "smoke-test-client", version: "1.0.0" });
  await client.connect(transport);

  console.log("=== tools/list ===");
  const tools = await client.listTools();
  const names = tools.tools.map((t) => t.name).sort();
  console.log(names.join(", "));
  assertTrue(
    ["rustlogger_get_log", "rustlogger_list_tracked", "rustlogger_start_tracking", "rustlogger_stop_tracking"].every(
      (n) => names.includes(n),
    ),
    "all four tools are registered",
  );

  console.log("\n=== start_tracking a short-lived command that produces output over time ===");
  const startResult = await client.callTool({
    name: "rustlogger_start_tracking",
    arguments: {
      command: "bash",
      args: ["-c", "for i in 1 2 3; do echo tick-$i; sleep 0.3; done; exit 0"],
      cwd: TEST_CWD,
      label: "smoke-test-ticker",
    },
  });
  const started = structured(startResult);
  console.log(startResult.content[0].text);
  assertTrue(!!started.session_id, "start_tracking returns a session_id");

  console.log("\n=== list_tracked shows it running ===");
  const list1 = structured(
    await client.callTool({ name: "rustlogger_list_tracked", arguments: { include_exited: true } }),
  );
  const found = list1.sessions.find((s) => s.session_id === started.session_id);
  assertTrue(!!found, "session appears in list_tracked");
  assertTrue(found.status === "running", `list_tracked reports "running" right after start (got: ${found.status})`);

  console.log("\n=== get_log partway through ===");
  await sleep(400);
  const midLog = structured(
    await client.callTool({
      name: "rustlogger_get_log",
      arguments: { session_id: started.session_id, tail_lines: 50 },
    }),
  );
  console.log(midLog.log);
  assertTrue(midLog.log.includes("tick-1"), "get_log shows output while the command is still running");

  console.log("\n=== waiting for it to finish, then get_log again ===");
  await sleep(1200);
  const finalLog = structured(
    await client.callTool({
      name: "rustlogger_get_log",
      arguments: { session_id: started.session_id, tail_lines: 50 },
    }),
  );
  console.log(finalLog.log);
  assertTrue(finalLog.status === "exited", `status is "exited" after the command finished (got: ${finalLog.status})`);
  assertTrue(finalLog.log.includes("tick-1") && finalLog.log.includes("tick-3"), "log contains all expected output");
  assertTrue(finalLog.log.includes("exit code: 0"), "log footer records the correct exit code");

  console.log("\n=== start a long-lived process, then stop it ===");
  const longSession = structured(
    await client.callTool({
      name: "rustlogger_start_tracking",
      arguments: { command: "sleep", args: ["30"], cwd: TEST_CWD, label: "smoke-test-sleep" },
    }),
  );
  await sleep(200);
  const stopResult = structured(
    await client.callTool({ name: "rustlogger_stop_tracking", arguments: { session_id: longSession.session_id } }),
  );
  assertTrue(stopResult.stopped === true, "stop_tracking reports the process was stopped");

  let pidStillAlive = true;
  try {
    process.kill(longSession.pid, 0);
  } catch (error) {
    pidStillAlive = error.code !== "ESRCH";
  }
  assertTrue(!pidStillAlive, "the underlying rustlogger pid is actually gone after stop_tracking");

  console.log("\n=== error handling: bogus session id ===");
  const badLog = await client.callTool({
    name: "rustlogger_get_log",
    arguments: { session_id: "not-a-real-session-id" },
  });
  assertTrue(badLog.isError === true, "get_log with an unknown session id reports isError");

  console.log("\n=== stop_tracking on an already-exited session ===");
  const doubleStop = structured(
    await client.callTool({ name: "rustlogger_stop_tracking", arguments: { session_id: started.session_id } }),
  );
  assertTrue(doubleStop.already_exited === true, "stopping an already-finished session reports already_exited");

  await client.close();
  fs.rmSync(TEST_CWD, { recursive: true, force: true });
  console.log("\nALL CHECKS PASSED");
}

main().catch((error) => {
  console.error("SMOKE TEST FAILED:", error);
  fs.rmSync(TEST_CWD, { recursive: true, force: true });
  process.exit(1);
});
