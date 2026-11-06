use crate::{
    client::{Client, ClientError},
    protocol::{
        Project,
        catalog::{
            AgentEntry, AgentStatus, Feature, Field, PageInfo, Provider, ProviderStatus, Workspace,
            WorkspaceStatus,
        },
    },
};
use nsh_base::{button, nav_page, theme};
use nsh_core::{element::Screen, prelude::*};
use std::{cell::RefCell, future::Future, rc::Rc};

#[path = "create_agent.rs"]
mod create_agent;

pub(super) struct RequestTask {
    owner: Owner,
    task: RefCell<Option<Spawned<()>>>,
}

impl RequestTask {
    pub(super) fn new() -> Rc<Self> {
        let task = Rc::new(Self {
            owner: Owner::current(),
            task: RefCell::new(None),
        });
        let cleanup_task = task.clone();
        cleanup(move || {
            // Spawned's drop cancels the RPC future and removes its pending request slot.
            cleanup_task.task.take();
        });
        task
    }

    pub(super) fn start(&self, work: impl Future<Output = ()> + 'static) {
        self.task.take();
        let task = self.owner.run(|| spawn(work));
        self.task.replace(Some(task));
    }
}

fn error_text(error: ClientError) -> String {
    format!("{error}（{}）", error.code())
}

struct Directory<T> {
    request: Rc<RequestTask>,
    rows: Signal<Vec<Rc<T>>>,
    cursor: Signal<Option<String>>,
    busy: Signal<bool>,
    status: Signal<String>,
    label: &'static str,
}

impl<T: PartialEq + 'static> Directory<T> {
    fn new(label: &'static str) -> Rc<Self> {
        Rc::new(Self {
            request: RequestTask::new(),
            rows: signal(Vec::new()),
            cursor: signal(None),
            busy: signal(false),
            status: signal(String::new()),
            label,
        })
    }

    fn begin(&self, more: bool) -> Option<Option<String>> {
        if self.busy.get_untracked() {
            return None;
        }
        let cursor = if more {
            Some(self.cursor.get_untracked()?)
        } else {
            None
        };
        if !more {
            self.rows.set(Vec::new());
            self.cursor.set(None);
        }
        self.busy.set(true);
        self.status.set(format!("正在加载{}…", self.label));
        Some(cursor)
    }

    fn finish(
        &self,
        entries: Vec<T>,
        info: PageInfo,
        requested: Option<&str>,
        key: impl Fn(&T) -> &str,
    ) {
        self.rows.update(|rows| append_unique(rows, entries, key));
        self.busy.set(false);
        let cursor = if info.has_more {
            match info.next_cursor {
                Some(cursor) if !cursor.is_empty() && Some(cursor.as_str()) != requested => {
                    Some(cursor)
                }
                _ => {
                    self.cursor.set(None);
                    self.status.set(format!(
                        "分页响应缺少有效游标，请刷新{}（invalid_pagination）",
                        self.label
                    ));
                    return;
                }
            }
        } else {
            None
        };
        self.cursor.set(cursor);
        let count = self.rows.with(Vec::len);
        self.status.set(if count == 0 {
            format!("暂无{}", self.label)
        } else {
            format!("已加载 {count} 个{}", self.label)
        });
    }

    fn fail(&self, error: ClientError) {
        self.busy.set(false);
        self.status.set(error_text(error));
    }
}

fn append_unique<T>(rows: &mut Vec<Rc<T>>, entries: Vec<T>, key: impl Fn(&T) -> &str) {
    for entry in entries {
        if let Some(index) = rows.iter().position(|row| key(row) == key(&entry)) {
            rows[index] = Rc::new(entry);
        } else {
            rows.push(Rc::new(entry));
        }
    }
}

struct ProjectPage {
    client: Rc<Client>,
    project: Project,
    workspaces: Rc<Directory<Workspace>>,
    agents: Rc<Directory<AgentEntry>>,
    selected: Signal<Vec<Rc<Workspace>>>,
}

impl ProjectPage {
    fn load_workspaces(self: &Rc<Self>, more: bool) {
        let Some(cursor) = self.workspaces.begin(more) else {
            return;
        };
        if !more {
            self.selected.set(Vec::new());
        }
        let state = self.clone();
        self.workspaces.request.start(async move {
            match state
                .client
                .workspaces(&state.project.project_id, cursor.as_deref())
                .await
            {
                Ok(page) => state.workspaces.finish(
                    page.entries,
                    page.page_info,
                    cursor.as_deref(),
                    |workspace| &workspace.id,
                ),
                Err(error) => state.workspaces.fail(error),
            }
        });
    }

    fn load_agents(self: &Rc<Self>, more: bool) {
        let Some(cursor) = self.agents.begin(more) else {
            return;
        };
        let state = self.clone();
        self.agents.request.start(async move {
            match state
                .client
                .agents(&state.project.project_id, cursor.as_deref())
                .await
            {
                Ok(page) => {
                    state
                        .agents
                        .finish(page.entries, page.page_info, cursor.as_deref(), |entry| {
                            &entry.agent.id
                        })
                }
                Err(error) => state.agents.fail(error),
            }
        });
    }
}

fn directory_controls<T: 'static>(
    directory: Rc<Directory<T>>,
    refresh: impl Fn() + 'static,
    more: impl Fn() + 'static,
) -> impl IntoElement {
    let busy = directory.busy;
    let cursor = directory.cursor;
    let status = directory.status;
    view()
        .gap_2()
        .child(text(directory.label).text_color(move || theme().foreground))
        .child(text(move || status.get()).text_color(move || theme().muted_foreground))
        .child(
            view()
                .flex_row()
                .gap_2()
                .child(
                    button()
                        .disabled(move || busy.get())
                        .on_press(refresh)
                        .p_3()
                        .child(text(format!("刷新{}", directory.label))),
                )
                .child(
                    button()
                        .disabled(move || busy.get() || cursor.with(Option::is_none))
                        .on_press(more)
                        .p_3()
                        .child(text(format!("加载更多{}", directory.label))),
                ),
        )
}

pub(super) fn page(
    client: Rc<Client>,
    project: Project,
    back: impl Fn() + 'static,
    disconnect: impl Fn() + 'static,
    open_agent: Rc<dyn Fn(String)>,
) -> Screen {
    let title = project.project_display_name.clone();
    let state = Rc::new(ProjectPage {
        client,
        project,
        workspaces: Directory::new("工作区"),
        agents: Directory::new("Agent"),
        selected: signal(Vec::new()),
    });
    state.load_workspaces(false);
    state.load_agents(false);
    let workspaces = state.workspaces.rows;
    let agents = state.agents.rows;
    let selected = state.selected;
    let refresh_workspaces = state.clone();
    let more_workspaces = state.clone();
    let refresh_agents = state.clone();
    let more_agents = state.clone();
    let agent_client = state.client.clone();
    let provider_client = state.client.clone();
    nav_page().title(title.clone()).child(
        scroll()
            .keyboard_taps(KeyboardTaps::Handled)
            .flex_1()
            .child(
                view()
                    .p_4()
                    .gap_3()
                    .pb_safe_area_plus(px(16.0))
                    .bg(move || theme().background)
                    .child(text(format!("项目：{title}")).text_color(move || theme().foreground))
                    .child(
                        text(state.project.project_root_path.clone())
                            .text_color(move || theme().muted_foreground),
                    )
                    .child(
                        view()
                            .flex_row()
                            .gap_2()
                            .child(button().on_press(back).p_3().child(text("返回项目列表")))
                            .child(button().on_press(disconnect).p_3().child(text("断开连接"))),
                    )
                    .child(directory_controls(
                        state.workspaces.clone(),
                        move || refresh_workspaces.load_workspaces(false),
                        move || more_workspaces.load_workspaces(true),
                    ))
                    .child(each(
                        move || workspaces.get(),
                        |workspace: &Rc<Workspace>| workspace.id.clone(),
                        move |workspace, _| {
                            button()
                                .on_press(move || selected.set(vec![workspace.get_untracked()]))
                                .p_3()
                                .child(
                                    text(move || {
                                        workspace.with(|workspace| {
                                            format!(
                                                "工作区：{} · {}",
                                                workspace.name,
                                                workspace_status(&workspace.status)
                                            )
                                        })
                                    })
                                    .text_color(move || theme().foreground),
                                )
                                .child(
                                    text(move || {
                                        workspace.with(|workspace| workspace.directory().to_owned())
                                    })
                                    .text_color(move || theme().muted_foreground),
                                )
                        },
                    ))
                    // Selection creates a new owner for this exact directory, cancelling old provider/feature requests.
                    .child(each(
                        move || selected.get(),
                        |workspace: &Rc<Workspace>| {
                            format!("{}:{}", workspace.id, workspace.directory())
                        },
                        move |workspace, _| {
                            provider_panel(provider_client.clone(), workspace.get_untracked())
                        },
                    ))
                    .child(directory_controls(
                        state.agents.clone(),
                        move || refresh_agents.load_agents(false),
                        move || more_agents.load_agents(true),
                    ))
                    .child(each(
                        move || agents.get(),
                        |entry: &Rc<AgentEntry>| entry.agent.id.clone(),
                        move |entry, _| agent_row(agent_client.clone(), entry, open_agent.clone()),
                    )),
            ),
    )
}

fn workspace_status(status: &WorkspaceStatus) -> &'static str {
    match status {
        WorkspaceStatus::NeedsInput => "等待输入",
        WorkspaceStatus::Failed => "失败",
        WorkspaceStatus::Running => "运行中",
        WorkspaceStatus::Attention => "需要查看",
        WorkspaceStatus::Done => "空闲",
    }
}

fn agent_status(status: &AgentStatus) -> &'static str {
    match status {
        AgentStatus::Initializing => "初始化中",
        AgentStatus::Idle => "空闲",
        AgentStatus::Running => "运行中",
        AgentStatus::Error => "错误",
        AgentStatus::Closed => "已关闭",
    }
}

fn agent_row(
    client: Rc<Client>,
    entry: ReadSignal<Rc<AgentEntry>>,
    open_agent: Rc<dyn Fn(String)>,
) -> impl IntoElement {
    let open = signal(Vec::<String>::new());
    let detail_client = client.clone();
    view()
        .gap_2()
        .child(
            button()
                .on_press(move || {
                    open.set(if open.with(Vec::is_empty) {
                        vec![entry.with_untracked(|entry| entry.agent.id.clone())]
                    } else {
                        Vec::new()
                    })
                })
                .p_3()
                .child(
                    text(move || {
                        entry.with(|entry| {
                            format!(
                                "Agent：{} · {}",
                                entry.agent.title.as_deref().unwrap_or(&entry.agent.id),
                                agent_status(&entry.agent.status)
                            )
                        })
                    })
                    .text_color(move || theme().foreground),
                ),
        )
        .child(each(
            move || open.get(),
            |id: &String| id.clone(),
            move |id, _| {
                agent_detail(
                    detail_client.clone(),
                    id.get_untracked(),
                    open_agent.clone(),
                )
            },
        ))
}

struct Detail {
    task: Rc<RequestTask>,
    client: Rc<Client>,
    id: String,
    text: Signal<String>,
    busy: Signal<bool>,
}

impl Detail {
    fn load(self: &Rc<Self>) {
        self.busy.set(true);
        self.text.set("正在读取 Agent 详情…".to_owned());
        let state = self.clone();
        self.task.start(async move {
            state.text.set(match state.client.agent(&state.id).await {
                Ok(result) => match (result.agent, result.error) {
                    (_, Some(error)) => format!("读取 Agent 失败：{error}（agent_error）"),
                    (Some(agent), None) => format!(
                        "Agent ID：{}\n服务商：{}\n模型：{}\n模式：{}\n状态：{}\n工作目录：{}",
                        agent.id,
                        agent.provider,
                        agent.model.as_deref().unwrap_or("未指定"),
                        agent.current_mode_id.as_deref().unwrap_or("未指定"),
                        agent_status(&agent.status),
                        agent.cwd
                    ),
                    (None, None) => "Agent 不存在或已删除".to_owned(),
                },
                Err(error) => error_text(error),
            });
            state.busy.set(false);
        });
    }
}

fn agent_detail(
    client: Rc<Client>,
    id: String,
    open_agent: Rc<dyn Fn(String)>,
) -> impl IntoElement {
    let open_id = id.clone();
    let state = Rc::new(Detail {
        task: RequestTask::new(),
        client,
        id,
        text: signal(String::new()),
        busy: signal(false),
    });
    state.load();
    let text_value = state.text;
    let busy = state.busy;
    view()
        .gap_2()
        .child(
            button()
                .on_press(move || open_agent(open_id.clone()))
                .p_3()
                .child(text("打开会话")),
        )
        .child(text(move || text_value.get()).text_color(move || theme().foreground))
        .child(
            button()
                .disabled(move || busy.get())
                .on_press(move || state.load())
                .p_3()
                .child(text("刷新 Agent 详情")),
        )
}

struct ProviderPanel {
    task: Rc<RequestTask>,
    client: Rc<Client>,
    cwd: String,
    rows: Signal<Vec<Rc<Provider>>>,
    status: Signal<String>,
    busy: Signal<bool>,
}

impl ProviderPanel {
    fn load(self: &Rc<Self>) {
        self.rows.set(Vec::new());
        self.busy.set(true);
        self.status.set("正在读取服务商…".to_owned());
        let state = self.clone();
        self.task.start(async move {
            match state.client.providers(&state.cwd).await {
                Ok(result) => {
                    state.status.set(
                        if result.entries.is_empty() {
                            "暂无服务商"
                        } else {
                            "请选择服务商查看模型、模式与功能"
                        }
                        .to_owned(),
                    );
                    state
                        .rows
                        .set(result.entries.into_iter().map(Rc::new).collect());
                }
                Err(error) => state.status.set(error_text(error)),
            }
            state.busy.set(false);
        });
    }
}

fn provider_panel(client: Rc<Client>, workspace: Rc<Workspace>) -> impl IntoElement {
    let workspace_id = workspace.id.clone();
    let state = Rc::new(ProviderPanel {
        task: RequestTask::new(),
        client,
        cwd: workspace.directory().to_owned(),
        rows: signal(Vec::new()),
        status: signal(String::new()),
        busy: signal(false),
    });
    state.load();
    let rows = state.rows;
    let status = state.status;
    let busy = state.busy;
    let row_state = state.clone();
    view()
        .gap_2()
        .child(text(format!("服务商目录：{}", state.cwd)).text_color(move || theme().foreground))
        .child(text(move || status.get()).text_color(move || theme().muted_foreground))
        .child(
            button()
                .disabled(move || busy.get())
                .on_press(move || state.load())
                .p_3()
                .child(text("刷新服务商")),
        )
        .child(each(
            move || rows.get(),
            |provider: &Rc<Provider>| provider.provider.clone(),
            move |provider, _| {
                provider_row(
                    row_state.client.clone(),
                    row_state.cwd.clone(),
                    workspace_id.clone(),
                    provider,
                )
            },
        ))
}

fn provider_status(provider: &Provider) -> &'static str {
    if !provider.enabled {
        return "已禁用";
    }
    match &provider.status {
        ProviderStatus::Ready => "可用",
        ProviderStatus::Loading => "加载中",
        ProviderStatus::Error => "错误",
        ProviderStatus::Unavailable => "不可用",
    }
}

fn provider_row(
    client: Rc<Client>,
    cwd: String,
    workspace_id: String,
    provider: ReadSignal<Rc<Provider>>,
) -> impl IntoElement {
    let open = signal(Vec::<String>::new());
    view()
        .gap_2()
        .child(
            button()
                .on_press(move || {
                    open.set(if open.with(Vec::is_empty) {
                        vec![provider.with_untracked(|provider| provider.provider.clone())]
                    } else {
                        Vec::new()
                    })
                })
                .p_3()
                .child(
                    text(move || {
                        provider.with(|provider| {
                            format!(
                                "服务商：{} · {}",
                                provider.label.as_deref().unwrap_or(&provider.provider),
                                provider_status(provider)
                            )
                        })
                    })
                    .text_color(move || theme().foreground),
                ),
        )
        .child(
            text(move || provider.with(|provider| provider.error.clone().unwrap_or_default()))
                .text_color(move || theme().muted_foreground),
        )
        .child(each(
            move || open.get(),
            |id: &String| id.clone(),
            move |_, _| {
                let provider = provider.get_untracked();
                view()
                    .gap_2()
                    .child(text(provider_catalog(&provider)).text_color(move || theme().foreground))
                    .child(feature_panel(client.clone(), cwd.clone(), provider.clone()))
                    .child(create_agent::form(
                        client.clone(),
                        cwd.clone(),
                        workspace_id.clone(),
                        provider,
                    ))
            },
        ))
}

fn provider_catalog(provider: &Provider) -> String {
    let models = match &provider.models {
        Some(models) if models.is_empty() => "暂无模型".to_owned(),
        Some(models) => models
            .iter()
            .map(|model| format!("{}（{}）", model.label, model.id))
            .collect::<Vec<_>>()
            .join("\n"),
        None => "模型尚未加载，请刷新服务商".to_owned(),
    };
    let modes = match &provider.modes {
        Some(modes) if modes.is_empty() => "暂无模式".to_owned(),
        Some(modes) => modes
            .iter()
            .map(|mode| format!("{}（{}）", mode.label, mode.id))
            .collect::<Vec<_>>()
            .join("\n"),
        None => "模式尚未加载，请刷新服务商".to_owned(),
    };
    format!(
        "服务商 ID：{}\n模型\n{models}\n模式\n{modes}",
        provider.provider
    )
}

struct Features {
    task: Rc<RequestTask>,
    client: Rc<Client>,
    cwd: String,
    provider: Rc<Provider>,
    text: Signal<String>,
    busy: Signal<bool>,
}

impl Features {
    fn load(self: &Rc<Self>) {
        if !self.provider.enabled {
            self.text.set("服务商已禁用".to_owned());
            return;
        }
        self.busy.set(true);
        self.text.set("正在读取服务商功能…".to_owned());
        let state = self.clone();
        self.task.start(async move {
            state.text.set(
                match state
                    .client
                    .provider_features(&state.provider.provider, &state.cwd)
                    .await
                {
                    Ok(result) => match result.error {
                        Field::Value(error) => {
                            format!("服务商功能读取失败：{error}（provider_error）")
                        }
                        Field::Missing | Field::Null => match result.features {
                            Some(features) if features.is_empty() => "暂无可配置功能".to_owned(),
                            Some(features) => features
                                .iter()
                                .map(feature_text)
                                .collect::<Vec<_>>()
                                .join("\n"),
                            None => "服务商未返回功能目录，请重试".to_owned(),
                        },
                    },
                    Err(error) => error_text(error),
                },
            );
            state.busy.set(false);
        });
    }
}

fn feature_text(feature: &Feature) -> String {
    match feature {
        Feature::Toggle { label, value, .. } => {
            format!("{label}：{}", if *value { "开启" } else { "关闭" })
        }
        Feature::Select {
            label,
            value,
            options,
            ..
        } => format!(
            "{label}：{}\n{}",
            value.as_deref().unwrap_or("未选择"),
            options
                .iter()
                .map(|option| format!("{}（{}）", option.label, option.id))
                .collect::<Vec<_>>()
                .join("、")
        ),
    }
}

fn feature_panel(client: Rc<Client>, cwd: String, provider: Rc<Provider>) -> impl IntoElement {
    let state = Rc::new(Features {
        task: RequestTask::new(),
        client,
        cwd,
        provider,
        text: signal(String::new()),
        busy: signal(false),
    });
    state.load();
    let text_value = state.text;
    let busy = state.busy;
    let enabled = state.provider.enabled;
    view()
        .gap_2()
        .child(text(move || text_value.get()).text_color(move || theme().foreground))
        .child(
            button()
                .disabled(move || busy.get() || !enabled)
                .on_press(move || state.load())
                .p_3()
                .child(text("刷新服务商功能")),
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use nsh_core::element::{Runtime, create_root};

    #[test]
    fn pagination_keeps_all_pages_without_duplicate_rows_and_reports_broken_cursors() {
        let runtime = Runtime::default();
        let _entered = runtime.enter();
        let (owner, directory) = create_root(|| Directory::<String>::new("工作区"));
        assert_eq!(directory.begin(false), Some(None));
        directory.finish(
            vec!["a".to_owned(), "b".to_owned()],
            PageInfo {
                next_cursor: Some("next".to_owned()),
                prev_cursor: None,
                has_more: true,
            },
            None,
            String::as_str,
        );
        assert_eq!(directory.begin(true), Some(Some("next".to_owned())));
        directory.finish(
            vec!["b".to_owned(), "c".to_owned()],
            PageInfo {
                next_cursor: None,
                prev_cursor: None,
                has_more: false,
            },
            Some("next"),
            String::as_str,
        );
        assert_eq!(
            directory.rows.with(|rows| rows
                .iter()
                .map(|row| row.as_str())
                .collect::<Vec<_>>()
                .join(",")),
            "a,b,c"
        );
        assert_eq!(directory.begin(true), None);
        directory.finish(
            Vec::new(),
            PageInfo {
                next_cursor: None,
                prev_cursor: None,
                has_more: true,
            },
            None,
            String::as_str,
        );
        assert!(
            directory
                .status
                .get_untracked()
                .contains("invalid_pagination")
        );
        assert_eq!(directory.rows.with(Vec::len), 3);
        owner.dispose();
    }

    #[test]
    fn a_failed_page_retains_its_cursor_for_retry() {
        let runtime = Runtime::default();
        let _entered = runtime.enter();
        let (owner, directory) = create_root(|| Directory::<String>::new("Agent"));
        directory.finish(
            vec!["a".to_owned()],
            PageInfo {
                next_cursor: Some("next".to_owned()),
                prev_cursor: None,
                has_more: true,
            },
            None,
            String::as_str,
        );
        assert_eq!(directory.begin(true), Some(Some("next".to_owned())));
        directory.fail(ClientError::Timeout);
        assert_eq!(directory.begin(true), Some(Some("next".to_owned())));
        assert_eq!(directory.rows.with(Vec::len), 1);
        owner.dispose();
    }

    #[test]
    fn workspace_context_uses_its_own_directory_instead_of_the_project_root() {
        let capture: serde_json::Value =
            serde_json::from_str(include_str!("../../../protocol-fixtures/catalog.json")).unwrap();
        let mut row = capture
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["frame"]["message"]["type"] == "fetch_workspaces_response")
            .unwrap()["frame"]["message"]["payload"]["entries"][0]
            .clone();
        row["workspaceDirectory"] = serde_json::json!("/fixture/worktree");
        let workspace: Workspace = serde_json::from_value(row).unwrap();
        assert_ne!(workspace.directory(), workspace.project_root_path);
        assert_eq!(workspace.directory(), "/fixture/worktree");
    }
}
