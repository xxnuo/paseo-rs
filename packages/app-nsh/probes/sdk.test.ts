import { defaultWebSocketFactory } from "@getpaseo/client/internal/daemon-client-websocket-transport";
import { expect, test } from "nsh/test";
import { probeSdk } from "./sdk";

test("the existing SDK, Zod validators and relay crypto run without Node adapters", async () => {
  expect(await probeSdk()).toEqual({
    initialState: { status: "idle" },
    agentId: "nsh-probe-agent",
    workspaceId: "nsh-probe-workspace",
    plaintext: "Paseo SDK / 中文 / nsh",
    pong: { type: "pong" },
    invalidSchemaAccepted: false,
    validatorAccepted: true,
    invalidValidatorAccepted: false,
  });
});

test("the SDK WebSocket factory uses the runtime's constructor and URL validation", () => {
  expect(() => defaultWebSocketFactory("not-a-websocket-url")).toThrow(/Invalid.*URL/);
});
