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

// Catalog captured at e4306d4d57c322c64a685e1b7915e38c5d920c0c with the same upstream source.
// SDK compact_provider_snapshots/provider_snapshot_references=false; no conditional snapshot request.
// Only deterministic fake Claude/Codex/OpenCode providers are enabled; other built-ins are disabled.
const catalog = JSON.parse(await readFile(new URL("./catalog.json", import.meta.url), "utf8"));

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

test("real catalog frames pass upstream validators and preserve project/workspace/agent associations", () => {
  for (const { direction, frame } of catalog) {
    const parsed =
      direction === "client"
        ? WSInboundMessageSchema.safeParse(frame)
        : validateWSOutboundMessage(frame);
    assert.equal(parsed.success, true, JSON.stringify(parsed));
  }
  const payload = (id) =>
    catalog.find(
      ({ direction, frame }) => direction === "server" && frame.message.payload.requestId === id,
    ).frame.message.payload;
  const request = (id) =>
    catalog.find(({ direction, frame }) => direction === "client" && frame.message.requestId === id)
      .frame.message;
  const project = payload("catalog-projects").projects[0];
  const providers = payload("catalog-providers");
  assert.equal(providers.compactSnapshot, undefined);
  assert.equal(
    providers.entries.find(({ provider }) => provider === "codex").models[0].id,
    "gpt-5.4-mini",
  );
  assert.equal(providers.entries.find(({ provider }) => provider === "pi").enabled, false);
  assert.deepEqual(request("catalog-features").draftConfig, {
    provider: "codex",
    cwd: "/fixture/project",
  });
  assert.deepEqual(payload("catalog-features").features, []);
  assert.equal(payload("catalog-features").error, null);
  assert.equal(request("catalog-workspaces").filter.projectId, project.projectId);
  // AgentDirectory's projectKeys filter matches stable projectId, not the directory descriptor's projectKey.
  assert.deepEqual(request("catalog-agents").filter.projectKeys, [project.projectId]);
  assert.notEqual(project.projectKey, project.projectId);
  assert.deepEqual(request("catalog-directory-key").filter.projectKeys, [project.projectKey]);
  assert.equal(payload("catalog-directory-key").entries.length, 0);
  const workspaces = payload("catalog-workspaces").entries;
  const agents = payload("catalog-agents").entries;
  assert.equal(workspaces.length, 2);
  assert.equal(agents.length, 2);
  for (const { agent, project: placement } of agents) {
    assert.equal(placement.projectKey, project.projectId);
    assert(workspaces.some(({ id }) => id === agent.workspaceId));
    assert.equal(agent.cwd, "/fixture/project");
  }
  assert.deepEqual(
    payload("catalog-agent").agent,
    agents.find(({ agent }) => agent.id === "fixture-agent-a").agent,
  );
  assert.equal(payload("catalog-agent").agent.features[0].id, "test_feature");
  assert.equal(payload("catalog-empty-agents").entries.length, 0);
  assert.deepEqual(payload("catalog-missing-agent"), {
    requestId: "catalog-missing-agent",
    agent: null,
    project: null,
    error: null,
  });
});
