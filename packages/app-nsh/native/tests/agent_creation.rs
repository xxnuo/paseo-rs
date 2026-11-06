//! Real Rust root over deterministic wire captures; no daemon or model inference.

use nsh_core::{
    element::{Spawned, spawn},
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

struct Transport(Arc<Mutex<Vec<Value>>>);
struct Socket {
    events: Rc<RefCell<WebSocketEvents>>,
    jobs: Vec<Spawned<()>>,
    requests: Arc<Mutex<Vec<Value>>>,
    creates: usize,
    workspace_reads: usize,
}

fn frame(file: &str, kind: &str) -> Value {
    let entries: Vec<Value> = serde_json::from_str(file).unwrap();
    entries
        .into_iter()
        .find(|entry| entry["direction"] == "server" && entry["frame"]["message"]["type"] == kind)
        .unwrap()["frame"]
        .clone()
}

impl Socket {
    fn deliver(&mut self, event: WebSocketEvent) {
        let events = self.events.clone();
        self.jobs.push(spawn(async move {
            events.borrow_mut()(event);
        }));
    }
}

impl WebSocketTask for Socket {
    fn send(&mut self, data: WebSocketData) -> Result<(), String> {
        let WebSocketData::Text(text) = data else {
            panic!("unexpected binary request")
        };
        let request: Value = serde_json::from_str(&text).unwrap();
        self.requests.lock().unwrap().push(request.clone());
        let direct: Vec<Value> =
            serde_json::from_str(include_str!("../../protocol-fixtures/direct-session.json"))
                .unwrap();
        if request["type"] == "hello" {
            let mut response = direct[1]["frame"].clone();
            response["message"]["payload"]["features"]["creationLifecycle"] = json!(true);
            self.deliver(WebSocketEvent::Message(WebSocketData::Text(
                response.to_string(),
            )));
            return Ok(());
        }
        let kind = request["message"]["type"].as_str().unwrap();
        let catalog = include_str!("../../protocol-fixtures/catalog.json");
        let chat = include_str!("../../protocol-fixtures/conversation.json");
        let mut response = match kind {
            "project.list.request" => {
                let mut value = direct[5]["frame"].clone();
                value["message"]["payload"]["projects"][0]["projectDisplayName"] =
                    json!("CREATION_PROJECT");
                value
            }
            "fetch_workspaces_request" => {
                self.workspace_reads += 1;
                let mut value = frame(catalog, "fetch_workspaces_response");
                if self.workspace_reads > 1 {
                    value["message"]["payload"]["entries"][0]["workspaceDirectory"] =
                        json!("/fixture/moved-worktree");
                }
                value
            }
            "fetch_agents_request" => frame(catalog, "fetch_agents_response"),
            "get_providers_snapshot_request" => {
                let mut value = frame(catalog, "get_providers_snapshot_response");
                value["message"]["payload"]["entries"]
                    .as_array_mut()
                    .unwrap()
                    .truncate(1);
                value["message"]["payload"]["entries"][0]["label"] = json!("CREATION_PROVIDER");
                value
            }
            "list_provider_features_request" => frame(catalog, "list_provider_features_response"),
            "agent.create.request" => {
                self.creates += 1;
                if self.creates == 1 {
                    return Ok(());
                }
                let mut value = frame(chat, "agent.create.response");
                if self.creates == 2 {
                    value["message"]["payload"]["agent"] = Value::Null;
                    value["message"]["payload"]["error"] = json!("snapshot write failed");
                    value["message"]["payload"]
                        .as_object_mut()
                        .unwrap()
                        .remove("creation");
                } else {
                    value["message"]["payload"]["creation"]["idempotencyKey"] =
                        request["message"]["idempotencyKey"].clone();
                    value["message"]["payload"]["creation"]["agentId"] = json!("created-ui-agent");
                }
                value
            }
            "creation.subscribe.request" => json!({"type":"session","message":{
                "type":"creation.subscribe.response", "payload":{"snapshot":null,"error":null}
            }}),
            _ => panic!("unexpected request type: {kind}"),
        };
        response["message"]["payload"]["requestId"] = request["message"]["requestId"].clone();
        self.deliver(WebSocketEvent::Message(WebSocketData::Text(
            response.to_string(),
        )));
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
            requests: self.0.clone(),
            creates: 0,
            workspace_reads: 0,
        };
        socket.deliver(WebSocketEvent::Open {
            protocol: String::new(),
        });
        Ok(Box::new(socket))
    }
}

#[test]
fn uncertain_creation_restores_original_config_and_retries_the_same_identity() {
    exercise_recovery(true);
}

#[test]
fn leaving_during_creation_and_moving_the_workspace_preserves_the_original_request() {
    exercise_recovery(false);
}

fn exercise_recovery(wait_for_timeout: bool) {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let mut config = EngineConfig::new(Root::of::<paseo_nsh::App>());
    config.clock = Clock::manual(SystemTime::UNIX_EPOCH);
    config.websocket = Some(Arc::new(Transport(requests.clone())));
    let mut app = Headless::start(config).unwrap();
    app.wait_for("连接开发主机").unwrap();
    app.type_text("label:主机地址", "ws://localhost:6767")
        .unwrap();
    app.press("role:button[连接主机]").unwrap();
    app.wait_for("CREATION_PROJECT").unwrap();
    app.press("role:button[CREATION_PROJECT]").unwrap();
    app.wait_for("已加载 2 个工作区").unwrap();
    let catalog: Vec<Value> =
        serde_json::from_str(include_str!("../../protocol-fixtures/catalog.json")).unwrap();
    let workspaces = &catalog
        .iter()
        .find(|row| row["frame"]["message"]["type"] == "fetch_workspaces_response")
        .unwrap()["frame"]["message"]["payload"]["entries"];
    let workspace = &workspaces[0];
    app.press(&format!(
        "text:工作区：{} · 空闲 >> nth=0",
        workspace["name"].as_str().unwrap()
    ))
    .unwrap();
    app.wait_for("服务商：CREATION_PROVIDER · 可用").unwrap();
    app.press("text:服务商：CREATION_PROVIDER · 可用").unwrap();
    app.wait_for("新建 Agent").unwrap();
    app.type_text("label:Agent 名称（可选）", "Recovered creation")
        .unwrap();
    app.press("role:button[创建 Agent]").unwrap();
    app.run_for(if wait_for_timeout {
        Duration::from_secs(11)
    } else {
        Duration::from_millis(1)
    })
    .unwrap();
    assert!(!app.screen().enabled("role:button[创建 Agent]").unwrap());
    // Destroy/recreate the form owner, as when a user leaves and returns to the provider.
    app.press("text:服务商：CREATION_PROVIDER · 可用").unwrap();
    if !wait_for_timeout {
        app.press("role:button[刷新工作区]").unwrap();
        app.wait_for("已加载 2 个工作区").unwrap();
        app.press(&format!(
            "text:工作区：{} · 空闲 >> nth=0",
            workspace["name"].as_str().unwrap()
        ))
        .unwrap();
        app.wait_for("服务商目录：/fixture/moved-worktree").unwrap();
        app.wait_for("服务商：CREATION_PROVIDER · 可用").unwrap();
    }
    app.press("text:服务商：CREATION_PROVIDER · 可用").unwrap();
    app.wait_for("有尚未确认的创建请求，请查询创建结果；重试将复用原创建标识和配置")
        .unwrap();
    assert!(!app.screen().enabled("label:Agent 名称（可选）").unwrap());
    app.press("role:button[查询创建结果]").unwrap();
    app.wait_for("主机尚无此创建记录；保留创建标识，请稍后再次查询")
        .unwrap();
    app.press("role:button[重试同一次创建]").unwrap();
    app.wait_for("创建结果尚未确认：snapshot write failed。请查询创建结果或重试同一次创建")
        .unwrap();
    assert!(!app.screen().enabled("role:button[创建 Agent]").unwrap());
    app.press("role:button[重试同一次创建]").unwrap();
    app.wait_for("已创建 Agent：created-ui-agent。刷新 Agent 列表即可查看")
        .unwrap();
    assert!(app.screen().enabled("role:button[创建 Agent]").unwrap());
    assert!(!app.screen().enabled("role:button[重试同一次创建]").unwrap());
    let requests = requests.lock().unwrap();
    let creates: Vec<_> = requests
        .iter()
        .filter(|value| value["message"]["type"] == "agent.create.request")
        .collect();
    assert_eq!(creates.len(), 3);
    assert_ne!(
        creates[0]["message"]["requestId"],
        creates[1]["message"]["requestId"]
    );
    assert_eq!(
        creates[0]["message"]["idempotencyKey"],
        creates[1]["message"]["idempotencyKey"]
    );
    assert_eq!(
        creates[0]["message"]["config"],
        creates[1]["message"]["config"]
    );
    assert_eq!(
        creates[1]["message"]["config"],
        creates[2]["message"]["config"]
    );
    assert_eq!(
        creates[1]["message"]["idempotencyKey"],
        creates[2]["message"]["idempotencyKey"]
    );
    let sent = &creates[1]["message"];
    assert!(sent["config"].get("model").is_none());
    assert!(sent["config"].get("modeId").is_none());
    assert_eq!(sent["config"]["title"], "Recovered creation");
    assert_eq!(sent["workspaceId"], workspace["id"]);
    assert_eq!(sent["config"]["cwd"], workspace["workspaceDirectory"]);
    let observed = requests
        .iter()
        .find(|value| value["message"]["type"] == "creation.subscribe.request")
        .unwrap();
    assert_eq!(
        observed["message"]["idempotencyKey"],
        sent["idempotencyKey"]
    );
}
