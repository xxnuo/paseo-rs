//! Workspace-scoped creation; an uncertain request keeps its identity across page lifetimes.

use super::{Field, Provider, ProviderStatus, RequestTask};
use crate::client::Client;
use crate::protocol::conversation::{
    AgentConfig, Created, CreationKind, CreationPhase, CreationSnapshot,
};
use nsh_base::{button, input, theme};
use nsh_core::{prelude::*, storage::Storage};
use std::{cell::RefCell, rc::Rc};

#[derive(serde::Serialize, serde::Deserialize)]
struct PendingCreation {
    key: String,
    provider: String,
    workspace_id: String,
    cwd: String,
    title: Option<String>,
    model: Option<String>,
    mode: Option<String>,
}

impl PendingCreation {
    fn config(&self) -> AgentConfig {
        AgentConfig {
            provider: self.provider.clone(),
            cwd: self.cwd.clone(),
            model: self.model.clone(),
            mode_id: self.mode.clone(),
            title: self
                .title
                .clone()
                .map(Field::Value)
                .unwrap_or(Field::Missing),
            ..Default::default()
        }
    }
}

struct CreationForm {
    client: Rc<Client>,
    task: Rc<RequestTask>,
    provider: Rc<Provider>,
    workspace_id: String,
    cwd: String,
    storage_key: String,
    pending: RefCell<Option<PendingCreation>>,
    pending_key: Signal<Option<String>>,
    title: Signal<String>,
    model: Signal<Option<String>>,
    mode: Signal<Option<String>>,
    busy: Signal<bool>,
    status: Signal<String>,
}

impl CreationForm {
    fn blocked(&self) -> bool {
        self.busy.get() || self.pending_key.get().is_some()
    }

    fn create(self: &Rc<Self>) {
        if self.blocked() || !self.provider.enabled || self.provider.status != ProviderStatus::Ready
        {
            return;
        }
        let title = self.title.get_untracked();
        let record = PendingCreation {
            key: uuid::Uuid::new_v4().to_string(),
            provider: self.provider.provider.clone(),
            workspace_id: self.workspace_id.clone(),
            cwd: self.cwd.clone(),
            model: self.model.get_untracked(),
            mode: self.mode.get_untracked(),
            title: if title.trim().is_empty() {
                None
            } else {
                Some(title)
            },
        };
        let config = record.config();
        let key = record.key.clone();
        // Validate before persisting an identity that otherwise could never reach the daemon.
        if crate::protocol::conversation::create_agent(
            "validate",
            &config,
            Some(&self.workspace_id),
            Some(&key),
            Some(false),
        )
        .is_err()
        {
            self.status
                .set("Agent 名称无效，请使用不超过 200 个字符的名称".to_owned());
            return;
        }
        let Ok(saved) = serde_json::to_string(&record) else {
            return;
        };
        if Storage::set_item(&self.storage_key, saved).is_err() {
            self.status
                .set("无法保存创建标识，请检查存储后重试".to_owned());
            return;
        }
        self.pending.replace(Some(record));
        self.pending_key.set(Some(key.clone()));
        self.submit(config, key);
    }

    fn retry(self: &Rc<Self>) {
        if self.busy.get_untracked() {
            return;
        }
        let pending = self.pending.borrow();
        let Some(record) = pending.as_ref() else {
            return;
        };
        let config = record.config();
        let key = record.key.clone();
        drop(pending);
        self.submit(config, key);
    }

    fn submit(self: &Rc<Self>, config: AgentConfig, key: String) {
        self.busy.set(true);
        self.status.set("正在创建 Agent…".to_owned());
        let state = self.clone();
        self.task.start(async move {
            match state
                .client
                .create_agent(&config, Some(&state.workspace_id), Some(&key), Some(false))
                .await
            {
                Ok(result) => state.created(result),
                Err(error) => state.status.set(format!(
                    "创建结果尚未确认：{error}。请查询创建结果，避免重复创建"
                )),
            }
            state.busy.set(false);
        });
    }

    fn query(self: &Rc<Self>) {
        if self.busy.get_untracked() {
            return;
        }
        let Some(key) = self.pending_key.get_untracked() else {
            return;
        };
        self.busy.set(true);
        self.status.set("正在查询创建结果…".to_owned());
        let state = self.clone();
        self.task.start(async move {
            match state
                .client
                .subscribe_creation(CreationKind::Agent, &key, Some(false))
                .await
            {
                Ok(result) => match (result.snapshot, result.error) {
                    (_, Some(error)) => state.status.set(format!("查询创建结果失败：{error}")),
                    (Some(snapshot), None) => state.snapshot(snapshot),
                    (None, None) => state
                        .status
                        .set("主机尚无此创建记录；保留创建标识，请稍后再次查询".to_owned()),
                },
                Err(error) => state.status.set(format!("查询创建结果失败：{error}")),
            }
            state.busy.set(false);
        });
    }

    fn created(&self, result: Created) {
        if let Some(snapshot) = result.creation {
            self.snapshot(snapshot);
        } else if let Some(agent) = result.agent {
            self.finish(format!(
                "已创建 Agent：{}。刷新 Agent 列表即可查看",
                agent.agent.id
            ));
        } else if let Some(error) = result.error {
            // A daemon catch-all error can follow resource creation but precede its snapshot.
            self.status.set(format!(
                "创建结果尚未确认：{error}。请查询创建结果或重试同一次创建"
            ));
        } else {
            self.status
                .set("创建结果尚未确认，请查询创建结果".to_owned());
        }
    }

    fn snapshot(&self, snapshot: CreationSnapshot) {
        if snapshot.kind != CreationKind::Agent
            || self.pending_key.get_untracked().as_deref() != Some(&snapshot.idempotency_key)
        {
            self.status
                .set("创建响应与当前请求不匹配，请再次查询".to_owned());
            return;
        }
        if snapshot.outcome_unknown == Some(true) {
            self.status
                .set("创建结果尚未确认，请稍后再次查询".to_owned());
        } else if snapshot.phase == CreationPhase::Failed {
            self.finish(format!(
                "创建 Agent 失败：{}",
                snapshot.error.as_deref().unwrap_or("主机未提供原因")
            ));
        } else if snapshot.phase == CreationPhase::Completed {
            if let Some(id) = snapshot.agent_id {
                self.finish(format!("已创建 Agent：{id}。刷新 Agent 列表即可查看"));
            } else {
                self.status
                    .set("主机报告创建完成，但缺少 Agent 标识，请再次查询".to_owned());
            }
        } else {
            self.status
                .set("主机正在创建 Agent，请稍后查询创建结果".to_owned());
        }
    }

    fn finish(&self, message: String) {
        // Only a confirmed outcome releases this key; cancellation/timeout leaves it recoverable.
        Storage::remove_item(&self.storage_key);
        self.pending.take();
        self.pending_key.set(None);
        self.status.set(message);
    }
}

pub(super) fn form(
    client: Rc<Client>,
    cwd: String,
    workspace_id: String,
    provider: Rc<Provider>,
) -> impl IntoElement {
    let storage_key = format!(
        "paseo.nsh.creation:{}",
        serde_json::json!([
            client.server_info().server_id,
            workspace_id,
            provider.provider
        ])
    );
    let saved = Storage::get_item(&storage_key);
    let pending = saved
        .as_deref()
        .and_then(|value| serde_json::from_str::<PendingCreation>(value).ok())
        .filter(|record| {
            !record.key.is_empty()
                && record.provider == provider.provider
                && record.workspace_id == workspace_id
        });
    let storage_valid = saved.is_none() || pending.is_some();
    let pending_key = pending.as_ref().map(|record| record.key.clone());
    let supported = client
        .server_info()
        .features
        .as_ref()
        .and_then(|features| features.get("creationLifecycle"))
        .and_then(serde_json::Value::as_bool)
        == Some(true);
    let restored_title = pending
        .as_ref()
        .and_then(|record| record.title.clone())
        .unwrap_or_default();
    let restored_model = pending.as_ref().and_then(|record| record.model.clone());
    let restored_mode = pending.as_ref().and_then(|record| record.mode.clone());
    let state = Rc::new(CreationForm {
        client,
        task: RequestTask::new(),
        provider,
        workspace_id,
        cwd,
        storage_key,
        pending: RefCell::new(pending),
        pending_key: signal(pending_key.clone()),
        title: signal(restored_title),
        model: signal(restored_model),
        mode: signal(restored_mode),
        busy: signal(false),
        status: signal(if !storage_valid {
            "创建记录无法读取，请保留数据并检查存储".to_owned()
        } else if pending_key.is_some() {
            "有尚未确认的创建请求，请查询创建结果；重试将复用原创建标识和配置".to_owned()
        } else if !supported {
            "请更新主机后创建 Agent".to_owned()
        } else {
            String::new()
        }),
    });
    let title = state.title;
    let model = state.model;
    let mode = state.mode;
    let busy = state.busy;
    let pending_key = state.pending_key;
    let status = state.status;
    let field_state = state.clone();
    let create_state = state.clone();
    let query_state = state.clone();
    let retry_state = state.clone();
    let supported = supported && storage_valid;
    let mut content = view()
        .gap_2()
        .child(text("新建 Agent").text_color(move || theme().foreground))
        .child(
            input()
                .label("Agent 名称（可选）")
                .value(move || title.get())
                .disabled(move || field_state.blocked() || !supported)
                .on_change(move |value: &str| {
                    title.set(value.to_owned());
                    status.set(String::new());
                })
                .p_3()
                .border_1()
                .border_color(move || theme().input)
                .text_color(move || theme().foreground),
        )
        .child(
            button()
                .disabled(move || busy.get() || pending_key.get().is_some() || !supported)
                .on_press(move || {
                    model.set(None);
                    status.set(String::new());
                })
                .p_3()
                .child(
                    text(move || {
                        if model.get().is_none() {
                            "模型：服务商默认（已选择）"
                        } else {
                            "模型：服务商默认"
                        }
                    })
                    .text_color(move || theme().foreground),
                ),
        );
    if let Some(models) = &state.provider.models {
        for entry in models {
            if entry.is_selectable == Some(false) {
                continue;
            }
            let id = entry.id.clone();
            let selected_id = id.clone();
            let label = entry.label.clone();
            content = content.child(
                button()
                    .disabled(move || busy.get() || pending_key.get().is_some() || !supported)
                    .on_press(move || {
                        model.set(Some(id.clone()));
                        status.set(String::new());
                    })
                    .p_3()
                    .child(
                        text(move || {
                            format!(
                                "模型：{label}{}",
                                if model.with(|value| value.as_ref() == Some(&selected_id)) {
                                    "（已选择）"
                                } else {
                                    ""
                                }
                            )
                        })
                        .text_color(move || theme().foreground),
                    ),
            );
        }
    }
    content = content.child(
        button()
            .disabled(move || busy.get() || pending_key.get().is_some() || !supported)
            .on_press(move || {
                mode.set(None);
                status.set(String::new());
            })
            .p_3()
            .child(
                text(move || {
                    if mode.get().is_none() {
                        "模式：服务商默认（已选择）"
                    } else {
                        "模式：服务商默认"
                    }
                })
                .text_color(move || theme().foreground),
            ),
    );
    if let Some(modes) = &state.provider.modes {
        for entry in modes {
            let id = entry.id.clone();
            let selected_id = id.clone();
            let label = entry.label.clone();
            content = content.child(
                button()
                    .disabled(move || busy.get() || pending_key.get().is_some() || !supported)
                    .on_press(move || {
                        mode.set(Some(id.clone()));
                        status.set(String::new());
                    })
                    .p_3()
                    .child(
                        text(move || {
                            format!(
                                "模式：{label}{}",
                                if mode.with(|value| value.as_ref() == Some(&selected_id)) {
                                    "（已选择）"
                                } else {
                                    ""
                                }
                            )
                        })
                        .text_color(move || theme().foreground),
                    ),
            );
        }
    }
    let provider_ready = state.provider.enabled && state.provider.status == ProviderStatus::Ready;
    content
        .child(
            button()
                .disabled(move || {
                    busy.get() || pending_key.get().is_some() || !supported || !provider_ready
                })
                .on_press(move || create_state.create())
                .p_3()
                .child(text("创建 Agent").text_color(move || theme().primary)),
        )
        .child(
            button()
                .disabled(move || busy.get() || pending_key.get().is_none() || !supported)
                .on_press(move || query_state.query())
                .p_3()
                .child(text("查询创建结果").text_color(move || theme().primary)),
        )
        .child(
            button()
                .disabled(move || busy.get() || pending_key.get().is_none() || !supported)
                .on_press(move || retry_state.retry())
                .p_3()
                .child(text("重试同一次创建").text_color(move || theme().primary)),
        )
        .child(text(move || status.get()).text_color(move || theme().muted_foreground))
}
