// Run with node --import tsx. Only a fresh loopback test daemon and fake provider are used.
import assert from "node:assert/strict";
import { mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { once } from "node:events";
import WebSocket from "ws";
import {
  AgentCreateResponseSchema,
  SessionInboundMessageSchema,
  WSOutboundMessageSchema,
} from "@getpaseo/protocol/messages";
import { createTestPaseoDaemon } from "../../server/src/server/test-utils/paseo-daemon.js";
import { createTestAgentClients } from "../../server/src/server/test-utils/fake-agent-client.js";

const scratch = await mkdtemp(path.join(tmpdir(), "paseo-conversation-"));
const previousTmpdir = process.env.TMPDIR;
process.env.TMPDIR = scratch;
const cwd = path.join(scratch, "project");
const frames: { direction: string; frame: unknown }[] = [];
const responses = new Map<string, unknown>();
let releaseTurn: (() => void) | undefined;
let interrupts = 0;
let daemon: Awaited<ReturnType<typeof createTestPaseoDaemon>> | undefined;
let socket: WebSocket | undefined;
let helloReady = false;
const serverKinds = new Set([
  "agent.create.response",
  "agent.create.update",
  "creation.subscribe.response",
  "session.events.set_subscription.response",
  "agent.timeline.set_subscription.response",
  "send_agent_message_response",
  "cancel_agent_response",
  "agent_stream",
  "agent_permission_request",
  "agent_permission_resolved",
  "agent.timeline.replacement",
]);
async function until(predicate: () => boolean) {
  const deadline = Date.now() + 10_000;
  while (!predicate()) {
    assert(Date.now() < deadline, "Timed out waiting for the fake daemon");
    await new Promise((resolve) => setTimeout(resolve, 10));
  }
}
function send(message: unknown) {
  const frame = { type: "session", message: SessionInboundMessageSchema.parse(message) };
  frames.push({ direction: "client", frame });
  socket!.send(JSON.stringify(frame));
  return frame.message;
}
async function rpc(message: unknown, responseType: string) {
  const parsed = send(message);
  assert("requestId" in parsed && typeof parsed.requestId === "string");
  const key = `${responseType}:${parsed.requestId}`;
  await until(() => responses.has(key));
  return responses.get(key);
}
try {
  await mkdir(cwd);
  daemon = await createTestPaseoDaemon({
    paseoHomeRoot: path.join(scratch, "daemon"),
    staticDir: path.join(scratch, "static"),
    cleanup: false,
    mcpEnabled: false,
    pluginsEnabled: false,
    agentClients: createTestAgentClients({
      beforeTurnComplete: async (prompt) => {
        if (prompt === "Respond with exactly: CHAT_CANCEL_READY") {
          await new Promise<void>((resolve) => {
            releaseTurn = resolve;
          });
        }
      },
      onInterrupt: () => {
        interrupts += 1;
        releaseTurn?.();
      },
    }),
  });
  socket = new WebSocket(`ws://127.0.0.1:${daemon.port}/ws`);
  socket.on("message", (data) => {
    const raw: unknown = JSON.parse(data.toString());
    const frame = WSOutboundMessageSchema.parse(raw);
    if (frame.type !== "session") return;
    if (frame.message.type === "status" && frame.message.payload.status === "server_info")
      helloReady = true;
    if ("payload" in frame.message && "requestId" in frame.message.payload) {
      responses.set(
        `${frame.message.type}:${frame.message.payload.requestId}`,
        frame.message.payload,
      );
    }
    if (serverKinds.has(frame.message.type)) frames.push({ direction: "server", frame: raw });
  });
  await once(socket, "open");
  socket.send(
    JSON.stringify({
      type: "hello",
      clientId: "conversation-oracle",
      clientType: "browser",
      protocolVersion: 1,
      capabilities: { hello_rejection: true },
    }),
  );
  await until(() => helloReady);
  await rpc(
    {
      type: "session.events.set_subscription.request",
      requestId: "chat-events",
      events: ["agent_permission_request", "agent_permission_resolved"],
      notifications: false,
    },
    "session.events.set_subscription.response",
  );
  const created = AgentCreateResponseSchema.parse({
    type: "agent.create.response",
    payload: await rpc(
      {
        type: "agent.create.request",
        requestId: "chat-create",
        idempotencyKey: "conversation-create",
        subscribe: true,
        config: { provider: "codex", cwd, modeId: "full-access", title: "Conversation fixture" },
      },
      "agent.create.response",
    ),
  }).payload;
  assert.equal(created.error, null);
  assert(created.agent);
  const agentId = created.agent.id;
  await rpc(
    {
      type: "creation.subscribe.request",
      requestId: "chat-observe",
      kind: "agent",
      idempotencyKey: "conversation-create",
      subscribe: true,
    },
    "creation.subscribe.response",
  );
  await rpc(
    {
      type: "creation.subscribe.request",
      requestId: "chat-observe-missing",
      kind: "agent",
      idempotencyKey: "conversation-not-created",
      subscribe: false,
    },
    "creation.subscribe.response",
  );
  await rpc(
    {
      type: "agent.timeline.set_subscription.request",
      requestId: "chat-timeline",
      agentIds: [agentId],
    },
    "agent.timeline.set_subscription.response",
  );
  await rpc(
    {
      type: "send_agent_message_request",
      requestId: "chat-send",
      agentId,
      messageId: "chat-message",
      text: "Respond with exactly: CHAT_CANCEL_READY",
    },
    "send_agent_message_response",
  );
  await until(() => releaseTurn !== undefined);
  await rpc(
    { type: "cancel_agent_request", requestId: "chat-cancel", agentId },
    "cancel_agent_response",
  );
  assert.equal(interrupts, 1, "Cancel must interrupt the held provider turn");
  await rpc(
    {
      type: "send_agent_message_request",
      requestId: "chat-followup",
      agentId,
      messageId: "chat-followup-message",
      text: "Respond with exactly: CHAT_FOLLOWUP",
    },
    "send_agent_message_response",
  );
  await rpc(
    {
      type: "send_agent_message_request",
      requestId: "chat-rejected",
      agentId: "missing-chat-agent",
      messageId: "chat-rejected-message",
      text: "Not delivered",
    },
    "send_agent_message_response",
  );
  await rpc(
    {
      type: "cancel_agent_request",
      requestId: "chat-cancel-missing",
      agentId: "missing-chat-agent",
    },
    "cancel_agent_response",
  );
  await rpc(
    {
      type: "agent.create.request",
      requestId: "chat-workspace-create",
      workspaceId: created.agent.workspaceId,
      config: { provider: "codex", cwd, modeId: "default" },
      subscribe: false,
    },
    "agent.create.response",
  );
  // Keep actual daemon wire values except the temporary directory; never synthesize a server frame.
  const sanitized = JSON.parse(JSON.stringify(frames).replaceAll(scratch, "/fixture"));
  await writeFile(
    new URL("./conversation.json", import.meta.url),
    JSON.stringify(sanitized, null, 2) + "\n",
  );
  console.log(`Captured ${frames.length} real fake-provider daemon conversation frames`);
} finally {
  releaseTurn?.();
  socket?.close();
  await daemon?.close();
  if (previousTmpdir === undefined) delete process.env.TMPDIR;
  else process.env.TMPDIR = previousTmpdir;
  await rm(scratch, { recursive: true, force: true, maxRetries: 3, retryDelay: 100 });
}
