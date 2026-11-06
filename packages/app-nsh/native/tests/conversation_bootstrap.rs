//! Real Rust root and WebSocket facade with captured protocol messages and a manual clock.

use nsh_core::{
    element::{Spawned, spawn, timer},
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
const TITLE: &str = "Bootstrap running Agent";
const LIVE_TEXT: &str = "BOOTSTRAP_LIVE_CHUNK";
const EVENTS_TOKEN: &str = "bootstrap-events";
const TIMELINE_TOKEN: &str = "bootstrap-timeline";
const DIRECT: &str = include_str!("../../protocol-fixtures/direct-session.json");
const CATALOG: &str = include_str!("../../protocol-fixtures/catalog.json");
const CONVERSATION: &str = include_str!("../../protocol-fixtures/conversation.json");
const TIMELINE: &str = include_str!("../../protocol-fixtures/timeline.json");

#[derive(Default)]
struct Trace {
    requests: Vec<Value>,
    delivered: Vec<Value>,
}

struct Transport {
    trace: Arc<Mutex<Trace>>,
    initial_history_delay: Duration,
}

struct Socket {
    events: Rc<RefCell<WebSocketEvents>>,
    jobs: Vec<Spawned<()>>,
    trace: Arc<Mutex<Trace>>,
    initial_history_delay: Duration,
    history_requests: usize,
    stopped: bool,
}

fn captured(file: &str, kind: &str) -> Value {
    let rows: Vec<Value> = serde_json::from_str(file).unwrap();
    rows.into_iter()
        .find(|row| row["direction"] == "server" && row["frame"]["message"]["type"] == kind)
        .unwrap()["frame"]
        .clone()
}

fn agent(stopped: bool) -> Value {
    let mut value =
        captured(CATALOG, "fetch_agent_response")["message"]["payload"]["agent"].clone();
    value["title"] = json!(TITLE);
    value["status"] = json!(if stopped { "idle" } else { "running" });
    value["activeTurn"] = if stopped {
        Value::Null
    } else {
        json!({"turnId":"existing-turn", "startedAt":"2026-11-06T00:00:00.000Z"})
    };
    value
}

impl Socket {
    fn deliver(&mut self, frame: Value, delay: Duration) {
        let events = self.events.clone();
        let trace = self.trace.clone();
        self.jobs.push(spawn(async move {
            if !delay.is_zero() {
                timer(delay).await;
            }
            trace.lock().unwrap().delivered.push(frame.clone());
            events.borrow_mut()(WebSocketEvent::Message(WebSocketData::Text(
                frame.to_string(),
            )));
        }));
    }

    fn history(&self, request: &Value) -> Value {
        let mut response = captured(TIMELINE, "fetch_agent_timeline_response");
        let payload = &mut response["message"]["payload"];
        payload["agentId"] = json!(AGENT);
        payload["agent"] = agent(self.stopped);
        payload["direction"] = request["message"]["direction"].clone();
        payload["window"]["maxSeq"] = json!(5);
        payload["window"]["nextSeq"] = json!(6);
        if request["message"]["direction"] == "after" {
            payload["entries"] = json!([]);
            payload["startCursor"] = Value::Null;
            payload["endCursor"] = Value::Null;
        } else {
            payload["endCursor"]["seq"] = json!(5);
            payload["entries"].as_array_mut().unwrap().push(json!({
                "provider":"codex", "item":{"type":"assistant_message","text":LIVE_TEXT},
                "timestamp":"2026-11-06T00:00:00.000Z", "seqStart":5,"seqEnd":5,
                "sourceSeqRanges":[{"startSeq":5,"endSeq":5}],"turnId":"existing-turn","collapsed":[]
            }));
        }
        response
    }
}

impl WebSocketTask for Socket {
    fn send(&mut self, data: WebSocketData) -> Result<(), String> {
        let WebSocketData::Text(text) = data else {
            panic!("unexpected binary request")
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

        let kind = request["message"]["type"].as_str().unwrap();
        let mut delay = Duration::ZERO;
        let mut response = match kind {
            "project.list.request" => {
                // The capture contains an initial empty list followed by the seeded project.
                let rows: Vec<Value> = serde_json::from_str(DIRECT).unwrap();
                let mut response = rows[5]["frame"].clone();
                response["message"]["payload"]["projects"][0]["projectDisplayName"] =
                    json!("BOOTSTRAP_PROJECT");
                response
            }
            "fetch_workspaces_request" => captured(CATALOG, "fetch_workspaces_response"),
            "fetch_agents_request" => {
                let mut response = captured(CATALOG, "fetch_agents_response");
                let entries = response["message"]["payload"]["entries"]
                    .as_array_mut()
                    .unwrap();
                entries.retain(|entry| entry["agent"]["id"] == AGENT);
                entries[0]["agent"] = agent(self.stopped);
                response
            }
            "fetch_agent_request" => {
                assert_eq!(request["message"]["agentId"], AGENT);
                let mut response = captured(CATALOG, "fetch_agent_response");
                response["message"]["payload"]["agent"] = agent(self.stopped);
                response
            }
            "session.events.set_subscription.request" => {
                let mut response =
                    captured(CONVERSATION, "session.events.set_subscription.response");
                response["message"]["payload"]["subscriptionId"] = json!(EVENTS_TOKEN);
                response
            }
            "agent.timeline.set_subscription.request" => {
                assert_eq!(request["message"]["agentIds"], json!([AGENT]));
                let mut response =
                    captured(CONVERSATION, "agent.timeline.set_subscription.response");
                response["message"]["payload"]["agentIds"] = json!([AGENT]);
                response["message"]["payload"]["subscriptionId"] = json!(TIMELINE_TOKEN);
                response
            }
            "fetch_agent_timeline_request" => {
                assert_eq!(request["message"]["agentId"], AGENT);
                assert_eq!(request["message"]["wholeTurns"], true);
                self.history_requests += 1;
                if self.history_requests == 1 {
                    // An existing run can stream before its bootstrap snapshot, without a new turn_started.
                    self.deliver(json!({"type":"session","message":{"type":"agent_stream","payload":{
                        "agentId":AGENT,"subscriptionId":TIMELINE_TOKEN,"seq":5,"epoch":"fixture-epoch",
                        "timestamp":"2026-11-06T00:00:00.000Z",
                        "event":{"type":"timeline","provider":"codex","turnId":"existing-turn",
                            "item":{"type":"assistant_message","text":LIVE_TEXT}}
                    }}}), Duration::ZERO);
                    delay = self.initial_history_delay;
                } else {
                    // Keep bootstrap's catch-up snapshot from hiding an ignored initial Running snapshot.
                    delay = Duration::from_secs(1);
                }
                self.history(&request)
            }
            "cancel_agent_request" => {
                assert_eq!(request["message"]["agentId"], AGENT);
                self.stopped = true;
                let mut response = captured(CONVERSATION, "cancel_agent_response");
                response["message"]["payload"]["agentId"] = json!(AGENT);
                response["message"]["payload"]["agent"] = agent(true);
                response
            }
            "subscription.release.request" => json!({"type":"session","message":{
                "type":"subscription.release.response","payload":{
                    "subscriptionId":request["message"]["subscriptionId"]
                }
            }}),
            _ => panic!("unexpected request type: {kind}"),
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
            events: Rc::new(RefCell::new(events)),
            jobs: Vec::new(),
            trace: self.trace.clone(),
            initial_history_delay: self.initial_history_delay,
            history_requests: 0,
            stopped: false,
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

fn open_conversation(delay: Duration) -> (Headless, Arc<Mutex<Trace>>) {
    let trace = Arc::new(Mutex::new(Trace::default()));
    let mut config = EngineConfig::new(Root::of::<paseo_nsh::App>());
    config.clock = Clock::manual(SystemTime::UNIX_EPOCH);
    config.websocket = Some(Arc::new(Transport {
        trace: trace.clone(),
        initial_history_delay: delay,
    }));
    let mut app = Headless::start(config).unwrap();
    app.wait_for("连接开发主机").unwrap();
    app.type_text("label:主机地址", "ws://localhost:6767")
        .unwrap();
    app.press("role:button[连接主机]").unwrap();
    app.wait_for("BOOTSTRAP_PROJECT").unwrap();
    app.press("role:button[BOOTSTRAP_PROJECT]").unwrap();
    app.wait_for("已加载 1 个Agent").unwrap();
    app.press(&format!("role:button[Agent：{TITLE} · 运行中]"))
        .unwrap();
    app.wait_for("text:/^Agent ID：fixture-agent-a/").unwrap();
    app.press("role:button[打开会话]").unwrap();
    app.wait_for("正在同步会话…").unwrap();
    app.run_for(Duration::from_millis(20)).unwrap();
    (app, trace)
}

fn assert_released(trace: &Trace) {
    let mut tokens: Vec<_> = trace
        .requests
        .iter()
        .filter(|request| request["message"]["type"] == "subscription.release.request")
        .map(|request| request["message"]["subscriptionId"].as_str().unwrap())
        .collect();
    tokens.sort_unstable();
    assert_eq!(tokens, vec![EVENTS_TOKEN, TIMELINE_TOKEN]);
}

#[test]
fn a_chunk_before_bootstrap_preserves_the_existing_turn_and_stop_can_confirm_idle() {
    let (mut app, trace) = open_conversation(Duration::from_secs(1));
    {
        let trace = trace.lock().unwrap();
        assert!(
            trace
                .delivered
                .iter()
                .any(|frame| frame["message"]["type"] == "agent_stream")
        );
        assert!(
            !trace
                .delivered
                .iter()
                .any(|frame| frame["message"]["type"] == "fetch_agent_timeline_response")
        );
    }
    app.run_for(Duration::from_secs(1)).unwrap();
    app.wait_for("已加载 4 条历史消息").unwrap();
    app.wait_for(LIVE_TEXT).unwrap();
    assert!(app.screen().enabled("role:button[停止]").unwrap());
    app.run_for(Duration::from_secs(2)).unwrap();
    app.wait_for("会话已同步").unwrap();
    app.press("role:button[停止]").unwrap();
    app.run_for(Duration::from_secs(2)).unwrap();
    app.wait_for("会话已同步").unwrap();
    assert!(!app.screen().enabled("role:button[停止]").unwrap());
    app.type_text("label:消息", "draft after confirmed stop")
        .unwrap();
    assert!(app.screen().enabled("role:button[发送]").unwrap());
    app.press("role:button[返回 Agent 详情]").unwrap();
    app.wait_for("text:/^Agent ID：fixture-agent-a/").unwrap();
    app.run_for(Duration::from_millis(20)).unwrap();
    let trace = trace.lock().unwrap();
    assert_eq!(
        trace
            .requests
            .iter()
            .filter(|request| request["message"]["type"] == "cancel_agent_request")
            .count(),
        1
    );
    assert!(
        !trace
            .delivered
            .iter()
            .any(|frame| frame["message"]["payload"]["event"]["type"] == "turn_started")
    );
    assert_released(&trace);
}

#[test]
fn leaving_before_bootstrap_releases_tokens_and_late_history_cannot_reopen_the_conversation() {
    let (mut app, trace) = open_conversation(Duration::from_secs(5));
    app.press("role:button[返回 Agent 详情]").unwrap();
    app.wait_for("text:/^Agent ID：fixture-agent-a/").unwrap();
    app.run_for(Duration::from_secs(6)).unwrap();
    assert!(app.screen().shows("text:/^Agent ID：fixture-agent-a/"));
    assert!(!app.screen().shows(&format!("text:会话：{TITLE}")));
    assert!(!app.screen().shows(LIVE_TEXT));
    {
        let trace = trace.lock().unwrap();
        assert!(
            trace
                .delivered
                .iter()
                .any(|frame| frame["message"]["type"] == "fetch_agent_timeline_response")
        );
        assert_released(&trace);
    }
    app.press("role:button[返回项目列表]").unwrap();
    app.wait_for("已连接：fixture-host").unwrap();
    assert!(!app.screen().enabled("label:主机地址").unwrap());
}
