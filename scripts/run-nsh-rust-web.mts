import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { randomUUID } from "node:crypto";
import { access, mkdir, mkdtemp, readdir, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { createPaseoClient } from "@getpaseo/client";
import { chromium, type Browser, type Page } from "playwright";
import { WebSocket } from "ws";
import { createTestPaseoDaemon } from "../packages/server/src/server/test-utils/paseo-daemon.js";
import { createTestAgentClients } from "../packages/server/src/server/test-utils/fake-agent-client.js";
import { getHostName } from "../packages/server/src/server/host-name.js";

// node --import tsx scripts/run-nsh-rust-web.mts /path/to/nsh [appDir] [browserPath]
// --fixture-only skips packaging/browser UI: checks daemon static hosting, SDK seed and Origin rules.
const command = process.argv[2];
assert(command, "Pass a built nsh binary path, or --fixture-only");
const repository = fileURLToPath(new URL("../", import.meta.url));
const appDirectory = path.resolve(process.argv[3] ?? path.join(repository, "packages/app-nsh"));
const scratch = await mkdtemp(path.join(tmpdir(), "paseo-nsh-rust-web-"));
const previousTmpdir = process.env.TMPDIR;
process.env.TMPDIR = scratch;
const evidence = path.join(appDirectory, ".nsh", "evidence", `rust-web-${randomUUID()}`);
let daemon: Awaited<ReturnType<typeof createTestPaseoDaemon>> | undefined;
let seed: ReturnType<typeof createPaseoClient> | undefined;
let browser: Browser | undefined;
let page: Page | undefined;

async function runPackage(binary: string, output: string): Promise<void> {
  await new Promise<void>((resolve, reject) => {
    const child = spawn(binary, ["package", appDirectory, "--target", "web", "--out", output], {
      cwd: appDirectory,
      stdio: "inherit",
    });
    const timeout = setTimeout(() => child.kill("SIGKILL"), 600_000);
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
      else reject(new Error(`NSH web package exited with ${signal ?? code}`));
    });
  });
}

async function browserExecutable(): Promise<string> {
  if (process.argv[4]) {
    const explicit = path.resolve(process.argv[4]);
    await access(explicit);
    return explicit;
  }
  for (const candidate of [
    "/opt/google/chrome/google-chrome",
    "/usr/bin/google-chrome",
    "/usr/bin/chromium",
  ]) {
    try {
      await access(candidate);
      return candidate;
    } catch {
      // Try the next installed browser; never download one during verification.
    }
  }
  throw new Error("No installed Chrome/Chromium found; pass browserPath as the third argument");
}

async function upgradeStatus(url: string, origin: string): Promise<number> {
  return new Promise<number>((resolve, reject) => {
    const socket = new WebSocket(url, { origin });
    const timeout = setTimeout(() => {
      socket.terminate();
      reject(new Error("Origin-policy WebSocket upgrade timed out"));
    }, 5_000);
    socket.once("open", () => {
      clearTimeout(timeout);
      socket.close();
      resolve(101);
    });
    socket.once("unexpected-response", (_request, response) => {
      clearTimeout(timeout);
      response.resume();
      socket.terminate();
      resolve(response.statusCode ?? 0);
    });
    socket.once("error", (error) => {
      clearTimeout(timeout);
      reject(error);
    });
  });
}

try {
  const output = path.join(scratch, "dist");
  let distDir = output;
  let executable: string | undefined;
  if (command === "--fixture-only") {
    await mkdir(output);
    await writeFile(
      path.join(output, "index.html"),
      "<!doctype html><title>Origin fixture only</title>STATIC_FIXTURE_ONLY",
    );
  } else {
    executable = await browserExecutable();
    await runPackage(path.resolve(command), output);
    // `nsh package` writes --out/<display name>/index.html, including a Rust app's WASM runtime.
    const sites = [];
    for (const entry of await readdir(output, { withFileTypes: true })) {
      if (!entry.isDirectory()) continue;
      const directory = path.join(output, entry.name);
      try {
        await access(path.join(directory, "index.html"));
        sites.push(directory);
      } catch {
        // Other packaging output is not the standalone website.
      }
    }
    assert.equal(sites.length, 1, "Expected exactly one packaged static website");
    distDir = sites[0];
  }

  const cwd = path.join(scratch, "NSH_RUST_FIXTURE");
  await mkdir(cwd);
  daemon = await createTestPaseoDaemon({
    paseoHomeRoot: path.join(scratch, "daemon"),
    staticDir: path.join(scratch, "static"),
    cleanup: false,
    mcpEnabled: false,
    pluginsEnabled: false,
    agentClients: createTestAgentClients(),
    webUi: { enabled: true, distDir },
  });
  const origin = `http://127.0.0.1:${daemon.port}`;
  const url = `ws://127.0.0.1:${daemon.port}/ws`;
  const document = await fetch(`${origin}/`);
  assert.equal(document.status, 200);
  const html = await document.text();
  assert(html.includes("__PASEO_INITIAL_DAEMON_CONNECTION__"));
  assert.equal(await upgradeStatus(url, origin), 101);
  assert.equal(await upgradeStatus(url, "http://different-origin.invalid"), 403);

  seed = createPaseoClient({
    url,
    clientId: `rust-web-seed-${randomUUID()}`,
    appVersion: "0.15.2",
    reconnect: { enabled: false },
  });
  await seed.connect();
  await seed.agents.list();
  await seed.agents.create({
    cwd,
    config: { provider: "codex/gpt-5.4-mini", modeId: "full-access" },
    title: "Rust web fixture (fake provider, no model)",
  });
  const listing = await seed.projects.list();
  const project = listing.projects.find((entry) => entry.projectRootPath === cwd);
  assert(project, "The SDK must observe the seed project before browser verification");
  assert.equal(project.projectDisplayName, "NSH_RUST_FIXTURE");
  await seed.close();
  seed = undefined;
  console.log(
    "Rust Web verification: daemon serves static site and WebSocket on one origin; fake Codex provider, no model",
  );
  if (command === "--fixture-only") {
    assert(html.includes("STATIC_FIXTURE_ONLY"));
    console.log(
      "Fixture only passed: static HTTP 200, same-origin WS 101, cross-origin WS 403, SDK project seed; no WASM/UI checked",
    );
  } else {
    await mkdir(evidence, { recursive: true });
    browser = await chromium.launch({ executablePath: executable, headless: true });
    const context = await browser.newContext({ viewport: { width: 1000, height: 800 } });
    page = await context.newPage();
    page.setDefaultTimeout(20_000);
    const errors: string[] = [];
    const sockets: string[] = [];
    page.on("pageerror", (error) => errors.push(error.message));
    page.on("websocket", (socket) => sockets.push(socket.url()));
    await page.goto(`${origin}/`, { waitUntil: "networkidle", timeout: 60_000 });
    await page.getByText("连接开发主机", { exact: true }).waitFor();
    const address = page.getByRole("textbox", { name: "主机地址", exact: true });
    const connect = page.getByRole("button", { name: "连接主机", exact: true });
    const refresh = page.getByRole("button", { name: "刷新项目", exact: true });
    const disconnect = page.getByRole("button", { name: "断开连接", exact: true });
    const projectName = page.getByText(project.projectDisplayName, { exact: true });
    const connected = page.getByText(`已连接：${getHostName()}`, { exact: true });
    assert.equal(await connect.isDisabled(), true);
    await address.fill(url);
    const openingSocket = page.waitForEvent("websocket");
    await connect.click();
    const firstSocket = await openingSocket;
    await projectName.waitFor();
    await connected.waitFor();
    assert.equal(await address.isDisabled(), true);
    assert.equal(await connect.isDisabled(), true);
    assert.equal(await refresh.isEnabled(), true);
    await page.screenshot({ path: path.join(evidence, "connected.png"), fullPage: true });

    const refreshed = firstSocket.waitForEvent("framereceived", {
      timeout: 20_000,
      predicate: ({ payload }) =>
        typeof payload === "string" && payload.includes('"project.list.response"'),
    });
    await refresh.click();
    await refreshed;
    await connected.waitFor();
    assert.equal(await projectName.isVisible(), true);
    await disconnect.click();
    await page.getByText("已断开连接", { exact: true }).waitFor();
    assert.equal(await projectName.count(), 0);
    assert.equal(await address.isEnabled(), true);
    assert.equal(await refresh.isDisabled(), true);
    await page.screenshot({ path: path.join(evidence, "disconnected.png"), fullPage: true });

    await connect.click();
    await projectName.waitFor();
    await connected.waitFor();
    assert.equal(await address.isDisabled(), true);
    await page.screenshot({ path: path.join(evidence, "reconnected.png"), fullPage: true });
    await disconnect.click();
    await page.getByText("已断开连接", { exact: true }).waitFor();
    assert.equal(await projectName.count(), 0);
    assert.equal(new URL(page.url()).origin, origin);
    assert.equal(sockets.length, 2);
    assert(sockets.every((socket) => new URL(socket).host === new URL(origin).host));
    assert.deepEqual(errors, []);
    console.log(`Rust WASM browser UI passed; screenshots: ${evidence}`);
  }
} catch (error) {
  if (page && !page.isClosed()) {
    await mkdir(evidence, { recursive: true });
    await page
      .screenshot({ path: path.join(evidence, "failure.png"), fullPage: true })
      .catch(() => undefined);
    console.error(`Browser failure screenshot: ${evidence}`);
  }
  throw error;
} finally {
  const closing = await Promise.allSettled([browser?.close(), seed?.close()]);
  try {
    await daemon?.close();
  } finally {
    if (previousTmpdir === undefined) delete process.env.TMPDIR;
    else process.env.TMPDIR = previousTmpdir;
    await rm(scratch, { recursive: true, force: true, maxRetries: 3, retryDelay: 100 });
  }
  for (const result of closing) {
    if (result.status === "rejected") {
      console.error("Failed to close verification browser or SDK client", result.reason);
      process.exitCode = 1;
    }
  }
}
