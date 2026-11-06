use crate::{
    client::{Client, ClientError},
    host::{ParsedHost, parse_direct_host},
    protocol::ProjectList,
};
use nsh_base::{button, input, nav_page, nav_stack, theme};
use nsh_core::{prelude::*, storage::Storage};
use std::{cell::RefCell, rc::Rc};

const CLIENT_ID_KEY: &str = "paseo.nsh.client-id";

struct Connection {
    owner: Owner,
    client: RefCell<Option<Client>>,
    task: RefCell<Option<Spawned<()>>>,
    busy: Signal<bool>,
    connected: Signal<bool>,
    status: Signal<String>,
    projects: Signal<String>,
    hostname: Signal<String>,
}

impl Connection {
    fn new() -> Rc<Self> {
        let state = Rc::new(Self {
            owner: Owner::current(),
            client: RefCell::new(None),
            task: RefCell::new(None),
            busy: signal(false),
            connected: signal(false),
            status: signal(String::new()),
            projects: signal(String::new()),
            hostname: signal(String::new()),
        });
        let cleanup_state = state.clone();
        cleanup(move || {
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
        self.projects.set(String::new());
        self.status.set("正在连接主机…".to_owned());
        let state = self.clone();
        let task = self.owner.run(|| {
            spawn(async move {
                let label = host.label.clone();
                let mut client = match Client::connect(host, &client_id).await {
                    Ok(client) => client,
                    Err(error) => {
                        state.failed(error);
                        return;
                    }
                };
                state
                    .hostname
                    .set(client.server_info().hostname.clone().unwrap_or(label));
                state.status.set("已连接，正在读取项目…".to_owned());
                match client.projects(&uuid::Uuid::new_v4().to_string()).await {
                    Ok(projects) => {
                        state.client.replace(Some(client));
                        state.loaded(projects);
                    }
                    Err(error) => state.failed(error),
                }
            })
        });
        self.task.replace(Some(task));
    }

    fn refresh(self: &Rc<Self>) {
        if self.busy.get_untracked() {
            return;
        }
        let Some(mut client) = self.client.take() else {
            return;
        };
        self.busy.set(true);
        self.status.set("正在读取项目…".to_owned());
        let state = self.clone();
        let task = self.owner.run(|| {
            spawn(async move {
                match client.projects(&uuid::Uuid::new_v4().to_string()).await {
                    Ok(projects) => {
                        state.client.replace(Some(client));
                        state.loaded(projects);
                    }
                    Err(error) => state.failed(error),
                }
            })
        });
        self.task.replace(Some(task));
    }

    fn loaded(&self, projects: ProjectList) {
        let names: Vec<String> = projects
            .projects
            .into_iter()
            .map(|project| project.project_display_name)
            .collect();
        self.projects.set(if names.is_empty() {
            "暂无项目".to_owned()
        } else {
            names.join("\n")
        });
        self.connected.set(true);
        self.busy.set(false);
        self.status
            .set(format!("已连接：{}", self.hostname.get_untracked()));
    }

    fn failed(&self, error: ClientError) {
        self.client.take();
        self.connected.set(false);
        self.busy.set(false);
        self.projects.set(String::new());
        self.status.set(format!("{error}（{}）", error.code()));
    }

    fn disconnect(&self) {
        if let Some(task) = self.task.take() {
            task.cancel();
        }
        self.client.take();
        self.connected.set(false);
        self.busy.set(false);
        self.projects.set(String::new());
        self.hostname.set(String::new());
        self.status.set("已断开连接".to_owned());
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
    let check = move || {
        if busy.get_untracked() || connected.get_untracked() {
            return;
        }
        status.set(match parse_direct_host(&address.get_untracked()) {
            Ok(host) if host.password.is_some() => {
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
    nav_stack().flex_1().child(
        nav_page().title("Paseo").child(
            view()
                .flex_1()
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
                        .on_press(move || state.disconnect())
                        .p_3()
                        .child(text("断开连接").text_color(move || theme().primary)),
                )
                .child(text(move || status.get()).text_color(move || theme().muted_foreground))
                .child(text(move || projects.get()).text_color(move || theme().foreground)),
        ),
    )
}
