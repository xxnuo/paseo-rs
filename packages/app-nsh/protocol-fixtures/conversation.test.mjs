import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { test } from "node:test";
import {
  WSInboundMessageSchema,
  WSOutboundMessageSchema,
  AgentPermissionRequestPayloadSchema,
  AgentPermissionResponseSchema,
  AgentStreamEventPayloadSchema,
  AgentTimelineItemPayloadSchema,
} from "../../protocol/src/messages.ts";

// conversation.json is captured by capture-conversation.mts at f2b1b7f01, upstream 3ed57e9dd.
// Real fresh loopback daemon + fake Codex provider; no model, relay, or production daemon.
// Only its temporary directory is replaced by /fixture. Random IDs/timestamps are actual wire data.
const capture = JSON.parse(await readFile(new URL("./conversation.json", import.meta.url), "utf8"));
const timeline = JSON.parse(await readFile(new URL("./timeline.json", import.meta.url), "utf8"));
const payload = (id) =>
  capture.find(
    ({ direction, frame }) => direction === "server" && frame.message.payload?.requestId === id,
  ).frame.message.payload;

function accepted(schema, value) {
  const result = schema.safeParse(value);
  assert.equal(result.success, true, JSON.stringify(result));
}
function rejected(schema, value) {
  assert.equal(schema.safeParse(value).success, false);
}

test("real conversation and timeline captures pass current source schemas", () => {
  for (const { direction, frame } of [...capture, ...timeline]) {
    accepted(direction === "client" ? WSInboundMessageSchema : WSOutboundMessageSchema, frame);
  }
  assert.equal(payload("chat-create").error, null);
  assert.equal(payload("chat-create").agent.status, "idle");
  assert.equal(
    payload("chat-workspace-create").agent.workspaceId,
    payload("chat-create").agent.workspaceId,
  );
  assert.equal(payload("chat-observe").snapshot.idempotencyKey, "conversation-create");
  assert.equal(payload("chat-observe").snapshot.phase, "completed");
  assert.equal(payload("chat-observe-missing").snapshot, null);
  assert.equal(payload("chat-send").accepted, true);
  assert.equal(payload("chat-followup").accepted, true);
  assert.equal(payload("chat-rejected").accepted, false);
  assert.equal(typeof payload("chat-cancel-missing").error, "string");
  assert.equal(payload("chat-cancel").error, null);
  // The capture records the state; an acknowledgement must not manufacture a stopped state.
  assert.equal(typeof payload("chat-cancel").agent.status, "string");
});

test("source schemas retain the missing/null distinctions used by the Rust projection", () => {
  const cancel = capture.find(({ frame }) => frame.message.type === "cancel_agent_response").frame;
  const clone = structuredClone(cancel);
  delete clone.message.payload.error;
  accepted(WSOutboundMessageSchema, clone);
  clone.message.payload.error = null;
  accepted(WSOutboundMessageSchema, clone);
  const create = structuredClone(
    capture.find(({ frame }) => frame.message.type === "agent.create.response").frame,
  );
  delete create.message.payload.error;
  rejected(WSOutboundMessageSchema, create);
  const subscription = structuredClone(
    capture.find(({ frame }) => frame.message.type === "session.events.set_subscription.response")
      .frame,
  );
  delete subscription.message.payload.subscriptionId;
  accepted(WSOutboundMessageSchema, subscription);
  subscription.message.payload.subscriptionId = null;
  rejected(WSOutboundMessageSchema, subscription);
  const history = structuredClone(timeline.at(-1).frame);
  for (const field of ["error", "startCursor", "endCursor", "window", "entries"]) {
    const value = structuredClone(history);
    delete value.message.payload[field];
    rejected(WSOutboundMessageSchema, value);
  }
  // queued is agent-originated delivery; this is a schema case, not the user's captured send.
  const queued = structuredClone(
    capture.find(({ frame }) => frame.message.type === "send_agent_message_response").frame,
  );
  queued.message.payload.deliveryStatus = "queued";
  accepted(WSOutboundMessageSchema, queued);
});

test("tool/plan/question/mode/other permissions share typed action and resolution contracts", () => {
  const request = timeline.find(
    ({ frame }) => frame.message.payload.event?.type === "permission_requested",
  ).frame.message.payload.event.request;
  // Mutations are schema test cases, not fabricated daemon captures.
  for (const kind of ["tool", "plan", "question", "mode", "other"]) {
    accepted(AgentPermissionRequestPayloadSchema, {
      ...request,
      provider: "third-party-provider",
      kind,
      actions: [
        {
          id: "implement",
          label: "Accept",
          behavior: "allow",
          variant: "primary",
          intent: "implement_resume",
        },
      ],
    });
  }
  accepted(AgentPermissionResponseSchema, {
    behavior: "allow",
    selectedActionId: "implement",
    updatedInput: { answers: ["yes"] },
    updatedPermissions: [{ scope: "session" }],
  });
  accepted(AgentPermissionResponseSchema, {
    behavior: "deny",
    selectedActionId: "deny",
    message: "No",
    interrupt: true,
  });
  for (const value of [
    { behavior: "future" },
    { behavior: "allow", updatedInput: null },
    { behavior: "deny", interrupt: null },
  ]) {
    rejected(AgentPermissionResponseSchema, value);
  }
  rejected(AgentStreamEventPayloadSchema, { type: "turn_completed" });
  rejected(AgentTimelineItemPayloadSchema, { type: "assistant_message", text: null });
  rejected(AgentTimelineItemPayloadSchema, {
    type: "tool_call",
    callId: "c",
    name: "shell",
    status: "failed",
    detail: { type: "shell", command: "echo" },
    error: null,
  });
});

test(
  "actual Rust serializer output conforms to current inbound source schema",
  {
    skip: !process.env.PASEO_CONVERSATION_REQUESTS_OUT,
  },
  async () => {
    const requests = JSON.parse(
      await readFile(process.env.PASEO_CONVERSATION_REQUESTS_OUT, "utf8"),
    );
    assert(
      requests.length >= 15,
      "Run the Rust builders test with PASEO_CONVERSATION_REQUESTS_OUT first",
    );
    for (const frame of requests) accepted(WSInboundMessageSchema, frame);
    assert(
      requests.some(
        ({ message }) =>
          message.type === "agent.create.request" &&
          message.config.provider === "third-party/provider" &&
          message.config.title === null,
      ),
    );
    assert(
      requests.some(
        ({ message }) =>
          message.type === "fetch_agent_timeline_request" &&
          message.projection === "projected" &&
          message.limit === 0 &&
          message.cursor.seq === 0,
      ),
    );
  },
);
