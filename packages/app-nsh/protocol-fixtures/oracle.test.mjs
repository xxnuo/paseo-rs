import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { test } from "node:test";
import { parseServerInfoStatusPayload, WSInboundMessageSchema } from "@getpaseo/protocol/messages";
import { validateWSOutboundMessage } from "@getpaseo/protocol/validation/ws-outbound";

// Captured at checkout b5eb278642865930553ab0d9b5bdd1e3cc95fd6b, upstream base 3710091a3.
// SDK/daemon/protocol source files last changed at upstream commit 3ed57e9dd72df4927b08f16859eaf8d4d2d70ac2.
// Source: client/src/daemon-client.ts sendHelloMessage; server/src/server/websocket-server.ts
// sendHelloResponse/buildServerInfoStatusPayload; protocol/src/messages.ts project.list schemas.
// Test daemon + fake provider; no relay, auth, model or HTTP-upgrade bytes. Identity/paths are replaced.
// The SDK advertises its complete capabilities; a new client must advertise only what it implements.
const capture = JSON.parse(
  await readFile(new URL("./direct-session.json", import.meta.url), "utf8"),
);

// Timeline captured at checkout a4616e73d916bfcd5cc13efc7aa9554ff641eccb with the same upstream source.
const timeline = JSON.parse(await readFile(new URL("./timeline.json", import.meta.url), "utf8"));

test("captured SDK handshake and project messages pass the upstream wire validators", () => {
  assert.deepEqual(
    capture.map(({ direction, frame }) => [direction, frame.message?.type ?? frame.type]),
    [
      ["client", "hello"],
      ["server", "status"],
      ["client", "project.list.request"],
      ["server", "project.list.response"],
      ["client", "project.list.request"],
      ["server", "project.list.response"],
    ],
  );
  for (const { direction, frame } of capture) {
    const parsed =
      direction === "client"
        ? WSInboundMessageSchema.safeParse(frame)
        : validateWSOutboundMessage(frame);
    assert.equal(parsed.success, true, JSON.stringify(parsed));
  }
  const info = parseServerInfoStatusPayload(capture[1].frame.message.payload);
  assert.equal(info.serverId, "fixture-server");
  assert.equal(info.protocolVersion, 1);
  assert.equal(capture[3].frame.message.payload.projects.length, 0);
  assert.equal(capture[5].frame.message.payload.projects[0].projectRootPath, "/fixture/project");
  assert.equal(capture[0].frame.auth, undefined);
});

test("real permission, assistant stream and fetched history share the canonical timeline shape", () => {
  for (const { direction, frame } of timeline) {
    const parsed =
      direction === "client"
        ? WSInboundMessageSchema.safeParse(frame)
        : validateWSOutboundMessage(frame);
    assert.equal(parsed.success, true, JSON.stringify(parsed));
  }
  const events = timeline
    .filter(({ frame }) => frame.message.type === "agent_stream")
    .map(({ frame }) => frame.message.payload.event);
  const permission = events.find(({ type }) => type === "permission_requested");
  assert.equal(permission.request.id, "fixture-permission");
  const resolved = events.find(({ type }) => type === "permission_resolved");
  assert.equal(resolved.requestId, permission.request.id);
  assert.equal(resolved.resolution.behavior, "allow");
  const assistant = events.find(
    (event) => event.type === "timeline" && event.item.type === "assistant_message",
  );
  assert.equal(assistant.item.text, "FIXTURE_REPLY_中文");
  assert.equal(
    events.some(({ type }) => type === "text_delta"),
    false,
  );
  const history = timeline.find(
    ({ frame }) => frame.message.type === "fetch_agent_timeline_response",
  ).frame.message.payload;
  assert.equal(history.requestId, "fixture-history");
  assert.equal(history.error, null);
  assert.equal(history.projection, "projected");
  assert.deepEqual(
    history.entries.find(({ item }) => item.type === "assistant_message").item,
    assistant.item,
  );
});
