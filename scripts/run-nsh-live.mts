import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdir, mkdtemp, rm, symlink, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { createTestPaseoDaemon } from "../packages/server/src/server/test-utils/paseo-daemon.js";
import { createTestAgentClients } from "../packages/server/src/server/test-utils/fake-agent-client.js";
import { runLiveSdkOracle } from "../packages/app-nsh/probes/live.js";

// node --import tsx scripts/run-nsh-live.mts /path/to/nsh
// --node-only checks the real daemon fixture and SDK oracle without claiming QuickJS coverage.
const command = process.argv[2];
assert(command, "Pass the built nsh binary path, or --node-only to check only the Node fixture");
const repository = fileURLToPath(new URL("../", import.meta.url));
const scratch = await mkdtemp(path.join(tmpdir(), "paseo-nsh-live-"));
const previousTmpdir = process.env.TMPDIR;
// The existing fake provider writes history under tmpdir(); keep this run's files together.
process.env.TMPDIR = scratch;
let daemon: Awaited<ReturnType<typeof createTestPaseoDaemon>> | undefined;
let releaseCancel: (() => void) | undefined;
let interrupts = 0;
try {
  const cwd = path.join(scratch, "project");
  await mkdir(cwd);
  daemon = await createTestPaseoDaemon({
    paseoHomeRoot: path.join(scratch, "daemon"),
    staticDir: path.join(scratch, "static"),
    cleanup: false,
    mcpEnabled: false,
    pluginsEnabled: false,
    agentClients: createTestAgentClients({
      beforeTurnComplete: async (prompt) => {
        if (prompt === "Respond with exactly: NSH_CANCEL_READY") {
          await new Promise<void>((resolve) => {
            releaseCancel = resolve;
          });
        }
      },
      onInterrupt: () => {
        interrupts += 1;
        releaseCancel?.();
      },
    }),
  });
  const config = { url: `ws://127.0.0.1:${daemon.port}/ws`, cwd };
  console.log(
    "SDK protocol oracle: real loopback daemon and WebSocket, fake Codex provider (no model)",
  );
  if (command === "--node-only") {
    console.log(JSON.stringify(await runLiveSdkOracle(config)));
  } else {
    await symlink(
      path.join(repository, "node_modules"),
      path.join(scratch, "node_modules"),
      "junction",
    );
    await writeFile(
      path.join(scratch, "nsh.json"),
      JSON.stringify({
        name: "Paseo SDK protocol oracle",
        id: "sh.paseo.sdk-oracle",
        capabilities: { network: { hosts: ["127.0.0.1"] } },
      }),
    );
    const bootstrap = path
      .join(repository, "packages/app-nsh/runtime/bootstrap.ts")
      .replaceAll("\\", "/");
    const oracle = path.join(repository, "packages/app-nsh/probes/live.ts").replaceAll("\\", "/");
    await writeFile(
      path.join(scratch, "App.tsx"),
      `import ${JSON.stringify(bootstrap)};
import { mount, signal, Text } from "nsh";
import { runLiveSdkOracle } from ${JSON.stringify(oracle)};
const status = signal("running");
function Oracle() { return <Text>{status()}</Text>; }
mount(Oracle);
runLiveSdkOracle(${JSON.stringify(config)}).then(
  report => { console.log(JSON.stringify(report)); status.set("SDK_ORACLE_PASSED"); },
  error => status.set("SDK_ORACLE_FAILED: " + String(error)),
);
`,
    );
    await writeFile(
      path.join(scratch, "live.test.ts"),
      `import { expect, open, test } from "nsh/test";
test("real daemon SDK protocol oracle (fake provider, no model)", async () => {
  const app = await open("./App.tsx", { network: "live" });
  await app.waitFor("text:SDK_ORACLE_PASSED", { timeout: 45000 });
  expect(app.shows("text:SDK_ORACLE_PASSED")).toBe(true);
}, 60000);
`,
    );
    const binary = path.resolve(command);
    await new Promise<void>((resolve, reject) => {
      const child = spawn(binary, ["test", path.join(scratch, "live.test.ts")], {
        cwd: scratch,
        stdio: "inherit",
      });
      const timeout = setTimeout(() => child.kill("SIGKILL"), 90_000);
      const interrupt = () => child.kill("SIGTERM");
      const cleanup = () => {
        clearTimeout(timeout);
        process.off("SIGINT", interrupt);
        process.off("SIGTERM", interrupt);
      };
      process.once("SIGINT", interrupt);
      process.once("SIGTERM", interrupt);
      child.once("error", (error) => {
        cleanup();
        reject(error);
      });
      child.once("close", (code, signal) => {
        cleanup();
        if (code === 0) resolve();
        else reject(new Error(`NSH oracle exited with ${signal ?? code}`));
      });
    });
  }
  assert.equal(
    interrupts,
    1,
    "The cancel RPC must interrupt an active fake-provider turn exactly once",
  );
} finally {
  releaseCancel?.();
  try {
    await daemon?.close();
  } finally {
    if (previousTmpdir === undefined) delete process.env.TMPDIR;
    else process.env.TMPDIR = previousTmpdir;
    await rm(scratch, { recursive: true, force: true, maxRetries: 3, retryDelay: 100 });
  }
}
