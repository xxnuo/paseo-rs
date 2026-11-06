use crate::{
    client::{Client, ClientError},
    host::{ParsedHost, parse_direct_host},
    protocol::{Project, ProjectList},
};
#[path = "project.rs"]
mod project;
use nsh_base::{button, input, nav_page, nav_stack, theme};
use nsh_core::{prelude::*, storage::Storage};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};

const CLIENT_ID_KEY: &str = "paseo.nsh.client-id";

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
    hostname: Signal<String>,
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
            hostname: signal(String::new()),
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
        });
        state
    }

    fn connect(self: &Rc<Self>, host: ParsedHost) {
        if self.busy.get_untracked() || self.connected.get_untracked() {
            return;
        }
        let client_id = match client_identity() {
            Ok(id) => id,
            Err(message) => {
                self.status.set(message.to_owned());
                return;
            }
        };
        self.busy.set(true);
        let generation = self.next_generation();
        self.projects.set(Vec::new());
        self.projects_loaded.set(false);
        self.status.set("正在连接主机…".to_owned());
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

    fn projects_finished(&self, generation: u64, result: Result<ProjectList, ClientError>) {
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
        self.projects.set(projects.projects);
        self.projects_loaded.set(true);
        self.connected.set(true);
        self.busy.set(false);
        self.status
            .set(format!("已连接：{}", self.hostname.get_untracked()));
    }

    fn failed(&self, generation: u64, error: ClientError) {
        if !self.current(generation) {
            return;
        }
        self.next_generation();
        if let Some(task) = self.task.take() {
            task.cancel();
        }
        self.routes.set(Vec::new());
        self.client.take();
        self.connected.set(false);
        self.busy.set(false);
        self.projects.set(Vec::new());
        self.projects_loaded.set(false);
        self.status.set(format!("{error}（{}）", error.code()));
    }

    fn disconnect(&self) {
        self.next_generation();
        if let Some(task) = self.task.take() {
            task.cancel();
        }
        self.routes.set(Vec::new());
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
            self.routes.set(vec![project]);
        }
    }
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
    nav_stack()
        .flex_1()
        .child(
            nav_page().title("Paseo").child(
                scroll().flex_1().child(
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
                                .child(text("断开连接").text_color(move || theme().primary)),
                        )
                        .child(
                            text(move || status.get()).text_color(move || theme().muted_foreground),
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
                                    .on_press(move || state.open_project(project.get_untracked()))
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
                let client = page_state
                    .client
                    .borrow()
                    .clone()
                    .expect("project route requires a live connection");
                let disconnect = page_state.clone();
                project::page(
                    client,
                    selected.get_untracked(),
                    move || routes.set(Vec::new()),
                    move || disconnect.disconnect(),
                )
                .on_dismiss(move || routes.set(Vec::new()))
            },
        ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use nsh_core::element::{Runtime, create_root};

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
