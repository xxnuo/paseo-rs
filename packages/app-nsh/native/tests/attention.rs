//! Attention interactions through the real application, typed RPCs and a manual network clock.

use nsh_core::{
    element::{Owner, Spawned, spawn, timer},
    geometry::{Points, Size},
    platform::Os,
    time::Clock,
    websocket::{
        WebSocketData, WebSocketEvent, WebSocketEvents, WebSocketRequest, WebSocketTask,
        WebSocketTransport,
    },
};
use nsh_engine::{EngineConfig, Root};
use nsh_testing::Headless;
use serde_json::{Value, json};
use std::{
    cell::RefCell,
    rc::Rc,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime},
};

const AGENT: &str = "fixture-agent-a";
const WORKSPACE: &str = "fixture-workspace-1";
const TITLE: &str = "Attention Agent";
const EVENTS: &str = "attention-events";
const TIMELINE: &str = "attention-timeline";
const DIRECT: &str = include_str!("../../protocol-fixtures/direct-session.json");
const CATALOG: &str = include_str!("../../protocol-fixtures/catalog.json");
const HISTORY: &str = include_str!("../../protocol-fixtures/timeline.json");

#[derive(Default)]
struct Trace {
    requests: Vec<Value>,
    delivered: Vec<Value>,
}

struct Transport {
    trace: Arc<Mutex<Trace>>,
    workspace_status: &'static str,
    permission_during_clear: bool,
}

struct Socket {
    owner: Owner,
    events: Rc<RefCell<WebSocketEvents>>,
    jobs: Vec<Spawned<()>>,
    trace: Arc<Mutex<Trace>>,
    workspace_status: &'static str,
    permission_during_clear: bool,
    agent_clears: usize,
    workspace_clears: usize,
    unread: bool,
}

fn captured(file: &str, kind: &str) -> Value {
    let rows: Vec<Value> = serde_json::from_str(file).unwrap();
    rows.into_iter()
        .find(|row| row["direction"] == "server" && row["frame"]["message"]["type"] == kind)
        .unwrap()["frame"]
        .clone()
}

impl Socket {
    fn snapshot(&self) -> Value {
        let mut agent =
            captured(CATALOG, "fetch_agent_response")["message"]["payload"]["agent"].clone();
        agent["id"] = json!(AGENT);
        agent["title"] = json!(TITLE);
        agent["status"] = json!("idle");
        agent["activeTurn"] = Value::Null;
        agent["pendingPermissions"] = json!([]);
        agent["requiresAttention"] = json!(self.unread);
        agent["attentionReason"] = if self.unread {
            json!("finished")
        } else {
            Value::Null
        };
        agent["attentionTimestamp"] = if self.unread {
            json!("2026-11-06T00:00:00.000Z")
        } else {
            Value::Null
        };
        agent
    }

    fn deliver(&mut self, frame: Value, delay: Duration) {
        let events = self.events.clone();
        let trace = self.trace.clone();
        self.jobs.push(self.owner.run(|| {
            spawn(async move {
                if !delay.is_zero() {
                    timer(delay).await;
                }
                trace.lock().unwrap().delivered.push(frame.clone());
                events.borrow_mut()(WebSocketEvent::Message(WebSocketData::Text(
                    frame.to_string(),
                )));
            })
        }));
    }

    fn attention(&mut self, reason: &str, delay: Duration) {
        self.deliver(
            json!({"type":"session","message":{"type":"agent_attention_required","payload":{
                "agentId":AGENT,"subscriptionId":EVENTS,"reason":reason,
                "timestamp":"2026-11-06T00:00:01.000Z","shouldNotify":true
            }}}),
            delay,
        );
    }
}

impl WebSocketTask for Socket {
    fn send(&mut self, data: WebSocketData) -> Result<(), String> {
        let WebSocketData::Text(text) = data else {
            panic!("unexpected binary request");
        };
        let request: Value = serde_json::from_str(&text).unwrap();
        self.trace.lock().unwrap().requests.push(request.clone());
        if request["type"] == "hello" {
            let mut response = captured(DIRECT, "status");
            for feature in [
                "explicitEventSubscriptions",
                "selectiveAgentTimeline",
                "wholeTurnTimeline",
            ] {
                response["message"]["payload"]["features"][feature] = json!(true);
            }
            self.deliver(response, Duration::ZERO);
            return Ok(());
        }
        let mut delay = Duration::ZERO;
        let mut response = match request["message"]["type"].as_str().unwrap() {
            "project.list.request" => {
                let rows: Vec<Value> = serde_json::from_str(DIRECT).unwrap();
                let mut response = rows[5]["frame"].clone();
                response["message"]["payload"]["projects"][0]["projectDisplayName"] =
                    json!("ATTENTION_PROJECT");
                response
            }
            "fetch_workspaces_request" => {
                let mut response = captured(CATALOG, "fetch_workspaces_response");
                let entries = response["message"]["payload"]["entries"]
                    .as_array_mut()
                    .unwrap();
                entries.truncate(1);
                entries[0]["status"] = json!(if self.workspace_clears >= 2 {
                    "done"
                } else {
                    self.workspace_status
                });
                response
            }
            "fetch_agents_request" => {
                let mut response = captured(CATALOG, "fetch_agents_response");
                let entries = response["message"]["payload"]["entries"]
                    .as_array_mut()
                    .unwrap();
                entries.retain(|entry| entry["agent"]["id"] == AGENT);
                entries[0]["agent"] = self.snapshot();
                response
            }
            "fetch_agent_request" => {
                let mut response = captured(CATALOG, "fetch_agent_response");
                response["message"]["payload"]["agent"] = self.snapshot();
                response
            }
            "session.events.set_subscription.request" => json!({"type":"session","message":{
                "type":"session.events.set_subscription.response","payload":{"subscriptionId":EVENTS}}}),
            "agent.timeline.set_subscription.request" => json!({"type":"session","message":{
                "type":"agent.timeline.set_subscription.response","payload":{"subscriptionId":TIMELINE,"agentIds":[AGENT]}}}),
            "fetch_agent_timeline_request" => {
                let mut response = captured(HISTORY, "fetch_agent_timeline_response");
                let payload = &mut response["message"]["payload"];
                payload["agentId"] = json!(AGENT);
                payload["agent"] = self.snapshot();
                payload["direction"] = request["message"]["direction"].clone();
                payload["entries"] = json!([]);
                payload["window"] = json!({"minSeq":0,"maxSeq":0,"nextSeq":1});
                payload["startCursor"] = Value::Null;
                payload["endCursor"] = Value::Null;
                payload["hasOlder"] = json!(false);
                payload["hasNewer"] = json!(false);
                response
            }
            "clear_agent_attention" => {
                assert_eq!(request["message"]["agentId"], AGENT);
                self.agent_clears += 1;
                self.unread = false;
                if self.agent_clears == 1 {
                    delay = Duration::from_secs(2);
                    self.attention("finished", Duration::from_secs(1));
                    if self.permission_during_clear {
                        self.attention("permission", Duration::from_millis(1500));
                    }
                } else if self.agent_clears == 2 {
                    self.attention("permission", Duration::from_secs(1));
                    self.attention("finished", Duration::from_secs(3));
                } else {
                    delay = Duration::from_secs(2);
                }
                json!({"type":"session","message":{"type":"clear_agent_attention_response","payload":{
                    "agentId":AGENT,"agents":[self.snapshot()]
                }}})
            }
            "workspace.clear_attention.request" => {
                assert_eq!(request["message"]["workspaceId"], WORKSPACE);
                self.workspace_clears += 1;
                let success = self.workspace_clears > 1;
                let error = if success {
                    Value::Null
                } else {
                    json!("Original workspace backend failure")
                };
                let ids = if success { json!([AGENT]) } else { json!([]) };
                json!({"type":"session","message":{"type":"workspace.clear_attention.response","payload":{
                    "workspaceId":WORKSPACE,"clearedAgentIds":ids,"results":[{
                        "workspaceId":WORKSPACE,"clearedAgentIds":ids,"success":success,"error":error
                    }],"success":success,"error":error
                }}})
            }
            "subscription.release.request" => json!({"type":"session","message":{
                "type":"subscription.release.response","payload":{"subscriptionId":request["message"]["subscriptionId"]}}}),
            kind => panic!("unexpected attention test request: {kind}"),
        };
        response["message"]["payload"]["requestId"] = request["message"]["requestId"].clone();
        self.deliver(response, delay);
        Ok(())
    }

    fn close(&mut self, _: u16, _: &str) -> Result<(), String> {
        Ok(())
    }
    fn buffered_amount(&self) -> usize {
        0
    }
    fn pause(&mut self, _: bool) {}
}

impl WebSocketTransport for Transport {
    fn connect(
        &self,
        _: WebSocketRequest,
        events: WebSocketEvents,
    ) -> Result<Box<dyn WebSocketTask>, String> {
        let mut socket = Socket {
            owner: Owner::current(),
            events: Rc::new(RefCell::new(events)),
            jobs: Vec::new(),
            trace: self.trace.clone(),
            workspace_status: self.workspace_status,
            permission_during_clear: self.permission_during_clear,
            agent_clears: 0,
            workspace_clears: 0,
            unread: true,
        };
        let events = socket.events.clone();
        socket.jobs.push(spawn(async move {
            events.borrow_mut()(WebSocketEvent::Open {
                protocol: String::new(),
            });
        }));
        Ok(Box::new(socket))
    }
}

fn open_project(
    workspace_status: &'static str,
    permission_during_clear: bool,
) -> (Headless, Arc<Mutex<Trace>>) {
    let trace = Arc::new(Mutex::new(Trace::default()));
    let mut config = EngineConfig::new(Root::of::<paseo_nsh::App>());
    config.clock = Clock::manual(SystemTime::UNIX_EPOCH);
    config.viewport = Size::new(Points(390.0), Points(844.0));
    config.environment.window = config.viewport;
    config.environment.os = Os::Linux;
    config.websocket = Some(Arc::new(Transport {
        trace: trace.clone(),
        workspace_status,
        permission_during_clear,
    }));
    let mut app = Headless::start(config).unwrap();
    app.wait_for("连接开发主机").unwrap();
    app.type_text("label:主机地址", "ws://localhost:6767")
        .unwrap();
    app.press("role:button[连接主机]").unwrap();
    app.wait_for("ATTENTION_PROJECT").unwrap();
    app.press("role:button[ATTENTION_PROJECT]").unwrap();
    app.wait_for("已加载 1 个Agent").unwrap();
    (app, trace)
}

fn open_agent(app: &mut Headless) {
    app.press(&format!("role:button[Agent：{TITLE} · 空闲]"))
        .unwrap();
    app.wait_for("text:/^Agent ID：fixture-agent-a/").unwrap();
    app.press("role:button[打开会话]").unwrap();
    app.wait_for("会话已同步").unwrap();
    app.run_for(Duration::from_millis(20)).unwrap();
}

fn count(frames: &[Value], kind: &str) -> usize {
    frames
        .iter()
        .filter(|frame| frame["message"]["type"] == kind)
        .count()
}

#[test]
fn attention_entry_interaction_permission_and_departure_use_distinct_confirmed_rpcs() {
    let (mut app, trace) = open_project("done", false);
    open_agent(&mut app);
    assert_eq!(
        count(&trace.lock().unwrap().requests, "clear_agent_attention"),
        1
    );
    app.run_for(Duration::from_secs(1)).unwrap();
    {
        let trace = trace.lock().unwrap();
        assert_eq!(count(&trace.requests, "clear_agent_attention"), 1);
        assert_eq!(count(&trace.delivered, "clear_agent_attention_response"), 0);
        assert_eq!(count(&trace.delivered, "agent_attention_required"), 1);
    }
    // A new finished event stays unread while viewed; explicit input focus queues behind the old ACK.
    app.type_text("label:消息", "retained attention draft")
        .unwrap();
    assert_eq!(
        count(&trace.lock().unwrap().requests, "clear_agent_attention"),
        1
    );
    app.run_for(Duration::from_secs(1)).unwrap();
    assert_eq!(
        count(&trace.lock().unwrap().requests, "clear_agent_attention"),
        2
    );
    app.run_for(Duration::from_secs(1)).unwrap();
    app.press_key("tab").unwrap();
    app.type_text("label:消息", "").unwrap();
    assert_eq!(
        count(&trace.lock().unwrap().requests, "clear_agent_attention"),
        2
    );
    app.run_for(Duration::from_secs(2)).unwrap();
    assert_eq!(
        count(&trace.lock().unwrap().requests, "clear_agent_attention"),
        2
    );
    app.press("role:button[返回 Agent 详情]").unwrap();
    app.wait_for("text:/^Agent ID：fixture-agent-a/").unwrap();
    app.run_for(Duration::from_millis(20)).unwrap();
    {
        let trace = trace.lock().unwrap();
        assert_eq!(count(&trace.requests, "clear_agent_attention"), 3);
        assert_eq!(count(&trace.delivered, "clear_agent_attention_response"), 2);
        assert_eq!(count(&trace.requests, "agent_permission_response"), 0);
        assert_eq!(
            count(&trace.requests, "workspace.clear_attention.request"),
            0
        );
        let ids: Vec<_> = trace
            .requests
            .iter()
            .filter(|frame| frame["message"]["type"] == "clear_agent_attention")
            .map(|frame| frame["message"]["requestId"].as_str().unwrap())
            .collect();
        assert_eq!(
            ids.len(),
            ids.iter().collect::<std::collections::HashSet<_>>().len()
        );
    }
    app.run_for(Duration::from_secs(2)).unwrap();
    assert!(
        app.screen()
            .matches("text:/^Agent ID：fixture-agent-a/")
            .unwrap()
    );
    let trace = trace.lock().unwrap();
    assert_eq!(count(&trace.delivered, "clear_agent_attention_response"), 3);
    let last_request = trace
        .requests
        .iter()
        .rev()
        .find(|frame| frame["message"]["type"] == "clear_agent_attention")
        .unwrap();
    let last_reply = trace
        .delivered
        .iter()
        .rev()
        .find(|frame| frame["message"]["type"] == "clear_agent_attention_response")
        .unwrap();
    assert_eq!(
        last_request["message"]["requestId"],
        last_reply["message"]["payload"]["requestId"]
    );
    assert_eq!(
        last_reply["message"]["payload"]["agents"][0]["requiresAttention"],
        false
    );
    let mut releases: Vec<_> = trace
        .requests
        .iter()
        .filter(|frame| frame["message"]["type"] == "subscription.release.request")
        .map(|frame| frame["message"]["subscriptionId"].as_str().unwrap())
        .collect();
    releases.sort_unstable();
    assert_eq!(releases, [EVENTS, TIMELINE]);
    assert!(
        !trace
            .delivered
            .iter()
            .any(|frame| frame["message"]["payload"]["event"]["type"] == "turn_started")
    );
}

#[test]
fn a_permission_arriving_before_the_old_ack_suppresses_queued_focus_and_departure_clears() {
    let (mut app, trace) = open_project("done", true);
    open_agent(&mut app);
    app.run_for(Duration::from_secs(1)).unwrap();
    app.type_text("label:消息", "draft while old clear waits")
        .unwrap();
    app.run_for(Duration::from_secs(2)).unwrap();
    assert_eq!(
        count(&trace.lock().unwrap().requests, "clear_agent_attention"),
        1
    );
    app.press("role:button[返回 Agent 详情]").unwrap();
    app.run_for(Duration::from_millis(20)).unwrap();
    let trace = trace.lock().unwrap();
    assert_eq!(count(&trace.requests, "clear_agent_attention"), 1);
    assert_eq!(count(&trace.requests, "agent_permission_response"), 0);
}

#[test]
fn workspace_read_is_explicit_retries_backend_failure_and_refreshes_both_directories() {
    for status in ["attention", "failed", "done"] {
        let (mut app, trace) = open_project(status, false);
        app.wait_for("已加载 1 个工作区").unwrap();
        assert_eq!(
            count(
                &trace.lock().unwrap().requests,
                "workspace.clear_attention.request"
            ),
            0
        );
        if status == "done" {
            assert!(!app.screen().shows("工作区标为已读"));
            continue;
        }
        app.press("role:button[工作区标为已读]").unwrap();
        app.wait_for("标为已读失败：Original workspace backend failure")
            .unwrap();
        assert!(app.screen().enabled("role:button[工作区标为已读]").unwrap());
        let before = {
            let trace = trace.lock().unwrap();
            assert_eq!(
                count(&trace.requests, "workspace.clear_attention.request"),
                1
            );
            (
                count(&trace.requests, "fetch_workspaces_request"),
                count(&trace.requests, "fetch_agents_request"),
            )
        };
        app.press("role:button[工作区标为已读]").unwrap();
        app.run_for(Duration::from_millis(50)).unwrap();
        assert!(!app.screen().shows("工作区标为已读"));
        let trace = trace.lock().unwrap();
        assert_eq!(
            count(&trace.requests, "workspace.clear_attention.request"),
            2
        );
        assert_eq!(
            count(&trace.requests, "fetch_workspaces_request"),
            before.0 + 1
        );
        assert_eq!(count(&trace.requests, "fetch_agents_request"), before.1 + 1);
        assert_eq!(count(&trace.requests, "clear_agent_attention"), 0);
        assert_eq!(count(&trace.requests, "agent_permission_response"), 0);
        let success = trace
            .delivered
            .iter()
            .rev()
            .find(|frame| frame["message"]["type"] == "workspace.clear_attention.response")
            .unwrap();
        assert_eq!(success["message"]["payload"]["success"], true);
        assert_eq!(
            success["message"]["payload"]["results"][0]["workspaceId"],
            WORKSPACE
        );
    }
}
