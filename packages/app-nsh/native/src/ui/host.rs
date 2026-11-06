use crate::{
    client::{Client, ClientError},
    host::{ParsedHost, parse_direct_host},
    protocol::{Project, ProjectList},
    state::viewed::ViewedConversations,
};
#[path = "conversation.rs"]
mod conversation;
#[path = "project.rs"]
mod project;
use nsh_base::{button, input, nav_page, nav_stack, theme};
use nsh_core::{prelude::*, storage::Storage};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    time::Duration,
};

const CLIENT_ID_KEY: &str = "paseo.nsh.client-id";

struct SavedRoutes {
    projects: Vec<Project>,
    agents: Vec<String>,
}

struct Connection {
    owner: Owner,
    client: RefCell<Option<Rc<Client>>>,
    task: RefCell<Option<Spawned<()>>>,
    generation: Cell<u64>,
    busy: Signal<bool>,
    connected: Signal<bool>,
    status: Signal<String>,
    projects: Signal<Vec<Project>>,
    projects_loaded: Signal<bool>,
    routes: Signal<Vec<Project>>,
    conversations: Signal<Vec<String>>,
    remembered_agent: RefCell<Option<String>>,
    hostname: Signal<String>,
    reconnect_host: RefCell<Option<ParsedHost>>,
    server_id: RefCell<Option<String>>,
    saved_routes: RefCell<Option<SavedRoutes>>,
    retry_attempt: Cell<u32>,
    reconnecting: Signal<bool>,
    viewed: RefCell<ViewedConversations>,
}

impl Connection {
    fn new() -> Rc<Self> {
        let state = Rc::new(Self {
            owner: Owner::current(),
            client: RefCell::new(None),
            task: RefCell::new(None),
            generation: Cell::new(0),
            busy: signal(false),
            connected: signal(false),
            status: signal(String::new()),
            projects: signal(Vec::new()),
            projects_loaded: signal(false),
            routes: signal(Vec::new()),
            conversations: signal(Vec::new()),
            remembered_agent: RefCell::new(None),
            hostname: signal(String::new()),
            reconnect_host: RefCell::new(None),
            server_id: RefCell::new(None),
            saved_routes: RefCell::new(None),
            retry_attempt: Cell::new(0),
            reconnecting: signal(false),
            viewed: RefCell::new(ViewedConversations::default()),
        });
        let cleanup_state = state.clone();
        cleanup(move || {
            cleanup_state
                .generation
                .set(cleanup_state.generation.get().wrapping_add(1));
            if let Some(task) = cleanup_state.task.take() {
                task.cancel();
            }
            cleanup_state.client.take();
            cleanup_state.reconnect_host.take();
            cleanup_state.saved_routes.take();
        });
        state
    }

    fn connect(self: &Rc<Self>, host: ParsedHost) {
        if self.busy.get_untracked() || self.connected.get_untracked() {
            return;
        }
        self.saved_routes.take();
        self.viewed.borrow_mut().clear();
        self.remembered_agent.take();
        self.server_id.take();
        self.retry_attempt.set(0);
        self.reconnecting.set(false);
        self.reconnect_host.replace(Some(copy_host(&host)));
        self.connect_attempt(host, false);
    }

    fn connect_attempt(self: &Rc<Self>, host: ParsedHost, automatic: bool) {
        let client_id = match client_identity() {
            Ok(id) => id,
            Err(message) => {
                self.disconnect();
                self.status.set(message.to_owned());
                return;
            }
        };
        self.busy.set(true);
        let generation = self.next_generation();
        self.projects.set(Vec::new());
        self.projects_loaded.set(false);
        self.status.set(
            if automatic {
                "正在重新连接主机…"
            } else {
                "正在连接主机…"
            }
            .to_owned(),
        );
        let state = self.clone();
        let weak = Rc::downgrade(self);
        let task = self.owner.run(|| {
            spawn(async move {
                let label = host.label.clone();
                let client = match Client::connect(host, &client_id, move |error| {
                    if let Some(state) = weak.upgrade() {
                        state.failed(generation, error);
                    }
                })
                .await
                {
                    Ok(client) => client,
                    Err(error) => {
                        state.failed(generation, error);
                        return;
                    }
                };
                if !state.current(generation) {
                    return;
                }
                if automatic && state.server_id.borrow().as_deref() != Some(client.server_info().server_id.as_str()) {
                    state.disconnect();
                    state.status.set("主机身份已变化，已停止自动重连，请确认地址后重新连接（server_identity_changed）".into());
                    return;
                }
                state.server_id.replace(Some(client.server_info().server_id.clone()));
                state.retry_attempt.set(0);
                state.reconnecting.set(false);
                state
                    .hostname
                    .set(client.server_info().hostname.clone().unwrap_or(label));
                state.status.set("已连接，正在读取项目…".to_owned());
                let client = Rc::new(client);
                state.client.replace(Some(client.clone()));
                state.connected.set(true);
                state.projects_finished(generation, client.projects().await);
            })
        });
        self.task.replace(Some(task));
    }

    fn refresh(self: &Rc<Self>) {
        if self.busy.get_untracked() {
            return;
        }
        let client = self.client.borrow().clone();
        let Some(client) = client else {
            return;
        };
        let generation = self.generation.get();
        self.busy.set(true);
        self.status.set("正在读取项目…".to_owned());
        let state = self.clone();
        let task = self.owner.run(|| {
            spawn(async move {
                state.projects_finished(generation, client.projects().await);
            })
        });
        self.task.replace(Some(task));
    }

    fn next_generation(&self) -> u64 {
        let generation = self.generation.get().wrapping_add(1);
        self.generation.set(generation);
        generation
    }

    fn current(&self, generation: u64) -> bool {
        self.owner.is_live() && self.generation.get() == generation
    }

    fn projects_finished(
        self: &Rc<Self>,
        generation: u64,
        result: Result<ProjectList, ClientError>,
    ) {
        if !self.current(generation) {
            return;
        }
        match result {
            Ok(projects) => self.loaded(projects),
            Err(error) => {
                let connected = self
                    .client
                    .borrow()
                    .as_ref()
                    .is_some_and(|client| client.is_connected());
                if connected {
                    self.busy.set(false);
                    self.status.set(format!("{error}（{}）", error.code()));
                } else {
                    self.failed(generation, error);
                }
            }
        }
    }

    fn loaded(&self, projects: ProjectList) {
        if let Some(saved) = self.saved_routes.take() {
            let restored: Vec<_> = saved
                .projects
                .iter()
                .filter_map(|previous| {
                    projects
                        .projects
                        .iter()
                        .find(|project| project.project_id == previous.project_id)
                        .cloned()
                })
                .collect();
            let can_restore_agents = !restored.is_empty() && restored.len() == saved.projects.len();
            if can_restore_agents {
                self.remembered_agent.replace(saved.agents.first().cloned());
            }
            self.routes.set(restored);
            if can_restore_agents {
                self.conversations.set(saved.agents);
            }
        }
        self.projects.set(projects.projects);
        self.projects_loaded.set(true);
        self.connected.set(true);
        self.busy.set(false);
        self.status
            .set(format!("已连接：{}", self.hostname.get_untracked()));
    }

    fn failed(self: &Rc<Self>, generation: u64, error: ClientError) {
        if !self.current(generation) {
            return;
        }
        self.next_generation();
        if let Some(task) = self.task.take() {
            task.cancel();
        }
        let retry = self.server_id.borrow().is_some()
            && self.reconnect_host.borrow().is_some()
            && retryable(error);
        if retry && self.saved_routes.borrow().is_none() {
            self.saved_routes.replace(Some(SavedRoutes {
                projects: self.routes.get_untracked(),
                agents: self.conversations.get_untracked(),
            }));
        }
        self.routes.set(Vec::new());
        self.conversations.set(Vec::new());
        self.client.take();
        self.connected.set(false);
        self.busy.set(false);
        self.projects.set(Vec::new());
        self.projects_loaded.set(false);
        self.status.set(format!("{error}（{}）", error.code()));
        if retry {
            self.schedule_reconnect();
        } else {
            self.reconnecting.set(false);
            self.reconnect_host.take();
            self.saved_routes.take();
            self.server_id.take();
        }
    }

    fn schedule_reconnect(self: &Rc<Self>) {
        let attempt = self.retry_attempt.get();
        self.retry_attempt.set(attempt.saturating_add(1));
        let seconds = reconnect_delay(attempt);
        self.reconnecting.set(true);
        self.busy.set(true);
        self.status
            .set(format!("连接中断，将在 {seconds} 秒后自动重连；可取消重连"));
        let generation = self.generation.get();
        let state = self.clone();
        let task = self.owner.run(|| {
            spawn(async move {
                timer(Duration::from_secs(seconds)).await;
                if !state.current(generation) || !state.reconnecting.get_untracked() {
                    return;
                }
                let host = state.reconnect_host.borrow().as_ref().map(copy_host);
                if let Some(host) = host {
                    state.connect_attempt(host, true);
                }
            })
        });
        self.task.replace(Some(task));
    }

    fn disconnect(&self) {
        self.next_generation();
        self.reconnecting.set(false);
        self.reconnect_host.take();
        self.server_id.take();
        self.saved_routes.take();
        self.viewed.borrow_mut().clear();
        self.remembered_agent.take();
        self.retry_attempt.set(0);
        if let Some(task) = self.task.take() {
            task.cancel();
        }
        self.routes.set(Vec::new());
        self.conversations.set(Vec::new());
        self.client.take();
        self.connected.set(false);
        self.busy.set(false);
        self.projects.set(Vec::new());
        self.projects_loaded.set(false);
        self.hostname.set(String::new());
        self.status.set("已断开连接".to_owned());
    }

    fn open_project(&self, project: Project) {
        if self.connected.get_untracked() && !self.busy.get_untracked() {
            self.remembered_agent.take();
            self.conversations.set(Vec::new());
            self.routes.set(vec![project]);
        }
    }
}

fn copy_host(host: &ParsedHost) -> ParsedHost {
    ParsedHost {
        url: host.url.clone(),
        password: host.password.clone(),
        label: host.label.clone(),
    }
}

fn retryable(error: ClientError) -> bool {
    matches!(
        error,
        ClientError::Closed | ClientError::Transport | ClientError::Timeout
    )
}

fn reconnect_delay(attempt: u32) -> u64 {
    1_u64.checked_shl(attempt).unwrap_or(30).min(30)
}

fn client_identity() -> Result<String, &'static str> {
    if let Some(id) = Storage::get_item(CLIENT_ID_KEY).filter(|id| !id.trim().is_empty()) {
        return Ok(id);
    }
    let id = uuid::Uuid::new_v4().to_string();
    Storage::set_item(CLIENT_ID_KEY, &id)
        .map_err(|_| "无法保存客户端身份，请清理应用存储空间后重试（identity_storage_full）")?;
    Ok(id)
}

pub(crate) fn screen() -> impl IntoElement {
    let state = Connection::new();
    let address = signal(String::new());
    let busy = state.busy;
    let connected = state.connected;
    let status = state.status;
    let projects = state.projects;
    let projects_loaded = state.projects_loaded;
    let reconnecting = state.reconnecting;
    let routes = state.routes;
    let check = move || {
        if busy.get_untracked() || connected.get_untracked() {
            return;
        }
        status.set(match parse_direct_host(&address.get_untracked()) {
            Ok(host)
                if host
                    .password
                    .as_deref()
                    .is_some_and(|value| !value.is_empty()) =>
            {
                format!("地址有效（已提供密码）：{}", host.label)
            }
            Ok(host) => format!("地址有效：{}", host.label),
            Err(error) => error,
        });
    };
    let connect_state = state.clone();
    let connect = move || {
        if busy.get_untracked() || connected.get_untracked() {
            return;
        }
        match parse_direct_host(&address.get_untracked()) {
            Ok(host) => connect_state.connect(host),
            Err(error) => status.set(error),
        }
    };
    let refresh_state = state.clone();
    let disconnect_state = state.clone();
    let row_state = state.clone();
    let page_state = state.clone();
    let conversation_state = state.clone();
    let conversations = state.conversations;
    nav_stack()
        .flex_1()
        .child(
            nav_page().title("Paseo").child(
                scroll()
                    .keyboard_taps(KeyboardTaps::Handled)
                    .flex_1()
                    .child(
                        view()
                            .p_4()
                            .gap_3()
                            .pb_safe_area_plus(px(16.0))
                            .bg(move || theme().background)
                            .child(text("连接开发主机").text_color(move || theme().foreground))
                            .child(
                                input()
                                    .label("主机地址")
                                    .placeholder("ws://localhost:6767/ws")
                                    .disabled(move || busy.get() || connected.get())
                                    .value(move || address.get())
                                    .on_change(move |value: &str| {
                                        address.set(value.to_owned());
                                        status.set(String::new());
                                    })
                                    .on_submit(move |_: &str| check())
                                    .p_3()
                                    .border_1()
                                    .border_color(move || theme().input)
                                    .text_color(move || theme().foreground),
                            )
                            .child(
                                button()
                                    .disabled(move || {
                                        busy.get()
                                            || connected.get()
                                            || address.with(|value| value.trim().is_empty())
                                    })
                                    .on_press(check)
                                    .p_3()
                                    .child(text("检查地址").text_color(move || theme().primary)),
                            )
                            .child(
                                button()
                                    .disabled(move || {
                                        busy.get()
                                            || connected.get()
                                            || address.with(|value| value.trim().is_empty())
                                    })
                                    .on_press(connect)
                                    .p_3()
                                    .child(text("连接主机").text_color(move || theme().primary)),
                            )
                            .child(
                                button()
                                    .disabled(move || busy.get() || !connected.get())
                                    .on_press(move || refresh_state.refresh())
                                    .p_3()
                                    .child(text("刷新项目").text_color(move || theme().primary)),
                            )
                            .child(
                                button()
                                    .disabled(move || !busy.get() && !connected.get())
                                    .on_press(move || disconnect_state.disconnect())
                                    .p_3()
                                    .child(
                                        text(move || {
                                            if reconnecting.get() {
                                                "取消重连"
                                            } else {
                                                "断开连接"
                                            }
                                        })
                                        .text_color(move || theme().primary),
                                    ),
                            )
                            .child(
                                text(move || status.get())
                                    .text_color(move || theme().muted_foreground),
                            )
                            .child(
                                text(move || {
                                    if projects_loaded.get()
                                        && !busy.get()
                                        && projects.with(Vec::is_empty)
                                    {
                                        "暂无项目"
                                    } else {
                                        ""
                                    }
                                })
                                .text_color(move || theme().foreground),
                            )
                            .child(each(
                                move || projects.get(),
                                |project: &Project| project.project_id.clone(),
                                move |project, _| {
                                    let state = row_state.clone();
                                    button()
                                        .disabled(move || busy.get())
                                        .on_press(move || {
                                            state.open_project(project.get_untracked())
                                        })
                                        .p_3()
                                        .child(
                                            text(move || {
                                                project.with(|project| {
                                                    project.project_display_name.clone()
                                                })
                                            })
                                            .text_color(move || theme().foreground),
                                        )
                                },
                            )),
                    ),
            ),
        )
        .child(each(
            move || routes.get(),
            |project: &Project| project.project_id.clone(),
            move |selected, _| {
                let generation = page_state.generation.get();
                let client = page_state
                    .client
                    .borrow()
                    .clone()
                    .expect("project route requires a live connection");
                let disconnect = page_state.clone();
                let dismissed = page_state.clone();
                let opener = page_state.clone();
                project::page(
                    client,
                    selected.get_untracked(),
                    move || routes.set(Vec::new()),
                    move || disconnect.disconnect(),
                    Rc::new(move |id| {
                        opener.remembered_agent.replace(Some(id.clone()));
                        conversations.set(vec![id]);
                    }),
                    page_state.remembered_agent.borrow().clone(),
                )
                .on_dismiss(move || {
                    if dismissed.current(generation) {
                        routes.set(Vec::new());
                    }
                })
            },
        ))
        .child(each(
            move || conversations.get(),
            |id: &String| id.clone(),
            move |id, _| {
                let generation = conversation_state.generation.get();
                let client = conversation_state
                    .client
                    .borrow()
                    .clone()
                    .expect("conversation route requires a live connection");
                let dismissed = conversation_state.clone();
                let id = id.get_untracked();
                let memory = conversation_state
                    .viewed
                    .borrow_mut()
                    .get(&client.server_info().server_id, &id);
                conversation::page(client, id, memory, move || conversations.set(Vec::new()))
                    .on_dismiss(move || {
                        if dismissed.current(generation) {
                            conversations.set(Vec::new());
                        }
                    })
            },
        ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use nsh_core::element::{Runtime, create_root};

    #[test]
    fn recovery_backoff_caps_at_thirty_and_does_not_retry_auth_or_protocol_errors() {
        assert_eq!(
            (0..8).map(reconnect_delay).collect::<Vec<_>>(),
            [1, 2, 4, 8, 16, 30, 30, 30]
        );
        assert_eq!(reconnect_delay(u32::MAX), 30);
        for error in [
            ClientError::Closed,
            ClientError::Transport,
            ClientError::Timeout,
        ] {
            assert!(retryable(error));
        }
        for error in [
            ClientError::PasswordRequired,
            ClientError::IncorrectPassword,
            ClientError::InvalidProtocol,
            ClientError::RequestRejected,
        ] {
            assert!(!retryable(error));
        }
    }

    #[test]
    fn authentication_failure_discards_saved_routes_and_credentials_without_retrying() {
        let runtime = Runtime::default();
        let _entered = runtime.enter();
        let (owner, state) = create_root(Connection::new);
        let generation = state.next_generation();
        state.server_id.replace(Some("same-server".into()));
        state.reconnect_host.replace(Some(
            parse_direct_host("ws://localhost:6767?password=private").unwrap(),
        ));
        state.saved_routes.replace(Some(SavedRoutes {
            projects: Vec::new(),
            agents: vec!["agent".into()],
        }));
        state.failed(generation, ClientError::IncorrectPassword);
        assert!(state.task.borrow().is_none());
        assert!(state.saved_routes.borrow().is_none());
        assert!(state.reconnect_host.borrow().is_none());
        assert!(!state.reconnecting.get_untracked());
        assert!(!state.busy.get_untracked());
        assert!(!state.status.get_untracked().contains("private"));
        owner.dispose();
    }

    #[test]
    fn an_old_connection_callback_cannot_clear_a_new_connection() {
        let runtime = Runtime::default();
        let _entered = runtime.enter();
        let (owner, state) = create_root(Connection::new);
        let old = state.next_generation();
        state.next_generation();
        state.connected.set(true);
        state.status.set("已连接：new-host".to_owned());
        let capture: serde_json::Value = serde_json::from_str(include_str!(
            "../../../protocol-fixtures/direct-session.json"
        ))
        .unwrap();
        let project: Project = serde_json::from_value(
            capture[5]["frame"]["message"]["payload"]["projects"][0].clone(),
        )
        .unwrap();
        state.projects.set(vec![project]);
        state.failed(old, ClientError::Closed);
        assert!(state.connected.get_untracked());
        assert_eq!(state.status.get_untracked(), "已连接：new-host");
        assert_eq!(
            state.projects.get_untracked()[0].project_id,
            "fixture-project"
        );
        owner.dispose();
    }

    #[test]
    fn disconnect_closes_project_pages_before_releasing_the_connection() {
        let runtime = Runtime::default();
        let _entered = runtime.enter();
        let (owner, state) = create_root(Connection::new);
        let capture: serde_json::Value = serde_json::from_str(include_str!(
            "../../../protocol-fixtures/direct-session.json"
        ))
        .unwrap();
        let project: Project = serde_json::from_value(
            capture[5]["frame"]["message"]["payload"]["projects"][0].clone(),
        )
        .unwrap();
        state.connected.set(true);
        state.open_project(project);
        assert_eq!(state.routes.get_untracked().len(), 1);
        state.disconnect();
        assert!(state.routes.get_untracked().is_empty());
        assert!(!state.connected.get_untracked());
        owner.dispose();
    }
}
