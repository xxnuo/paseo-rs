import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { randomUUID } from "node:crypto";
import { mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { createPaseoClient } from "@getpaseo/client";
import { createTestPaseoDaemon } from "../packages/server/src/server/test-utils/paseo-daemon.js";
import { createTestAgentClients } from "../packages/server/src/server/test-utils/fake-agent-client.js";
import { getHostName } from "../packages/server/src/server/host-name.js";

// node --import tsx scripts/run-nsh-rust-live.mts /path/to/nsh [linux|headless|web|...] [appDir]
// --fixture-only verifies real SDK fixture setup without claiming Rust UI coverage.
const command = process.argv[2];
assert(command, "Pass the built nsh binary path, or --fixture-only to verify fixture setup");
const target = process.argv[3] ?? "linux";
const repository = fileURLToPath(new URL("../", import.meta.url));
const appDirectory = path.resolve(process.argv[4] ?? path.join(repository, "packages/app-nsh"));
const testFile = path.join(appDirectory, "probes", `.rust-live-${randomUUID()}.test.ts`);
const scratch = await mkdtemp(path.join(tmpdir(), "paseo-nsh-rust-live-"));
const previousTmpdir = process.env.TMPDIR;
process.env.TMPDIR = scratch;
let daemon: Awaited<ReturnType<typeof createTestPaseoDaemon>> | undefined;
let seed: ReturnType<typeof createPaseoClient> | undefined;
try {
  const cwd = path.join(scratch, "NSH_RUST_FIXTURE");
  await mkdir(cwd);
  daemon = await createTestPaseoDaemon({
    paseoHomeRoot: path.join(scratch, "daemon"),
    staticDir: path.join(scratch, "static"),
    cleanup: false,
    mcpEnabled: false,
    pluginsEnabled: false,
    agentClients: createTestAgentClients(),
  });
  const url = `ws://127.0.0.1:${daemon.port}/ws`;
  seed = createPaseoClient({
    url,
    clientId: `nsh-rust-fixture-${randomUUID()}`,
    appVersion: "0.15.2",
    reconnect: { enabled: false },
  });
  await seed.connect();
  await seed.agents.list();
  await seed.agents.create({
    config: { provider: "codex/gpt-5.4-mini", modeId: "full-access" },
    cwd,
    title: "Rust UI fixture (fake provider, no model)",
  });
  const listing = await seed.projects.list();
  const project = listing.projects.find((entry) => entry.projectRootPath === cwd);
  assert(project, "The real SDK must observe the seeded project before running Rust UI");
  assert.equal(project.projectDisplayName, "NSH_RUST_FIXTURE");
  await seed.close();
  seed = undefined;
  console.log("Rust UI fixture: real loopback daemon and SDK seed, fake Codex provider (no model)");
  if (command === "--fixture-only") {
    console.log(
      JSON.stringify({ projectDisplayName: project.projectDisplayName, fixture: "ready" }),
    );
  } else {
    await writeFile(
      testFile,
      `import { expect, open, test } from "nsh/test";

test("Rust root connects, lists, refreshes, disconnects and reconnects to the real daemon", async () => {
  const app = await open("..", { network: "live" });
  const project = ${JSON.stringify(`text:${project.projectDisplayName}`)};
  const connected = ${JSON.stringify(`text:已连接：${getHostName()}`)};
  const address = "label:主机地址";
  const connect = "role:button[连接主机]";
  const refresh = "role:button[刷新项目]";
  const disconnect = "role:button[断开连接]";
  await app.waitFor("连接开发主机");
  expect(app.enabled(connect)).toBe(false);
  expect(app.enabled(refresh)).toBe(false);
  expect(app.enabled(disconnect)).toBe(false);
  await app.type(address, ${JSON.stringify(url)});
  expect(app.enabled(connect)).toBe(true);
  await app.press(connect);
  expect(app.enabled(address)).toBe(false);
  expect(app.enabled(connect)).toBe(false);
  await app.waitFor(project, { timeout: 20000 });
  await app.waitFor(connected);
  expect(app.enabled(refresh)).toBe(true);
  expect(app.enabled(disconnect)).toBe(true);
  const clientId = app.storage()["paseo.nsh.client-id"];
  expect(typeof clientId).toBe("string");
  expect(/^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/.test(clientId)).toBe(true);

  await app.press(refresh);
  await app.waitFor(connected, { timeout: 15000 });
  expect(app.shows(project)).toBe(true);
  expect(app.enabled(refresh)).toBe(true);
  await app.press(disconnect);
  await app.waitFor("text:已断开连接");
  expect(app.shows(project)).toBe(false);
  expect(app.enabled(address)).toBe(true);
  expect(app.enabled(connect)).toBe(true);
  expect(app.enabled(refresh)).toBe(false);
  expect(app.enabled(disconnect)).toBe(false);

  await app.press(connect);
  expect(app.enabled(address)).toBe(false);
  await app.waitFor(project, { timeout: 20000 });
  await app.waitFor(connected);
  expect(app.storage()["paseo.nsh.client-id"]).toBe(clientId);
  await app.press(disconnect);
  await app.waitFor("text:已断开连接");
  expect(app.shows(project)).toBe(false);
}, 90000);
`,
      { flag: "wx" },
    );
    const args = ["test", testFile];
    if (target !== "headless") args.push("--target", target);
    await new Promise<void>((resolve, reject) => {
      const child = spawn(path.resolve(command), args, { cwd: appDirectory, stdio: "inherit" });
      const timeout = setTimeout(() => child.kill("SIGKILL"), 240_000);
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
        else reject(new Error(`Rust UI verification exited with ${signal ?? code}`));
      });
    });
  }
} finally {
  try {
    await seed?.close();
  } finally {
    try {
      await daemon?.close();
    } finally {
      if (previousTmpdir === undefined) delete process.env.TMPDIR;
      else process.env.TMPDIR = previousTmpdir;
      await rm(testFile, { force: true });
      await rm(scratch, { recursive: true, force: true, maxRetries: 3, retryDelay: 100 });
    }
  }
}
