import { createPaseoApi, type PaseoAgentTimelineEvent } from "@getpaseo/client";
import { DaemonClient } from "@getpaseo/client/internal/daemon-client";
import type { WebSocketLike } from "@getpaseo/client/internal/daemon-client-transport-types";
import { defaultWebSocketFactory } from "@getpaseo/client/internal/daemon-client-websocket-transport";

export interface LiveSdkOracleConfig {
  url: string;
  cwd: string;
}

function check(condition: unknown, message: string): asserts condition {
  if (!condition) throw new Error(message);
}

async function waitFor(condition: () => boolean, message: string): Promise<void> {
  const deadline = Date.now() + 10_000;
  while (!condition()) {
    check(Date.now() < deadline, message);
    await new Promise((resolve) => setTimeout(resolve, 10));
  }
}

// A protocol oracle against a real daemon with its deterministic fake Codex provider, not model QA.
export async function runLiveSdkOracle(config: LiveSdkOracleConfig) {
  const transport: { socket: WebSocketLike | null } = { socket: null };
  let connections = 0;
  const driver = new DaemonClient({
    url: config.url,
    clientId: "nsh-live-sdk-oracle",
    appVersion: "0.15.2",
    connectTimeoutMs: 5_000,
    reconnect: { enabled: true, baseDelayMs: 20, maxDelayMs: 100 },
    webSocketFactory: (url, options) => {
      transport.socket = defaultWebSocketFactory(url, options);
      return transport.socket;
    },
  });
  const api = createPaseoApi(driver);
  const stopStatus = driver.subscribeConnectionStatus((state) => {
    if (state.status === "connected") connections += 1;
  });
  const events: PaseoAgentTimelineEvent[] = [];
  const assistantText = () => {
    let text = "";
    for (const { event } of events) {
      if (event.type === "timeline" && event.item.type === "assistant_message") {
        text += event.item.text;
      }
    }
    return text;
  };
  try {
    await driver.connect();
    await api.agents.list();
    const initialProjects = await api.projects.list();
    check(initialProjects.projects.length === 0, "The oracle daemon must start empty");

    const agent = await api.agents.create({
      config: { provider: "codex/gpt-5.4-mini", modeId: "full-access" },
      cwd: config.cwd,
      title: "NSH SDK protocol oracle (fake provider)",
    });
    check(agent.status === "idle", "Created agent is not idle");
    const projects = await api.projects.list();
    const project = projects.projects.find((entry) => entry.projectRootPath === config.cwd);
    check(project, "Created agent's project is absent from project.list");

    const subscription = agent.timeline.subscribe((event) => events.push(event));
    await subscription.ready;
    const first = await agent.run("Respond with exactly: NSH_FIRST_中文", { timeoutMs: 10_000 });
    check(first.status === "idle", "First turn did not finish idle");
    await waitFor(() => assistantText().includes("NSH_FIRST_中文"), "Missing streamed first reply");

    await agent.send("Respond with exactly: NSH_CANCEL_READY");
    await waitFor(() => assistantText().includes("NSH_CANCEL_READY"), "Cancel turn did not start");
    await driver.cancelAgent(agent.id);
    const cancelled = await agent.refresh();
    check(cancelled?.agent.status === "idle", "Cancel did not restore idle state");

    check(transport.socket, "SDK did not construct a real WebSocket");
    transport.socket.close(4000, "oracle reconnect");
    await waitFor(
      () => connections >= 2 && events.some(({ event }) => event.type === "subscription_restored"),
      "SDK did not reconnect and restore its timeline subscription",
    );
    const second = await agent.run("Respond with exactly: NSH_RECONNECTED", { timeoutMs: 10_000 });
    check(second.status === "idle", "Turn after reconnect did not finish idle");
    await waitFor(
      () => assistantText().includes("NSH_RECONNECTED"),
      "Missing streamed reply after reconnect",
    );
    const history = await agent.timeline.refetch({ direction: "tail", limit: 100 });
    check(history.error === null, "Timeline history RPC returned an error");
    const historyText = history.entries
      .filter(({ item }) => item.type === "assistant_message")
      .map(({ item }) => (item.type === "assistant_message" ? item.text : ""))
      .join("");
    check(historyText.includes("NSH_FIRST_中文"), "History lost the turn before reconnect");
    check(historyText.includes("NSH_RECONNECTED"), "History lost the turn after reconnect");
    await subscription.release();
    return {
      provider: "fake-codex",
      projectId: project.projectId,
      agentId: agent.id,
      connections,
      historyEntries: history.entries.length,
      checks: ["project.list", "agent.create", "send", "stream", "cancel", "reconnect", "history"],
    };
  } finally {
    stopStatus();
    try {
      await api.dispose();
    } finally {
      await driver.close();
    }
  }
}
