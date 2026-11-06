import assert from "node:assert/strict";
import { execFile, spawn } from "node:child_process";
import { randomUUID } from "node:crypto";
import { access, mkdir, mkdtemp, readdir, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { promisify } from "node:util";
import {
  createPaseoClient,
  type PaseoAgentHandle,
  type PaseoAgentTimelineEvent,
} from "@getpaseo/client";
import { DaemonClient } from "@getpaseo/client/internal/daemon-client";
import { chromium, type Browser, type Page, type Locator } from "playwright";
import { expect } from "playwright/test";
import { WebSocket } from "ws";
import { createTestPaseoDaemon } from "../packages/server/src/server/test-utils/paseo-daemon.js";
import { createTestAgentClients } from "../packages/server/src/server/test-utils/fake-agent-client.js";
import { getHostName } from "../packages/server/src/server/host-name.js";

// node --import tsx scripts/run-nsh-rust-web.mts /path/to/nsh [appDir] [browserPath]
// --fixture-only skips packaging/browser UI: checks static hosting, Origin rules and the real daemon chat oracle.
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

async function completeTimeline(agent: PaseoAgentHandle) {
  let historyPage = await agent.timeline.refetch({
    direction: "tail",
    limit: 50,
    projection: "projected",
  });
  assert.equal(historyPage.error, null);
  const rows = new Map(
    historyPage.entries.map((entry) => [`${entry.seqStart}:${entry.seqEnd}`, entry]),
  );
  let pages = 1;
  while (historyPage.hasOlder) {
    const cursor = historyPage.startCursor;
    assert(cursor, "An older history page must have a start cursor");
    historyPage = await agent.timeline.refetch({
      direction: "before",
      cursor,
      limit: 50,
      projection: "projected",
    });
    assert.equal(historyPage.error, null);
    assert(
      historyPage.startCursor && historyPage.startCursor.seq < cursor.seq,
      "History cursor must advance",
    );
    for (const entry of historyPage.entries) rows.set(`${entry.seqStart}:${entry.seqEnd}`, entry);
    pages += 1;
  }
  return { entries: [...rows.values()], pages };
}

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
  const worktreeCwd = path.join(scratch, "NSH_RUST_WORKTREE");
  await mkdir(cwd);
  const git = promisify(execFile);
  const gitOptions = { cwd, timeout: 10_000 };
  await git("git", ["init", "--initial-branch=main"], gitOptions);
  await git(
    "git",
    [
      "-c",
      "user.name=NSH fixture",
      "-c",
      "user.email=nsh-fixture@example.invalid",
      "-c",
      "commit.gpgsign=false",
      "-c",
      "core.hooksPath=/dev/null",
      "commit",
      "--allow-empty",
      "-m",
      "Temporary NSH directory fixture",
    ],
    gitOptions,
  );
  await git("git", ["worktree", "add", "--detach", worktreeCwd, "HEAD"], gitOptions);
  const startedPrompts: string[] = [];
  let interruptions = 0;
  const permissionResponses: { behavior: string; selectedActionId?: string }[] = [];
  const permissionActions = [
    { id: "nsh-fixture-allow", label: "本次允许", behavior: "allow" as const },
    { id: "nsh-fixture-deny", label: "本次拒绝", behavior: "deny" as const },
  ];
  const agentClients = createTestAgentClients({
    onStartTurn: (prompt) => {
      if (typeof prompt === "string") startedPrompts.push(prompt);
    },
    onInterrupt: () => {
      interruptions += 1;
    },
  });
  const createSession = agentClients.codex.createSession.bind(agentClients.codex);
  agentClients.codex.createSession = async (...args) => {
    const session = await createSession(...args);
    const subscribe = session.subscribe.bind(session);
    session.subscribe = (callback) =>
      subscribe((event) => {
        if (event.type === "permission_requested") {
          // Mutate the fake provider's actual pending request, so refresh sees the same backend actions.
          event.request.actions = permissionActions;
        }
        callback(event);
      });
    const respond = session.respondToPermission.bind(session);
    session.respondToPermission = async (requestId, response) => {
      permissionResponses.push({ ...response });
      await respond(requestId, response);
    };
    return session;
  };
  // Deliberate fake catalog marker: proves the Rust draft RPC used the selected worktree cwd.
  // This is fixture metadata only, and never sends a prompt or invokes a model.
  agentClients.codex.listFeatures = async (config) => [
    { type: "toggle", id: "fixture_cwd", label: `Fixture cwd: ${config.cwd}`, value: true },
  ];
  daemon = await createTestPaseoDaemon({
    paseoHomeRoot: path.join(scratch, "daemon"),
    staticDir: path.join(scratch, "static"),
    cleanup: false,
    mcpEnabled: false,
    pluginsEnabled: false,
    agentClients,
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
  const worktree = await seed.workspaces.create({
    source: { kind: "directory", path: worktreeCwd, projectId: project.projectId },
  });
  const worktreeAgent = await worktree.agents.create({
    config: { provider: "codex/gpt-5.4-mini", modeId: "default" },
    title: "Rust worktree fixture (fake provider, no model)",
  });
  const workspaces = await seed.workspaces.list({
    filter: { projectId: project.projectId },
    page: { limit: 50 },
  });
  assert.equal(workspaces.pageInfo.hasMore, false);
  const workspace = workspaces.entries.find((entry) => entry.workspaceDirectory === worktreeCwd);
  assert(workspace, "The SDK must discover the actual worktree workspace");
  assert.notEqual(workspace.workspaceDirectory, workspace.projectRootPath);
  assert.equal(workspace.status, "done");
  assert.equal(workspace.projectId, project.projectId);
  const agents = await seed.agents.list({
    // The agent wire field projectKeys matches projectId, not project.list.projectKey.
    filter: { projectKeys: [project.projectId] },
    page: { limit: 50 },
  });
  assert.equal(agents.pageInfo.hasMore, false);
  const agentEntry = agents.entries.find((entry) => entry.agent.id === worktreeAgent.id);
  assert(agentEntry, "The SDK must discover the seeded Agent within the project directory");
  assert.equal(agentEntry.project.projectKey, project.projectId);
  const detail = (await worktreeAgent.refresh())?.agent;
  assert(detail, "The SDK must read the seeded Agent detail");
  assert.equal(detail.cwd, worktreeCwd);
  assert.equal(detail.status, "idle");
  const providers = await seed.providers.waitForReady({ cwd: worktreeCwd, timeoutMs: 15_000 });
  const provider = providers.entries.find((entry) => entry.provider === detail.provider);
  assert(provider && provider.enabled && provider.status === "ready");
  assert(
    provider.models?.length && provider.modes?.length,
    "Fake catalog must have real wire rows",
  );
  assert(detail.model, "The fake Agent must expose its selected model");
  const features = await seed.providers.listFeatures({
    provider: `${provider.provider}/${detail.model}`,
    cwd: worktreeCwd,
  });
  assert.equal(features.error, null);
  assert.deepEqual(features.features, [
    { type: "toggle", id: "fixture_cwd", label: `Fixture cwd: ${worktreeCwd}`, value: true },
  ]);
  // Real persisted daemon history, with an early page that cannot fit in the first 50 rows.
  const historyTurns = 64;
  for (let index = 0; index < historyTurns; index += 1) {
    const ordinal = String(index).padStart(3, "0");
    await daemon.daemon.agentManager.appendTimelineItem(worktreeAgent.id, {
      type: "user_message",
      text: `NSH_HISTORY_USER_${ordinal}`,
      messageId: `nsh-history-user-${ordinal}`,
    });
    await daemon.daemon.agentManager.appendTimelineItem(worktreeAgent.id, {
      type: "assistant_message",
      text: `NSH_HISTORY_REPLY_${ordinal}`,
    });
  }
  const history = await completeTimeline(worktreeAgent);
  assert(history.pages > 1, "The history fixture must require loading earlier pages");
  assert.equal(history.entries.length, historyTurns * 2);
  assert(
    history.entries.some(
      (entry) => entry.item.type === "user_message" && entry.item.text === "NSH_HISTORY_USER_000",
    ),
  );
  assert(
    history.entries.some(
      (entry) =>
        entry.item.type === "assistant_message" && entry.item.text === "NSH_HISTORY_REPLY_063",
    ),
  );
  const chat = {
    plain: "Respond with exactly: NSH_CHAT_STREAM_中文",
    plainReply: "NSH_CHAT_STREAM_中文",
    tool: "echo hello; Respond with exactly: NSH_CHAT_TOOL_OK",
    toolReply: "NSH_CHAT_TOOL_OK",
    denied: 'printf "ok" > nsh-chat-denied.txt; Respond with exactly: NSH_CHAT_DENIED_UNEXPECTED',
    cancelled:
      'printf "ok" > nsh-chat-cancelled.txt; Respond with exactly: NSH_CHAT_CANCEL_UNEXPECTED',
    after: "Respond with exactly: NSH_CHAT_AFTER_CANCEL",
    afterReply: "NSH_CHAT_AFTER_CANCEL",
    allow: permissionActions[0],
    deny: permissionActions[1],
  };
  const verifyChat = async () => {
    const fetched = await completeTimeline(worktreeAgent);
    for (let index = 0; index < historyTurns; index += 1) {
      const ordinal = String(index).padStart(3, "0");
      for (const text of [`NSH_HISTORY_USER_${ordinal}`, `NSH_HISTORY_REPLY_${ordinal}`]) {
        assert.equal(
          fetched.entries.filter(
            (entry) =>
              (entry.item.type === "user_message" || entry.item.type === "assistant_message") &&
              entry.item.text === text,
          ).length,
          1,
          "All seeded history must remain available exactly once",
        );
      }
    }
    for (const prompt of [chat.plain, chat.tool, chat.denied, chat.cancelled, chat.after]) {
      assert.equal(
        startedPrompts.filter((value) => value === prompt).length,
        1,
        "Each chat turn must start once",
      );
      assert.equal(
        fetched.entries.filter(
          (entry) => entry.item.type === "user_message" && entry.item.text === prompt,
        ).length,
        1,
        "Each sent message must persist once",
      );
    }
    for (const reply of [chat.plainReply, chat.toolReply, chat.afterReply]) {
      assert.equal(
        fetched.entries.filter(
          (entry) => entry.item.type === "assistant_message" && entry.item.text === reply,
        ).length,
        1,
        `Each assistant reply must persist exactly once: ${reply}`,
      );
    }
    for (const reply of ["NSH_CHAT_DENIED_UNEXPECTED", "NSH_CHAT_CANCEL_UNEXPECTED"]) {
      assert.equal(
        fetched.entries.some(
          (entry) => entry.item.type === "assistant_message" && entry.item.text === reply,
        ),
        false,
        "A denied or cancelled tool must not finish with its assistant reply",
      );
    }
    assert(
      fetched.entries.some(
        (entry) => entry.item.type === "tool_call" && entry.item.status === "completed",
      ),
    );
    assert(
      permissionResponses.some(
        (response) => response.behavior === "allow" && response.selectedActionId === chat.allow.id,
      ),
    );
    assert(
      permissionResponses.some(
        (response) => response.behavior === "deny" && response.selectedActionId === chat.deny.id,
      ),
    );
    assert(interruptions >= 1, "The provider must actually receive cancellation");
    await assert.rejects(access(path.join(worktreeCwd, "nsh-chat-denied.txt")));
    await assert.rejects(access(path.join(worktreeCwd, "nsh-chat-cancelled.txt")));
    const refreshed = (await worktreeAgent.refresh())?.agent;
    assert.equal(refreshed?.status, "idle");
    assert.equal(refreshed?.pendingPermissions.length, 0);
    assert.equal(refreshed?.cwd, worktreeCwd);
    assert.equal(refreshed?.workspaceId, workspace.id);
  };
  console.log(
    "Rust Web verification: daemon serves static site and WebSocket on one origin; fake Codex provider, no model",
  );
  if (command === "--fixture-only") {
    assert(html.includes("STATIC_FIXTURE_ONLY"));
    const driver = new DaemonClient({
      url,
      clientId: `rust-web-oracle-${randomUUID()}`,
      clientType: "cli",
      appVersion: "0.15.2",
      reconnect: { enabled: false },
    });
    try {
      await driver.connect();
      const tail = await driver.fetchAgentTimeline(worktreeAgent.id, {
        direction: "tail",
        projection: "projected",
        limit: 50,
        wholeTurns: true,
      });
      assert.equal(tail.error, null);
      assert.equal(tail.hasOlder, true);
      const stream: PaseoAgentTimelineEvent[] = [];
      const observation = worktreeAgent.timeline.subscribe((event) => stream.push(event));
      try {
        await observation.ready;
        assert.equal(
          (
            await worktreeAgent.run(chat.plain, {
              clientMessageId: randomUUID(),
              timeoutMs: 10_000,
            })
          ).status,
          "idle",
        );
        const tool = await worktreeAgent.run(chat.tool, {
          clientMessageId: randomUUID(),
          timeoutMs: 10_000,
        });
        assert.equal(tool.status, "permission");
        const request = tool.final?.pendingPermissions[0];
        assert(request);
        assert.deepEqual(request.actions, permissionActions);
        await worktreeAgent.respondToPermission({
          requestId: request.id,
          response: { behavior: "allow", selectedActionId: chat.allow.id },
        });
        assert.equal((await worktreeAgent.waitForFinish(10_000)).status, "idle");

        const denied = await worktreeAgent.run(chat.denied, {
          clientMessageId: randomUUID(),
          timeoutMs: 10_000,
        });
        assert.equal(denied.status, "permission");
        assert(denied.final?.pendingPermissions[0]);
        await worktreeAgent.respondToPermission({
          requestId: denied.final.pendingPermissions[0].id,
          response: { behavior: "deny", selectedActionId: chat.deny.id },
        });
        assert.equal((await worktreeAgent.waitForFinish(10_000)).status, "idle");

        const cancelled = await worktreeAgent.run(chat.cancelled, {
          clientMessageId: randomUUID(),
          timeoutMs: 10_000,
        });
        assert.equal(cancelled.status, "permission");
        await driver.cancelAgent(worktreeAgent.id);
        assert.equal((await worktreeAgent.waitForFinish(10_000)).status, "idle");
        assert.equal(
          (
            await worktreeAgent.run(chat.after, {
              clientMessageId: randomUUID(),
              timeoutMs: 10_000,
            })
          ).status,
          "idle",
        );
        assert(
          stream.some(
            (value) =>
              value.event.type === "timeline" &&
              value.event.item.type === "assistant_message" &&
              value.event.item.text === chat.plainReply.slice(0, 6),
          ),
          "The oracle must observe a streamed assistant fragment",
        );
        assert(stream.some((value) => value.event.type === "turn_canceled"));
        await verifyChat();
      } finally {
        await observation.release();
      }
    } finally {
      await driver.close();
    }
    console.log(
      JSON.stringify({
        staticHttp: 200,
        sameOriginWebSocket: 101,
        crossOriginWebSocket: 403,
        historyPages: history.pages,
        chatSendToolPermissionCancelVerified: true,
        fixture: "ready",
        wasmUiExecuted: false,
        modelInference: false,
      }),
    );
  } else {
    await mkdir(evidence, { recursive: true });
    browser = await chromium.launch({ executablePath: executable, headless: true });
    const context = await browser.newContext({ viewport: { width: 1000, height: 800 } });
    page = await context.newPage();
    page.setDefaultTimeout(20_000);
    const ui = page;
    const errors: string[] = [];
    const sockets: string[] = [];
    const hellos: { clientId?: string }[] = [];
    const incoming: PaseoAgentTimelineEvent[] = [];
    let historyReplies = 0;
    let projectReplies = 0;
    ui.on("pageerror", (error) => errors.push(error.message));
    ui.on("websocket", (socket) => {
      sockets.push(socket.url());
      socket.on("framesent", ({ payload }) => {
        if (typeof payload !== "string") return;
        const frame = JSON.parse(payload);
        if (frame.type === "hello") hellos.push({ clientId: frame.clientId });
      });
      socket.on("framereceived", ({ payload }) => {
        if (typeof payload !== "string") return;
        const frame = JSON.parse(payload);
        if (frame.type === "session" && frame.message?.type === "fetch_agent_timeline_response")
          historyReplies += 1;
        if (frame.type === "session" && frame.message?.type === "project.list.response")
          projectReplies += 1;
        if (
          frame.type === "session" &&
          frame.message?.type === "agent_stream" &&
          frame.message.payload.agentId === worktreeAgent.id
        ) {
          incoming.push(frame.message.payload);
        }
      });
    });
    const button = (name: string) => ui.getByRole("button", { name, exact: true });
    const visibleText = (value: string) =>
      ui.getByText(value, { exact: true }).filter({ visible: true });
    const address = ui.getByRole("textbox", { name: "主机地址", exact: true });
    const connect = button("连接主机");
    const refresh = button("刷新项目");
    const disconnect = button("断开连接");
    const projectButton = button(project.projectDisplayName);
    const connected = visibleText(`已连接：${getHostName()}`);
    const list = ui.getByTestId("conversation-messages");
    const permissions = ui.getByTestId("conversation-permissions");
    const message = ui.getByRole("textbox", { name: "消息", exact: true });
    const send = button("发送");
    const stop = button("停止");
    const allow = button(chat.allow.label);
    const deny = button(chat.deny.label);
    const older = button("加载更早消息");
    const historyCount = async () => {
      const text = await ui
        .getByText(/^已加载 [0-9]+ 条历史消息$/)
        .filter({ visible: true })
        .innerText();
      return Number(/^已加载 ([0-9]+) 条历史消息$/.exec(text.trim())?.[1]);
    };
    const synced = () => expect(visibleText("会话已同步")).toBeVisible({ timeout: 20_000 });
    const messageVisible = async (target: Locator) => {
      if (!(await target.isVisible())) return false;
      const [box, viewport] = await Promise.all([target.boundingBox(), list.boundingBox()]);
      return (
        !!box &&
        !!viewport &&
        box.height > 0 &&
        box.y < viewport.y + viewport.height &&
        box.y + box.height > viewport.y
      );
    };
    const reveal = async (text: string, direction: -1 | 1) => {
      const target = list.getByText(text, { exact: true });
      const limit = Math.max((await historyCount()) * 2, 20);
      for (let step = 0; step < limit && !(await messageVisible(target)); step += 1) {
        const box = await list.boundingBox();
        assert(box && box.height >= 24, "The actual browser message viewport must remain usable");
        const before = await list.evaluate((element) => element.scrollTop);
        await ui.mouse.move(box.x + box.width / 2, box.y + box.height / 2);
        await ui.mouse.wheel(0, direction * 160);
        await expect
          .poll(
            async () =>
              (await messageVisible(target)) ||
              Math.abs((await list.evaluate((element) => element.scrollTop)) - before) > 0.5,
            { timeout: 5_000 },
          )
          .toBe(true);
      }
      await expect.poll(() => messageVisible(target)).toBe(true);
      return target;
    };
    const loadEarlier = async (lastText: string, exactCount?: number) => {
      for (let index = 0; index < history.pages + 2 && (await older.isEnabled()); index += 1) {
        const before = await historyCount();
        await older.click();
        await expect.poll(historyCount, { timeout: 20_000 }).toBeGreaterThan(before);
        await synced();
      }
      await expect(older).toBeDisabled();
      if (exactCount !== undefined) assert.equal(await historyCount(), exactCount);
      await reveal("NSH_HISTORY_USER_000", -1);
      await reveal(lastText, 1);
    };
    const screenshot = (name: string) =>
      ui.screenshot({ path: path.join(evidence, `${name}.png`), fullPage: true });
    const composerUsable = async () => {
      await expect(message).toBeInViewport();
      await expect(send).toBeInViewport();
      await expect(stop).toBeInViewport();
      await expect(message).toBeEnabled();
      const box = await list.boundingBox();
      assert(box && box.height >= 24, "Permissions must preserve a readable message viewport");
    };
    const sendMessage = async (prompt: string) => {
      await message.fill(prompt);
      await expect(send).toBeEnabled();
      const before = await historyCount();
      await send.click();
      await expect.poll(historyCount, { timeout: 20_000 }).toBeGreaterThan(before);
    };
    const waitIdle = async () => {
      await expect(stop).toBeDisabled({ timeout: 20_000 });
      await synced();
    };
    const refreshChat = async () => {
      const before = historyReplies;
      await button("刷新会话").click();
      await expect.poll(() => historyReplies, { timeout: 20_000 }).toBeGreaterThan(before);
      await synced();
    };
    await ui.goto(`${origin}/`, { waitUntil: "networkidle", timeout: 60_000 });
    await expect(visibleText("连接开发主机")).toBeVisible();
    await expect(connect).toBeDisabled();
    await address.fill(url);
    await connect.click();
    await expect(projectButton).toBeVisible();
    await expect(connected).toBeVisible();
    await expect(address).toBeDisabled();
    await expect(connect).toBeDisabled();
    await expect(refresh).toBeEnabled();
    await expect.poll(() => hellos.length).toBe(1);
    const clientId = hellos[0].clientId;
    assert(
      clientId &&
        /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/.test(clientId),
    );
    await screenshot("connected");
    const beforeRefresh = projectReplies;
    await refresh.click();
    await expect.poll(() => projectReplies).toBeGreaterThan(beforeRefresh);
    await expect(connected).toBeVisible();
    await projectButton.click();
    await expect(visibleText(`已加载 ${workspaces.entries.length} 个工作区`)).toBeVisible();
    await expect(visibleText(`已加载 ${agents.entries.length} 个Agent`)).toBeVisible();
    // testID is emitted as data-testid by the packaged DOM runtime; assert the real node here.
    const workspaceButton = ui.getByTestId(`workspace:${workspace.id}`);
    await expect(workspaceButton).toHaveAttribute("role", "button");
    await workspaceButton.click();
    await expect(visibleText(`服务商目录：${worktreeCwd}`)).toBeVisible();
    await button(`服务商：${provider.label ?? provider.provider} · 可用`).click();
    await expect(visibleText(`Fixture cwd: ${worktreeCwd}：开启`)).toBeVisible();
    const agentButton = button(`Agent：${detail.title ?? detail.id} · 空闲`);
    await agentButton.click();
    await expect(
      ui.getByText(new RegExp(`^Agent ID：${detail.id}`)).filter({ visible: true }),
    ).toBeVisible();
    const openChat = button("打开会话");
    await openChat.click();
    await synced();
    await expect(list).toHaveAttribute("data-testid", "conversation-messages");
    assert.equal(await list.evaluate((element) => getComputedStyle(element).overflowY), "auto");
    await expect(older).toBeEnabled();
    assert((await historyCount()) < historyTurns * 2);
    await loadEarlier("NSH_HISTORY_REPLY_063", historyTurns * 2);
    await composerUsable();
    await screenshot("conversation-wide");

    await sendMessage(chat.plain);
    await reveal(chat.plainReply, 1);
    await waitIdle();
    await expect
      .poll(() =>
        incoming.some(
          (value) =>
            value.event.type === "timeline" &&
            value.event.item.type === "assistant_message" &&
            value.event.item.text === chat.plainReply.slice(0, 6),
        ),
      )
      .toBe(true);
    await sendMessage(chat.tool);
    await expect(allow).toBeVisible();
    await expect(stop).toBeEnabled();
    await expect(permissions).toBeVisible();
    await reveal("工具：shell · 运行中", 1);
    await composerUsable();
    await screenshot("permission-wide");
    await ui.setViewportSize({ width: 390, height: 600 });
    await composerUsable();
    await expect(allow).toBeInViewport();
    await message.fill("NSH_RESIZE_DRAFT");
    await expect(message).toHaveValue("NSH_RESIZE_DRAFT");
    await expect(send).toBeEnabled();
    await screenshot("permission-narrow");
    await message.fill("");
    await ui.setViewportSize({ width: 1000, height: 800 });
    await composerUsable();
    await allow.click();
    await expect(allow).toHaveCount(0);
    await reveal(chat.toolReply, 1);
    await waitIdle();
    await reveal("工具：shell · 已完成", -1);

    await sendMessage(chat.denied);
    await expect(deny).toBeVisible();
    await deny.click();
    await expect(deny).toHaveCount(0);
    await waitIdle();
    await sendMessage(chat.cancelled);
    await expect(allow).toBeVisible();
    await expect(stop).toBeEnabled();
    await stop.click();
    await expect(allow).toHaveCount(0);
    await waitIdle();
    await expect
      .poll(() => incoming.some((value) => value.event.type === "turn_canceled"))
      .toBe(true);
    await sendMessage(chat.after);
    await reveal(chat.afterReply, 1);
    await waitIdle();
    await screenshot("conversation-complete-wide");
    await ui.setViewportSize({ width: 390, height: 600 });
    await composerUsable();
    await message.fill("NSH_REOPEN_DRAFT");
    await expect(message).toHaveValue("NSH_REOPEN_DRAFT");
    await expect(send).toBeEnabled();
    await screenshot("conversation-narrow");
    await button("返回 Agent 详情").click();
    await openChat.click();
    await synced();
    await expect(message).toHaveValue("NSH_REOPEN_DRAFT");
    await loadEarlier(chat.afterReply);
    await reveal(chat.plainReply, -1);
    await refreshChat();
    await loadEarlier(chat.afterReply);
    await message.fill("");
    await button("返回 Agent 详情").click();
    await button("返回项目列表").click();
    await expect(connected).toBeVisible();
    await expect(address).toBeDisabled();
    await disconnect.click();
    await expect(visibleText("已断开连接")).toBeVisible();
    await expect(projectButton).toHaveCount(0);
    await expect(list).toHaveCount(0);
    await expect(message).toHaveCount(0);
    await expect(address).toBeEnabled();
    await expect(refresh).toBeDisabled();
    await screenshot("disconnected");
    await connect.click();
    await expect(projectButton).toBeVisible();
    await expect(connected).toBeVisible();
    await expect(address).toBeDisabled();
    await expect.poll(() => hellos.length).toBe(2);
    assert.equal(
      hellos[1].clientId,
      clientId,
      "Reconnect must reuse the persisted browser client identity",
    );
    await screenshot("reconnected");
    await disconnect.click();
    await expect(visibleText("已断开连接")).toBeVisible();
    assert.equal(new URL(ui.url()).origin, origin);
    assert.equal(sockets.length, 2);
    assert(sockets.every((socket) => new URL(socket).host === new URL(origin).host));
    assert.deepEqual(errors, []);
    await verifyChat();
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
