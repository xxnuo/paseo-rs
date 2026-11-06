//! Actual root and NSH WebSocket facade, with deterministic transport events and a manual clock.

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
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, SystemTime},
};

struct Transport {
    idle_close: bool,
    drops: Arc<AtomicUsize>,
}

struct Socket {
    events: Rc<RefCell<WebSocketEvents>>,
    jobs: Vec<Spawned<()>>,
    idle_close: bool,
    drops: Arc<AtomicUsize>,
}

impl Socket {
    fn later(&mut self, event: WebSocketEvent, delay: Duration) {
        let events = self.events.clone();
        self.jobs.push(spawn(async move {
            if !delay.is_zero() {
                timer(delay).await;
            }
            events.borrow_mut()(event);
        }));
    }
}

impl Drop for Socket {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}

impl WebSocketTask for Socket {
    fn send(&mut self, data: WebSocketData) -> Result<(), String> {
        let WebSocketData::Text(text) = data else {
            return Err("expected text".to_owned());
        };
        let request: Value = serde_json::from_str(&text).unwrap();
        let capture: Vec<Value> =
            serde_json::from_str(include_str!("../../protocol-fixtures/direct-session.json"))
                .unwrap();
        if request["type"] == "hello" {
            self.later(
                WebSocketEvent::Message(WebSocketData::Text(capture[1]["frame"].to_string())),
                Duration::ZERO,
            );
        } else if request["message"]["type"] == "project.list.request" {
            let mut response = capture[5]["frame"].clone();
            response["message"]["payload"]["requestId"] = request["message"]["requestId"].clone();
            response["message"]["payload"]["projects"][0]["projectDisplayName"] =
                json!("LIVE_PUMP_PROJECT");
            self.later(
                WebSocketEvent::Message(WebSocketData::Text(response.to_string())),
                Duration::ZERO,
            );
            if self.idle_close {
                self.later(
                    WebSocketEvent::Close {
                        code: 1000,
                        reason: String::new(),
                        was_clean: true,
                    },
                    Duration::from_secs(1),
                );
            }
        } else if matches!(
            request["message"]["type"].as_str(),
            Some(
                "fetch_workspaces_request"
                    | "fetch_agents_request"
                    | "get_providers_snapshot_request"
            )
        ) {
            let catalog: Vec<Value> =
                serde_json::from_str(include_str!("../../protocol-fixtures/catalog.json")).unwrap();
            let response_type = match request["message"]["type"].as_str().unwrap() {
                "fetch_workspaces_request" => "fetch_workspaces_response",
                "fetch_agents_request" => "fetch_agents_response",
                _ => "get_providers_snapshot_response",
            };
            let mut response = catalog
                .iter()
                .find(|entry| entry["frame"]["message"]["type"] == response_type)
                .unwrap()["frame"]
                .clone();
            response["message"]["payload"]["requestId"] = request["message"]["requestId"].clone();
            let mut delay = 5;
            if response_type == "fetch_workspaces_response" {
                // Distinct worktrees make a project-root substitution visible to this UI test.
                for (index, workspace) in response["message"]["payload"]["entries"]
                    .as_array_mut()
                    .unwrap()
                    .iter_mut()
                    .enumerate()
                {
                    workspace["name"] = json!(format!("worktree-{}", index + 1));
                    workspace["workspaceDirectory"] =
                        json!(format!("/fixture/worktree-{}", index + 1));
                }
            } else if response_type == "get_providers_snapshot_response" {
                let cwd = request["message"]["cwd"].as_str().unwrap();
                assert!(matches!(cwd, "/fixture/worktree-1" | "/fixture/worktree-2"));
                let old = cwd.ends_with('1');
                delay = if old { 7 } else { 2 };
                response["message"]["payload"]["entries"]
                    .as_array_mut()
                    .unwrap()
                    .truncate(1);
                response["message"]["payload"]["entries"][0]["label"] = json!(if old {
                    "OLD_WORKTREE"
                } else {
                    "CURRENT_WORKTREE"
                });
            }
            self.later(
                WebSocketEvent::Message(WebSocketData::Text(response.to_string())),
                Duration::from_secs(delay),
            );
        }
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
            idle_close: self.idle_close,
            drops: self.drops.clone(),
        };
        socket.later(
            WebSocketEvent::Open {
                protocol: String::new(),
            },
            Duration::ZERO,
        );
        Ok(Box::new(socket))
    }
}

fn connected_app(idle_close: bool, drops: Arc<AtomicUsize>) -> Headless {
    let mut config = EngineConfig::new(Root::of::<paseo_nsh::App>());
    config.clock = Clock::manual(SystemTime::UNIX_EPOCH);
    config.websocket = Some(Arc::new(Transport { idle_close, drops }));
    let mut app = Headless::start(config).unwrap();
    app.wait_for("连接开发主机").unwrap();
    app.type_text("label:主机地址", "ws://localhost:6767")
        .unwrap();
    app.press("role:button[连接主机]").unwrap();
    app.wait_for("LIVE_PUMP_PROJECT").unwrap();
    app.wait_for("已连接：fixture-host").unwrap();
    app
}

#[test]
fn an_idle_remote_close_clears_projects_and_restores_the_host_form() {
    let drops = Arc::new(AtomicUsize::new(0));
    let mut app = connected_app(true, drops.clone());
    app.run_for(Duration::from_secs(2)).unwrap();
    app.wait_for("主机已断开连接，请重新连接（closed）")
        .unwrap();
    assert!(!app.screen().shows("LIVE_PUMP_PROJECT"));
    assert!(app.screen().enabled("label:主机地址").unwrap());
    assert!(app.screen().enabled("role:button[连接主机]").unwrap());
    assert!(!app.screen().enabled("role:button[刷新项目]").unwrap());
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

#[test]
fn dropping_the_root_releases_an_idle_socket_with_a_pending_read() {
    let drops = Arc::new(AtomicUsize::new(0));
    let app = connected_app(false, drops.clone());
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    drop(app);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

#[test]
fn leaving_a_project_cancels_page_requests_and_late_replies_cannot_replace_the_host_screen() {
    let drops = Arc::new(AtomicUsize::new(0));
    let mut app = connected_app(false, drops.clone());
    app.press("role:button[LIVE_PUMP_PROJECT]").unwrap();
    app.wait_for("正在加载工作区…").unwrap();
    app.press("role:button[返回项目列表]").unwrap();
    app.wait_for("已连接：fixture-host").unwrap();
    app.run_for(Duration::from_secs(6)).unwrap();
    assert!(!app.screen().shows("项目：LIVE_PUMP_PROJECT"));
    assert!(app.screen().shows("已连接：fixture-host"));
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    app.press("role:button[LIVE_PUMP_PROJECT]").unwrap();
    app.wait_for("正在加载工作区…").unwrap();
    app.run_for(Duration::from_secs(6)).unwrap();
    app.wait_for("已加载 2 个工作区").unwrap();
    app.wait_for("已加载 2 个Agent").unwrap();
    app.press("role:button[断开连接]").unwrap();
    app.wait_for("已断开连接").unwrap();
    assert!(!app.screen().shows("项目：LIVE_PUMP_PROJECT"));
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

#[test]
fn switching_workspaces_uses_their_real_directories_and_ignores_the_old_provider_reply() {
    let mut app = connected_app(false, Arc::new(AtomicUsize::new(0)));
    app.press("role:button[LIVE_PUMP_PROJECT]").unwrap();
    app.wait_for("正在加载工作区…").unwrap();
    app.run_for(Duration::from_secs(6)).unwrap();
    app.press("text:工作区：worktree-1 · 空闲").unwrap();
    app.wait_for("服务商目录：/fixture/worktree-1").unwrap();
    app.press("text:工作区：worktree-2 · 空闲").unwrap();
    app.wait_for("服务商目录：/fixture/worktree-2").unwrap();
    app.run_for(Duration::from_secs(3)).unwrap();
    app.wait_for("服务商：CURRENT_WORKTREE · 可用").unwrap();
    app.run_for(Duration::from_secs(5)).unwrap();
    assert!(app.screen().shows("服务商：CURRENT_WORKTREE · 可用"));
    assert!(!app.screen().shows("OLD_WORKTREE"));
    assert!(!app.screen().shows("服务商目录：/fixture/worktree-1"));
}
