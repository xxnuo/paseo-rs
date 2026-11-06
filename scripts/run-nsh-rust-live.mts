import assert from "node:assert/strict";
import { execFile, spawn } from "node:child_process";
import { randomUUID } from "node:crypto";
import { access, mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
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
import { createTestPaseoDaemon } from "../packages/server/src/server/test-utils/paseo-daemon.js";
import { createTestAgentClients } from "../packages/server/src/server/test-utils/fake-agent-client.js";
import { getHostName } from "../packages/server/src/server/host-name.js";

// node --import tsx scripts/run-nsh-rust-live.mts /path/to/nsh [linux|headless|web|...] [appDir]
// --fixture-only verifies real SDK fixture setup without claiming Rust UI coverage.
// --self-test-adb checks forwarding ownership using a fake adb, without touching a device.
const command = process.argv[2];
assert(command, "Pass the built nsh binary path, or --fixture-only to verify fixture setup");
type RunAdb = (args: string[]) => Promise<string>;
interface AndroidReverse {
  run: RunAdb;
  serial: string;
  port: string;
}

async function completeTimeline(agent: PaseoAgentHandle) {
  let page = await agent.timeline.refetch({
    direction: "tail",
    limit: 50,
    projection: "projected",
  });
  assert.equal(page.error, null);
  const rows = new Map(page.entries.map((entry) => [`${entry.seqStart}:${entry.seqEnd}`, entry]));
  let pages = 1;
  while (page.hasOlder) {
    const cursor = page.startCursor;
    assert(cursor, "An older history page must have a start cursor");
    page = await agent.timeline.refetch({
      direction: "before",
      cursor,
      limit: 50,
      projection: "projected",
    });
    assert.equal(page.error, null);
    assert(page.startCursor && page.startCursor.seq < cursor.seq, "History cursor must advance");
    for (const entry of page.entries) rows.set(`${entry.seqStart}:${entry.seqEnd}`, entry);
    pages += 1;
  }
  return { entries: [...rows.values()], pages };
}

function mappingTarget(list: string, port: string): string | undefined {
  return list
    .split(/\r?\n/)
    .map((line) => line.trim().split(/\s+/))
    .find((fields) => fields[1] === port)?.[2];
}

async function installReverse(run: RunAdb, daemonPort: number): Promise<AndroidReverse> {
  // Match nsh-cli/android.rs::device exactly; NSH does not use ANDROID_SERIAL to select a device.
  const devices = (await run(["devices"])).split(/\r?\n/).slice(1);
  const serial = devices
    .map((line) => line.split("\t"))
    .find((fields) => fields[1] === "device")?.[0];
  assert(serial, "No ready Android device found in adb devices (the same selection as NSH)");
  const port = `tcp:${daemonPort}`;
  const existing = mappingTarget(await run(["-s", serial, "reverse", "--list"]), port);
  assert.equal(
    existing,
    undefined,
    `ADB reverse ${port} already exists on ${serial}; refusing to replace it`,
  );
  await run(["-s", serial, "reverse", "--no-rebind", port, port]);
  return { run, serial, port };
}

async function removeReverse(reverse: AndroidReverse): Promise<void> {
  const { run, serial, port } = reverse;
  const current = mappingTarget(await run(["-s", serial, "reverse", "--list"]), port);
  if (current === undefined) return;
  assert.equal(current, port, `ADB reverse ${port} changed; refusing to remove another mapping`);
  await run(["-s", serial, "reverse", "--remove", port]);
}

async function androidReverse(daemonPort: number): Promise<AndroidReverse> {
  const home = process.platform === "win32" ? process.env.LOCALAPPDATA : process.env.HOME;
  const defaultSdk = home
    ? path.join(
        home,
        ...(process.platform === "win32" ? ["Android", "Sdk"] : ["Library", "Android", "sdk"]),
      )
    : undefined;
  const sdk = process.env.ANDROID_HOME ?? process.env.ANDROID_SDK_ROOT ?? defaultSdk;
  assert(sdk, "Android SDK not found; set ANDROID_HOME as required by NSH");
  const adb = path.join(sdk, "platform-tools", "adb");
  await access(adb);
  const execute = promisify(execFile);
  const run: RunAdb = async (args) =>
    (await execute(adb, args, { encoding: "utf8", timeout: 10_000 })).stdout;
  const reverse = await installReverse(run, daemonPort);
  console.log(`Android fixture reverse: ${reverse.serial} ${reverse.port} -> ${reverse.port}`);
  return reverse;
}

async function selfTestAdb(): Promise<void> {
  const commands: string[][] = [];
  let mapping = "UsbFfs tcp:9999 tcp:9999\n";
  const run: RunAdb = async (args) => {
    commands.push(args);
    if (args[0] === "devices")
      return "List of devices attached\noffline\toffline\nfirst\tdevice\nsecond\tdevice\n";
    if (args.at(-1) === "--list") return mapping;
    if (args.includes("--no-rebind")) mapping += "UsbFfs tcp:42671 tcp:42671\n";
    if (args.includes("--remove")) mapping = "UsbFfs tcp:9999 tcp:9999\n";
    return "";
  };
  const reverse = await installReverse(run, 42671);
  assert.equal(reverse.serial, "first");
  await removeReverse(reverse);
  assert.deepEqual(commands, [
    ["devices"],
    ["-s", "first", "reverse", "--list"],
    ["-s", "first", "reverse", "--no-rebind", "tcp:42671", "tcp:42671"],
    ["-s", "first", "reverse", "--list"],
    ["-s", "first", "reverse", "--remove", "tcp:42671"],
  ]);
  assert.equal(mapping, "UsbFfs tcp:9999 tcp:9999\n");
  commands.length = 0;
  mapping = "UsbFfs tcp:42671 tcp:1234\n";
  await assert.rejects(installReverse(run, 42671), /already exists/);
  assert.equal(
    commands.some((args) => args.includes("--no-rebind")),
    false,
  );
  await assert.rejects(removeReverse(reverse), /changed/);
  assert.equal(
    commands.some((args) => args.includes("--remove")),
    false,
  );
  await assert.rejects(
    installReverse(async () => "List of devices attached\none\tunauthorized\n", 42671),
    /No ready Android device/,
  );
}

if (command === "--self-test-adb") {
  await selfTestAdb();
  console.log(
    "ADB self-check passed: first ready device, scoped creation/removal, conflict preservation",
  );
  process.exit(0);
}

const target = process.argv[3] ?? "linux";
const repository = fileURLToPath(new URL("../", import.meta.url));
const appDirectory = path.resolve(process.argv[4] ?? path.join(repository, "packages/app-nsh"));
const testFile = path.join(appDirectory, "probes", `.rust-live-${randomUUID()}.test.ts`);
const scratch = await mkdtemp(path.join(tmpdir(), "paseo-nsh-rust-live-"));
const previousTmpdir = process.env.TMPDIR;
process.env.TMPDIR = scratch;
let daemon: Awaited<ReturnType<typeof createTestPaseoDaemon>> | undefined;
let seed: ReturnType<typeof createPaseoClient> | undefined;
let reverse: AndroidReverse | undefined;
try {
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
  const explicitModel = provider.models.find((model) => model.isSelectable !== false);
  const explicitMode = provider.modes.find((mode) => mode.id !== provider.defaultModeId);
  assert(explicitModel && explicitMode, "The fake provider must offer explicit selectable options");
  const uiCreation = {
    defaultsTitle: "NSH_UI_DEFAULTS",
    explicitTitle: "NSH_UI_EXPLICIT",
    storageKey: `paseo.nsh.creation:${JSON.stringify([daemon.daemon.getServerId(), workspace.id, provider.provider])}`,
    model: explicitModel.id,
    mode: explicitMode.id,
    modelButton: `role:button[模型：${explicitModel.label}]`,
    selectedModel: `role:button[模型：${explicitModel.label}（已选择）]`,
    modeButton: `role:button[模式：${explicitMode.label}]`,
    selectedMode: `role:button[模式：${explicitMode.label}（已选择）]`,
  };
  const directoryOracle = {
    // The button also names its cwd; press the title text's nearest pressable ancestor.
    workspace: `text:工作区：${workspace.name} · 空闲`,
    provider: `role:button[服务商：${provider.label ?? provider.provider} · 可用]`,
    cwd: `text:服务商目录：${worktreeCwd}`,
    catalog: `text:服务商 ID：${provider.provider}\n模型\n${provider.models.map((model) => `${model.label}（${model.id}）`).join("\n")}\n模式\n${provider.modes.map((mode) => `${mode.label}（${mode.id}）`).join("\n")}`,
    features: `text:Fixture cwd: ${worktreeCwd}：开启`,
    agent: `role:button[Agent：${agentEntry.agent.title ?? agentEntry.agent.id} · 空闲]`,
    detail: `text:Agent ID：${detail.id}\n服务商：${detail.provider}\n模型：${detail.model ?? "未指定"}\n模式：${detail.currentModeId ?? "未指定"}\n状态：空闲\n工作目录：${detail.cwd}`,
    workspaces: `text:已加载 ${workspaces.entries.length} 个工作区`,
    agents: `text:已加载 ${agents.entries.length} 个Agent`,
  };
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
      assert(
        fetched.entries.some(
          (entry) => entry.item.type === "assistant_message" && entry.item.text === reply,
        ),
        `Missing persisted assistant reply: ${reply}`,
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
  console.log("Rust UI fixture: real loopback daemon and SDK seed, fake Codex provider (no model)");
  if (command === "--fixture-only") {
    // The public SDK requires provider/model, so use its shipped low-level driver to omit model/mode.
    const driver = new DaemonClient({
      url,
      clientId: `nsh-creation-oracle-${randomUUID()}`,
      clientType: "cli",
      appVersion: "0.15.2",
      reconnect: { enabled: false },
    });
    try {
      await driver.connect();
      const wholeTurnTail = await driver.fetchAgentTimeline(worktreeAgent.id, {
        direction: "tail",
        projection: "projected",
        limit: 50,
        wholeTurns: true,
      });
      assert.equal(wholeTurnTail.error, null);
      assert.equal(
        wholeTurnTail.hasOlder,
        true,
        "Rust's whole-turn history mode must still paginate this fixture",
      );
      assert.equal(wholeTurnTail.entries.length, 50);
      const defaults = {
        config: { provider: provider.provider, cwd: worktreeCwd, title: "NSH_ORACLE_DEFAULTS" },
        workspaceId: workspace.id,
        idempotencyKey: randomUUID(),
      };
      assert.equal(Object.hasOwn(defaults.config, "model"), false);
      assert.equal(Object.hasOwn(defaults.config, "modeId"), false);
      const created = await driver.createAgent(defaults);
      const repeated = await driver.createAgent(defaults);
      assert.equal(
        repeated.id,
        created.id,
        "Reusing a confirmed key must not create another Agent",
      );
      assert.equal(created.cwd, worktreeCwd);
      assert.equal(created.workspaceId, workspace.id);
      const explicit = await driver.createAgent({
        config: {
          ...defaults.config,
          title: "NSH_ORACLE_EXPLICIT",
          model: uiCreation.model,
          modeId: uiCreation.mode,
        },
        workspaceId: workspace.id,
        idempotencyKey: randomUUID(),
      });
      assert.equal(explicit.cwd, worktreeCwd);
      assert.equal(explicit.workspaceId, workspace.id);
      assert.equal(explicit.model, uiCreation.model);
      assert.equal(explicit.currentModeId, uiCreation.mode);
      const after = await seed.agents.list({ filter: { projectKeys: [project.projectId] } });
      assert.equal(after.entries.length, agents.entries.length + 2);

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
        projectDisplayName: project.projectDisplayName,
        workspaces: workspaces.entries.length,
        agents: agents.entries.length,
        worktreeDiffersFromProjectRoot: true,
        provider: provider.provider,
        featureContextVerified: true,
        defaultAndExplicitCreationVerified: true,
        sameKeyDeduplicated: true,
        historyPages: history.pages,
        chatSendToolPermissionCancelVerified: true,
        fixture: "ready",
        rustUiExecuted: false,
      }),
    );
  } else {
    if (target === "android") reverse = await androidReverse(daemon.port);
    const failureScreenshot = path.join(
      appDirectory,
      ".nsh",
      "evidence",
      `project-connect-${target}-${randomUUID()}.png`,
    );
    await mkdir(path.dirname(failureScreenshot), { recursive: true });
    const permissionScreenshot = failureScreenshot.replace("project-connect-", "chat-permission-");
    const conversationScreenshot = failureScreenshot.replace("project-connect-", "chat-complete-");
    await writeFile(
      testFile,
      `import { expect, open, test } from "nsh/test";

test("Rust workspace creates Agents and chats through real daemon RPCs without model inference", async () => {
  const app = await open("..", { network: "live" });
  const project = ${JSON.stringify(`text:${project.projectDisplayName}`)};
  const connected = ${JSON.stringify(`text:已连接：${getHostName()}`)};
  const address = "label:主机地址";
  const connect = "role:button[连接主机]";
  const refresh = "role:button[刷新项目]";
  const disconnect = "role:button[断开连接]";
  const directory = ${JSON.stringify(directoryOracle)};
  const projectPage = ${JSON.stringify(`text:项目：${project.projectDisplayName}`)};
  const projectButton = ${JSON.stringify(`role:button[${project.projectDisplayName}]`)};
  const creation = ${JSON.stringify(uiCreation)};
  const chat = ${JSON.stringify(chat)};
  await app.waitFor("连接开发主机");
  expect(app.enabled(connect)).toBe(false);
  expect(app.enabled(refresh)).toBe(false);
  expect(app.enabled(disconnect)).toBe(false);
  await app.type(address, ${JSON.stringify(url)});
  await app.waitUntil(() => app.enabled(connect));
  expect(app.enabled(connect)).toBe(true);
  await app.press(connect);
  // Android touch dispatch can finish before the engine publishes the callback's frame.
  try {
    await app.waitUntil(() => !app.enabled(address) && !app.enabled(connect));
  } catch (error) {
    await app.screenshot(${JSON.stringify(failureScreenshot)});
    console.log("Connection failure screenshot: " + ${JSON.stringify(failureScreenshot)});
    throw error;
  }
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

  await app.press(projectButton);
  await app.waitFor(projectPage);
  await app.waitFor(directory.workspaces, { timeout: 15000 });
  await app.waitFor(directory.agents, { timeout: 15000 });
  expect(app.enabled("role:button[加载更多工作区]")).toBe(false);
  expect(app.enabled("role:button[加载更多Agent]")).toBe(false);
  await app.press("role:button[刷新工作区]");
  await app.waitFor(directory.workspaces, { timeout: 15000 });
  await app.press("role:button[刷新Agent]");
  await app.waitFor(directory.agents, { timeout: 15000 });
  await app.press(directory.workspace);
  await app.waitFor(directory.cwd);
  await app.waitFor(directory.provider, { timeout: 15000 });
  await app.press(directory.provider);
  await app.waitFor(directory.catalog);
  await app.waitFor(directory.features, { timeout: 15000 });
  await app.press("role:button[刷新服务商功能]");
  await app.waitFor(directory.features, { timeout: 15000 });
  const create = "role:button[创建 Agent]";
  const title = "label:Agent 名称（可选）";
  await app.waitFor("role:button[模型：服务商默认（已选择）]");
  await app.waitFor("role:button[模式：服务商默认（已选择）]");
  expect(app.enabled(create)).toBe(true);
  await app.type(title, creation.defaultsTitle);
  await app.press(create);
  await app.waitFor("text:/^已创建 Agent：/", { timeout: 20000 });
  const firstCompletion = app.texts().find((value) => value.startsWith("已创建 Agent："));
  expect(typeof firstCompletion).toBe("string");
  expect(app.enabled("role:button[查询创建结果]")).toBe(false);
  expect(app.storage()[creation.storageKey]).toBe(undefined);
  await app.press("role:button[刷新Agent]");
  await app.waitFor("role:button[Agent：" + creation.defaultsTitle + " · 空闲]", { timeout: 15000 });

  await app.clear(title);
  await app.type(title, creation.explicitTitle);
  await app.press(creation.modelButton);
  await app.waitFor(creation.selectedModel);
  await app.press(creation.modeButton);
  await app.waitFor(creation.selectedMode);
  expect(app.shows(creation.selectedModel)).toBe(true);
  expect(app.shows(creation.selectedMode)).toBe(true);
  await app.press(create);
  await app.waitUntil(() => !app.shows("text:" + firstCompletion));
  await app.waitFor("text:/^已创建 Agent：/", { timeout: 20000 });
  expect(app.texts().find((value) => value.startsWith("已创建 Agent：")) === firstCompletion).toBe(false);
  expect(app.enabled("role:button[查询创建结果]")).toBe(false);
  expect(app.storage()[creation.storageKey]).toBe(undefined);
  await app.press("role:button[刷新Agent]");
  await app.waitFor("role:button[Agent：" + creation.explicitTitle + " · 空闲]", { timeout: 15000 });
  await app.press(directory.agent);
  await app.waitFor(directory.detail, { timeout: 15000 });
  await app.press("role:button[刷新 Agent 详情]");
  await app.waitFor(directory.detail, { timeout: 15000 });

  const openChat = "role:button[打开会话]";
  const returnDetail = "role:button[返回 Agent 详情]";
  const message = "label:消息";
  const send = "role:button[发送]";
  const stop = "role:button[停止]";
  const older = "role:button[加载更早消息]";
  const allow = "role:button[" + chat.allow.label + "]";
  const deny = "role:button[" + chat.deny.label + "]";
  const earliest = "text:NSH_HISTORY_USER_000";
  const latest = "text:NSH_HISTORY_REPLY_063";
  const historyCount = () => {
    const match = app.texts().map((value) => /^已加载 ([0-9]+) 条历史消息$/.exec(value.trim())).find(Boolean);
    return match ? Number(match[1]) : 0;
  };
  const revealMessage = async (selector: string, direction: number) => {
    // Virtual rows must first be materialized by real scroll gestures; text queries cannot seek them.
    for (let step = 0; step < Math.max(historyCount() * 2, 20) && !app.shows(selector); step += 1) {
      await app.scroll("id:conversation-messages", direction * 160);
    }
    await app.waitFor(selector);
  };
  const loadEarlier = async (lastMessage: string, expectedRows?: number) => {
    for (let page = 0; page < ${history.pages + 1} && app.enabled(older); page += 1) {
      await app.waitUntil(() => app.enabled(older));
      const before = historyCount();
      await app.press(older);
      await app.waitUntil(() => historyCount() > before);
    }
    await app.waitUntil(() => !app.enabled(older));
    if (expectedRows !== undefined) expect(historyCount()).toBe(expectedRows);
    await revealMessage(earliest, -1);
    await revealMessage(lastMessage, 1);
  };
  const sendMessage = async (prompt: string) => {
    await app.clear(message);
    await app.type(message, prompt);
    await app.waitUntil(() => app.enabled(send));
    const before = historyCount();
    await app.press(send);
    await app.waitUntil(() => historyCount() > before);
  };
  const waitIdle = async () => {
    await app.waitUntil(() => app.enabled(message) && app.shows(stop) && !app.enabled(stop));
  };
  await app.press(openChat);
  await app.waitFor("text:会话已同步", { timeout: 15000 });
  expect(app.enabled(older)).toBe(true);
  expect(historyCount() < ${historyTurns * 2}).toBe(true);
  await loadEarlier(latest, ${historyTurns * 2});
  await sendMessage(chat.plain);
  await waitIdle();
  await revealMessage("text:" + chat.plainReply, 1);

  await sendMessage(chat.tool);
  await app.waitFor(allow, { timeout: 15000 });
  expect(app.enabled(stop)).toBe(true);
  await app.screenshot(${JSON.stringify(permissionScreenshot)});
  await revealMessage("text:/shell/", 1);
  await app.press(allow);
  await app.waitUntil(() => !app.shows(allow));
  await waitIdle();
  await revealMessage("text:" + chat.toolReply, 1);
  await app.waitFor("text:/shell/");

  await sendMessage(chat.denied);
  await app.waitFor(deny, { timeout: 15000 });
  await app.press(deny);
  await app.waitUntil(() => !app.shows(deny));
  await waitIdle();
  expect(app.shows("text:NSH_CHAT_DENIED_UNEXPECTED")).toBe(false);

  await sendMessage(chat.cancelled);
  await app.waitFor(allow, { timeout: 15000 });
  expect(app.enabled(stop)).toBe(true);
  await app.press(stop);
  await app.waitUntil(() => !app.shows(allow));
  await waitIdle();
  expect(app.shows("text:NSH_CHAT_CANCEL_UNEXPECTED")).toBe(false);
  await sendMessage(chat.after);
  await waitIdle();
  await revealMessage("text:" + chat.afterReply, 1);
  await app.screenshot(${JSON.stringify(conversationScreenshot)});
  console.log("Chat screenshots: " + ${JSON.stringify(permissionScreenshot)} + ", " + ${JSON.stringify(conversationScreenshot)});
  await app.press("role:button[刷新会话]");
  await app.waitFor("text:会话已同步", { timeout: 15000 });
  await revealMessage("text:" + chat.afterReply, 1);
  await app.press(returnDetail);
  await app.waitFor(directory.detail);
  await app.press(openChat);
  await app.waitFor("text:会话已同步", { timeout: 15000 });
  await loadEarlier("text:" + chat.afterReply);
  await revealMessage("text:" + chat.plainReply, -1);
  await app.press(returnDetail);
  await app.waitFor(directory.detail);
  await app.press("role:button[返回项目列表]");
  await app.waitFor(connected);
  expect(app.enabled(address)).toBe(false);
  expect(app.enabled(connect)).toBe(false);
  expect(app.storage()["paseo.nsh.client-id"]).toBe(clientId);
  expect(app.shows(projectPage)).toBe(false);
  await app.press(projectButton);
  await app.waitFor(projectPage);
  await app.waitFor(directory.workspaces, { timeout: 15000 });
  await app.waitFor(${JSON.stringify(`text:已加载 ${agents.entries.length + 2} 个Agent`)}, { timeout: 15000 });
  await app.press(directory.workspace);
  await app.waitFor(directory.cwd);
  await app.waitFor(directory.provider, { timeout: 15000 });
  await app.press(directory.provider);
  await app.waitFor(directory.features, { timeout: 15000 });
  await app.press(directory.agent);
  await app.waitFor(directory.detail, { timeout: 15000 });
  await app.press(disconnect);
  await app.waitFor("text:已断开连接");
  expect(app.shows(project)).toBe(false);
  expect(app.shows(projectPage)).toBe(false);
  expect(app.shows(directory.cwd)).toBe(false);
  expect(app.shows(directory.detail)).toBe(false);
  expect(app.enabled(address)).toBe(true);
  expect(app.enabled(connect)).toBe(true);
  expect(app.enabled(refresh)).toBe(false);
  expect(app.enabled(disconnect)).toBe(false);

  await app.press(connect);
  await app.waitUntil(() => !app.enabled(address) && !app.enabled(connect));
  expect(app.enabled(address)).toBe(false);
  await app.waitFor(project, { timeout: 20000 });
  await app.waitFor(connected);
  expect(app.storage()["paseo.nsh.client-id"]).toBe(clientId);
  await app.press(disconnect);
  await app.waitFor("text:已断开连接");
  expect(app.shows(project)).toBe(false);
}, 300000);
`,
      { flag: "wx" },
    );
    const args = ["test", testFile];
    if (target !== "headless") args.push("--target", target);
    await new Promise<void>((resolve, reject) => {
      const child = spawn(path.resolve(command), args, { cwd: appDirectory, stdio: "inherit" });
      const timeout = setTimeout(() => child.kill("SIGKILL"), 420_000);
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
    const after = await seed.agents.list({ filter: { projectKeys: [project.projectId] } });
    assert.equal(after.pageInfo.hasMore, false);
    assert.equal(
      after.entries.length,
      agents.entries.length + 2,
      "UI must create exactly two Agents",
    );
    for (const title of [uiCreation.defaultsTitle, uiCreation.explicitTitle]) {
      const matches = after.entries.filter((entry) => entry.agent.title === title);
      assert.equal(matches.length, 1, `UI-created title must be unique: ${title}`);
      const actual = (await seed.agents.ref(matches[0].agent.id).refresh())?.agent;
      assert(actual, "UI-created Agent must have a real daemon detail");
      assert.equal(actual.cwd, worktreeCwd);
      assert.equal(actual.workspaceId, workspace.id);
      assert.equal(actual.provider, provider.provider);
      if (title === uiCreation.explicitTitle) {
        assert.equal(actual.model, uiCreation.model);
        assert.equal(actual.currentModeId, uiCreation.mode);
      }
    }
    await verifyChat();
    console.log(
      "Rust UI creation verified by SDK: two unique worktree Agents, defaults and explicit options",
    );
    console.log(
      "Rust UI chat verified by SDK: paged history, tool actions, cancellation and unique persisted messages",
    );
  }
} finally {
  const closing = await Promise.allSettled([
    seed?.close(),
    reverse ? removeReverse(reverse) : undefined,
  ]);
  try {
    await daemon?.close();
  } finally {
    if (previousTmpdir === undefined) delete process.env.TMPDIR;
    else process.env.TMPDIR = previousTmpdir;
    await rm(testFile, { force: true });
    await rm(scratch, { recursive: true, force: true, maxRetries: 3, retryDelay: 100 });
  }
  for (const result of closing) {
    if (result.status === "rejected") {
      console.error(
        "Failed to close SDK fixture or remove its Android reverse mapping",
        result.reason,
      );
      process.exitCode = 1;
    }
  }
}
