//! Root connection recovery over deterministic sockets; no daemon or inference.

use nsh_core::{
    element::{Owner, Spawned, spawn, timer},
    geometry::{Points, Size},
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
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, SystemTime},
};

const AGENT: &str = "fixture-agent-a";
const TITLE: &str = "Reconnect Agent";
const DIRECT: &str = include_str!("../../protocol-fixtures/direct-session.json");
const CATALOG: &str = include_str!("../../protocol-fixtures/catalog.json");
const HISTORY: &str = include_str!("../../protocol-fixtures/timeline.json");

#[derive(Clone, Copy)]
enum Scenario {
    SameServer,
    OtherServer,
    Rejected(&'static str),
    DirectoryFailure,
    Offline,
    SlowHandshake,
}

#[derive(Default)]
struct Trace {
    requests: Vec<(usize, Value)>,
    delivered: Vec<(usize, Value)>,
}

struct Transport {
    scenario: Scenario,
    attempts: Arc<AtomicUsize>,
    trace: Arc<Mutex<Trace>>,
}
struct Socket {
    owner: Owner,
    events: Rc<RefCell<WebSocketEvents>>,
    jobs: Vec<Spawned<()>>,
    number: usize,
    scenario: Scenario,
    trace: Arc<Mutex<Trace>>,
    histories: usize,
    projects: usize,
}

fn captured(file: &str, kind: &str) -> Value {
    let rows: Vec<Value> = serde_json::from_str(file).unwrap();
    rows.into_iter()
        .find(|row| row["direction"] == "server" && row["frame"]["message"]["type"] == kind)
        .unwrap()["frame"]
        .clone()
}

fn snapshot() -> Value {
    let mut agent =
        captured(CATALOG, "fetch_agent_response")["message"]["payload"]["agent"].clone();
    agent["id"] = json!(AGENT);
    agent["title"] = json!(TITLE);
    agent["status"] = json!("idle");
    agent["activeTurn"] = Value::Null;
    agent["pendingPermissions"] = json!([]);
    agent["requiresAttention"] = json!(false);
    agent["attentionReason"] = Value::Null;
    agent["attentionTimestamp"] = Value::Null;
    agent
}

impl Socket {
    fn deliver(&mut self, frame: Value, delay: Duration) {
        let trace = self.trace.clone();
        let number = self.number;
        let events = self.events.clone();
        self.jobs.push(self.owner.run(|| {
            spawn(async move {
                if !delay.is_zero() {
                    timer(delay).await;
                }
                trace
                    .lock()
                    .unwrap()
                    .delivered
                    .push((number, frame.clone()));
                events.borrow_mut()(WebSocketEvent::Message(WebSocketData::Text(
                    frame.to_string(),
                )));
            })
        }));
    }

    fn close_later(&mut self) {
        let events = self.events.clone();
        self.jobs.push(self.owner.run(|| {
            spawn(async move {
                timer(Duration::from_secs(1)).await;
                events.borrow_mut()(WebSocketEvent::Close {
                    code: 1006,
                    reason: String::new(),
                    was_clean: false,
                });
            })
        }));
    }
}

impl WebSocketTask for Socket {
    fn send(&mut self, data: WebSocketData) -> Result<(), String> {
        let WebSocketData::Text(text) = data else {
            panic!("unexpected binary request");
        };
        let request: Value = serde_json::from_str(&text).unwrap();
        self.trace
            .lock()
            .unwrap()
            .requests
            .push((self.number, request.clone()));
        if request["type"] == "hello" {
            if self.number > 1
                && let Scenario::Rejected(reason) = self.scenario
            {
                self.deliver(
                    json!({"type":"hello.rejected","reason":reason,"accepts":[]}),
                    Duration::ZERO,
                );
                return Ok(());
            }
            let mut response = captured(DIRECT, "status");
            for feature in [
                "explicitEventSubscriptions",
                "selectiveAgentTimeline",
                "wholeTurnTimeline",
            ] {
                response["message"]["payload"]["features"][feature] = json!(true);
            }
            if self.number > 1 && matches!(self.scenario, Scenario::OtherServer) {
                response["message"]["payload"]["serverId"] = json!("different-server");
            }
            let delay = if self.number > 1 && matches!(self.scenario, Scenario::SlowHandshake) {
                Duration::from_secs(5)
            } else {
                Duration::ZERO
            };
            self.deliver(response, delay);
            return Ok(());
        }
        let mut response = match request["message"]["type"].as_str().unwrap() {
            "project.list.request" => {
                self.projects += 1;
                if self.number > 1
                    && self.projects == 1
                    && matches!(self.scenario, Scenario::DirectoryFailure)
                {
                    json!({"type":"session","message":{"type":"rpc_error","payload":{
                        "requestType":"project.list.request","error":"Directory temporarily unavailable"
                    }}})
                } else {
                    let rows: Vec<Value> = serde_json::from_str(DIRECT).unwrap();
                    let mut response = rows[5]["frame"].clone();
                    response["message"]["payload"]["projects"][0]["projectDisplayName"] =
                        json!("RECONNECT_PROJECT");
                    response
                }
            }
            "fetch_workspaces_request" => captured(CATALOG, "fetch_workspaces_response"),
            "fetch_agents_request" => {
                let mut response = captured(CATALOG, "fetch_agents_response");
                let entries = response["message"]["payload"]["entries"]
                    .as_array_mut()
                    .unwrap();
                entries.retain(|entry| entry["agent"]["id"] == AGENT);
                entries[0]["agent"] = snapshot();
                response
            }
            "fetch_agent_request" => {
                assert_eq!(request["message"]["agentId"], AGENT);
                let mut response = captured(CATALOG, "fetch_agent_response");
                response["message"]["payload"]["agent"] = snapshot();
                response
            }
            "session.events.set_subscription.request" => json!({"type":"session","message":{
                "type":"session.events.set_subscription.response","payload":{"subscriptionId":format!("events-{}",self.number)}}}),
            "agent.timeline.set_subscription.request" => {
                assert_eq!(request["message"]["agentIds"], json!([AGENT]));
                json!({"type":"session","message":{"type":"agent.timeline.set_subscription.response","payload":{
                    "subscriptionId":format!("timeline-{}", self.number),"agentIds":[AGENT]
                }}})
            }
            "fetch_agent_timeline_request" => {
                assert_eq!(request["message"]["agentId"], AGENT);
                self.histories += 1;
                if self.number == 1 && self.histories == 1 {
                    self.close_later();
                }
                let mut response = captured(HISTORY, "fetch_agent_timeline_response");
                let payload = &mut response["message"]["payload"];
                payload["agentId"] = json!(AGENT);
                payload["agent"] = snapshot();
                payload["direction"] = request["message"]["direction"].clone();
                if request["message"]["direction"] == "after" {
                    payload["entries"] = json!([]);
                    payload["startCursor"] = Value::Null;
                    payload["endCursor"] = Value::Null;
                }
                response
            }
            "subscription.release.request" => json!({"type":"session","message":{
                "type":"subscription.release.response","payload":{"subscriptionId":request["message"]["subscriptionId"]}}}),
            kind => panic!("unexpected reconnect test request: {kind}"),
        };
        response["message"]["payload"]["requestId"] = request["message"]["requestId"].clone();
        self.deliver(response, Duration::ZERO);
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
        let number = self.attempts.fetch_add(1, Ordering::SeqCst) + 1;
        if number > 1 && matches!(self.scenario, Scenario::Offline) {
            return Err("offline".into());
        }
        let mut socket = Socket {
            owner: Owner::current(),
            events: Rc::new(RefCell::new(events)),
            jobs: Vec::new(),
            number,
            scenario: self.scenario,
            trace: self.trace.clone(),
            histories: 0,
            projects: 0,
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

fn open(scenario: Scenario) -> (Headless, Arc<AtomicUsize>, Arc<Mutex<Trace>>) {
    let trace = Arc::new(Mutex::new(Trace::default()));
    let attempts = Arc::new(AtomicUsize::new(0));
    let mut config = EngineConfig::new(Root::of::<paseo_nsh::App>());
    config.clock = Clock::manual(SystemTime::UNIX_EPOCH);
    config.viewport = Size::new(Points(390.0), Points(844.0));
    config.environment.window = config.viewport;
    config.websocket = Some(Arc::new(Transport {
        scenario,
        attempts: attempts.clone(),
        trace: trace.clone(),
    }));
    let mut app = Headless::start(config).unwrap();
    app.wait_for("连接开发主机").unwrap();
    app.type_text("label:主机地址", "ws://localhost:6767?password=memory-only")
        .unwrap();
    app.press("role:button[连接主机]").unwrap();
    app.wait_for("RECONNECT_PROJECT").unwrap();
    app.press("role:button[RECONNECT_PROJECT]").unwrap();
    app.wait_for("已加载 1 个Agent").unwrap();
    app.press(&format!("role:button[Agent：{TITLE} · 空闲]"))
        .unwrap();
    app.wait_for("text:/^Agent ID：fixture-agent-a/").unwrap();
    app.press("role:button[打开会话]").unwrap();
    app.wait_for("会话已同步").unwrap();
    app.run_for(Duration::from_millis(20)).unwrap();
    (app, attempts, trace)
}

fn wait_for_retry(app: &mut Headless) {
    app.run_for(Duration::from_secs(1)).unwrap();
    app.wait_for("连接中断，将在 1 秒后自动重连；可取消重连")
        .unwrap();
    assert!(app.screen().enabled("role:button[取消重连]").unwrap());
}

#[test]
fn one_remote_close_reconnects_with_the_same_identity_and_restores_the_agent_subscription() {
    let (mut app, attempts, trace) = open(Scenario::SameServer);
    app.type_text("label:消息", "draft survives reconnect")
        .unwrap();
    wait_for_retry(&mut app);
    app.run_for(Duration::from_secs(1)).unwrap();
    app.wait_for("会话已同步").unwrap();
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    assert_eq!(
        app.screen().input_text("label:消息").unwrap(),
        Some("draft survives reconnect")
    );
    let trace = trace.lock().unwrap();
    let hellos: Vec<_> = trace
        .requests
        .iter()
        .filter(|(_, frame)| frame["type"] == "hello")
        .collect();
    assert_eq!(hellos.len(), 2);
    assert_eq!(hellos[0].1["clientId"], hellos[1].1["clientId"]);
    assert_eq!(hellos[1].1["auth"]["password"], "memory-only");
    assert!(trace.requests.iter().any(|(number, frame)| *number == 2
        && frame["message"]["type"] == "agent.timeline.set_subscription.request"
        && frame["message"]["agentIds"] == json!([AGENT])));
    assert!(trace.requests.iter().any(|(number, frame)| *number == 2
        && frame["message"]["type"] == "fetch_agent_timeline_request"
        && frame["message"]["agentId"] == AGENT));
}

#[test]
fn cancelling_retry_or_an_inflight_handshake_prevents_late_restore() {
    for scenario in [Scenario::SameServer, Scenario::SlowHandshake] {
        let (mut app, attempts, _) = open(scenario);
        wait_for_retry(&mut app);
        if matches!(scenario, Scenario::SlowHandshake) {
            app.run_for(Duration::from_secs(1)).unwrap();
        }
        let expected = attempts.load(Ordering::SeqCst);
        app.press("role:button[取消重连]").unwrap();
        app.wait_for("已断开连接").unwrap();
        app.run_for(Duration::from_secs(40)).unwrap();
        assert_eq!(attempts.load(Ordering::SeqCst), expected);
        assert!(app.screen().enabled("label:主机地址").unwrap());
        assert!(!app.screen().shows(&format!("会话：{TITLE}")));
    }
}

#[test]
fn identity_change_or_authentication_and_protocol_rejection_stop_without_restoring_routes() {
    for scenario in [
        Scenario::OtherServer,
        Scenario::Rejected("incorrect_password"),
        Scenario::Rejected("password_required"),
        Scenario::Rejected("incompatible_protocol"),
    ] {
        let (mut app, attempts, trace) = open(scenario);
        wait_for_retry(&mut app);
        app.run_for(Duration::from_secs(1)).unwrap();
        app.run_for(Duration::from_secs(40)).unwrap();
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        assert!(app.screen().enabled("label:主机地址").unwrap());
        assert!(!app.screen().shows(&format!("会话：{TITLE}")));
        assert!(
            !trace
                .lock()
                .unwrap()
                .requests
                .iter()
                .any(|(number, frame)| *number == 2 && frame["type"] == "session")
        );
        if matches!(scenario, Scenario::OtherServer) {
            app.wait_for("text:/server_identity_changed/").unwrap();
        }
    }
}

#[test]
fn a_connected_directory_failure_waits_for_explicit_refresh_before_route_restore() {
    let (mut app, attempts, trace) = open(Scenario::DirectoryFailure);
    wait_for_retry(&mut app);
    app.run_for(Duration::from_secs(1)).unwrap();
    app.wait_for("主机拒绝了请求，请检查主机状态后重试（request_rejected）")
        .unwrap();
    assert!(app.screen().enabled("role:button[刷新项目]").unwrap());
    assert!(!app.screen().shows(&format!("会话：{TITLE}")));
    assert!(
        !trace
            .lock()
            .unwrap()
            .requests
            .iter()
            .any(|(number, frame)| *number == 2
                && frame["message"]["type"] == "agent.timeline.set_subscription.request")
    );
    app.press("role:button[刷新项目]").unwrap();
    app.wait_for("会话已同步").unwrap();
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
}

#[test]
fn consecutive_transport_failures_back_off_and_can_be_cancelled() {
    let (mut app, attempts, _) = open(Scenario::Offline);
    wait_for_retry(&mut app);
    for (delay, expected, next) in [(1, 2, 2), (2, 3, 4), (4, 4, 8), (8, 5, 16), (16, 6, 30)] {
        app.run_for(Duration::from_secs(delay)).unwrap();
        assert_eq!(attempts.load(Ordering::SeqCst), expected);
        app.wait_for(&format!("连接中断，将在 {next} 秒后自动重连；可取消重连"))
            .unwrap();
    }
    app.press("role:button[取消重连]").unwrap();
    app.run_for(Duration::from_secs(40)).unwrap();
    assert_eq!(attempts.load(Ordering::SeqCst), 6);
}
