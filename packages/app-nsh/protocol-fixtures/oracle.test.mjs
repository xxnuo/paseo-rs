import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { test } from "node:test";
import { parseServerInfoStatusPayload, WSInboundMessageSchema } from "@getpaseo/protocol/messages";
import { validateWSOutboundMessage } from "@getpaseo/protocol/validation/ws-outbound";

// Captured from the unmodified SDK/daemon at Paseo commit 3ed57e9dd72df4927b08f16859eaf8d4d2d70ac2.
// Source: client/src/daemon-client.ts sendHelloMessage; server/src/server/websocket-server.ts
// sendHelloResponse/buildServerInfoStatusPayload; protocol/src/messages.ts project.list schemas.
// Test daemon + fake provider; no relay, auth, model or HTTP-upgrade bytes. Identity/paths are replaced.
// The SDK advertises its complete capabilities; a new client must advertise only what it implements.
const capture = JSON.parse(
  await readFile(new URL("./direct-session.json", import.meta.url), "utf8"),
);

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
