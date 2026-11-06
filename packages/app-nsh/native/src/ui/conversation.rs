//! A viewed Agent owns its subscriptions, history, draft and local delivery transactions.

use super::project::RequestTask;
use crate::protocol::conversation::Cursor;
use crate::{
    client::{Client, EventRegistration},
    protocol::{catalog::AgentStatus, conversation::*},
    state::{
        attention::{Attention, Trigger},
        conversation::{
            CancelStatus, ConversationState, PendingMessage, PendingPermission, SendStatus,
        },
        timeline::{Action, FetchPlan, Row, Status, Timeline},
    },
};
use nsh_base::{
    MarkdownOptions, TextViewLinkPressEvent, TextViewLinks, button, input, nav_page, text_view,
    theme,
};
use nsh_core::{element::Screen, prelude::*, storage::Storage};
use std::{
    cell::{Cell, RefCell},
    future::Future,
    rc::Rc,
    time::Duration,
};

#[path = "permission.rs"]
mod permission;

#[derive(Default, serde::Serialize, serde::Deserialize)]
struct SavedComposer {
    draft: String,
    pending: Vec<PendingMessage>,
}

#[derive(Clone)]
enum DisplayRow {
    History(Row),
    Pending(PendingMessage),
}

impl PartialEq for DisplayRow {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::History(a), Self::History(b)) => {
                a.id == b.id
                    && a.epoch == b.epoch
                    && a.active == b.active
                    && a.provisional == b.provisional
                    && Rc::ptr_eq(&a.item, &b.item)
            }
            (Self::Pending(a), Self::Pending(b)) => a == b,
            _ => false,
        }
    }
}

impl DisplayRow {
    fn key(&self) -> String {
        match self {
            Self::History(row) => format!("history:{}:{}", row.epoch, row.id),
            Self::Pending(row) => format!("pending:{}", row.client_message_id),
        }
    }

    fn label(&self) -> String {
        match self {
            Self::History(row) => match &*row.item {
                TimelineItem::User(_) => "你".into(),
                TimelineItem::Assistant(_) => "助手".into(),
                TimelineItem::Reasoning(_) => "思考".into(),
                TimelineItem::Tool(tool) => format!(
                    "工具：{} · {}",
                    tool.name,
                    match tool.status {
                        ToolStatus::Running => "运行中",
                        ToolStatus::Completed => "已完成",
                        ToolStatus::Failed => "失败",
                        ToolStatus::Canceled => "已取消",
                    }
                ),
                TimelineItem::OtherKnown(_) => "会话事件".into(),
                TimelineItem::Unknown(_) => "未支持的内容".into(),
            },
            Self::Pending(row) => match row.status {
                SendStatus::Sending => "你 · 正在发送",
                SendStatus::Accepted => "你 · 已接收，等待确认",
                SendStatus::Queued => "你 · 已排队",
                SendStatus::UnknownOutcome => "你 · 发送结果待确认",
                SendStatus::Rejected => "你 · 发送失败",
                SendStatus::Observed => "你",
            }
            .into(),
        }
    }

    fn body(&self) -> String {
        match self {
            Self::Pending(row) => row.text.clone(),
            Self::History(row) => match &*row.item {
                TimelineItem::User(text)
                | TimelineItem::Assistant(text)
                | TimelineItem::Reasoning(text) => text.text.clone(),
                TimelineItem::Tool(tool) => {
                    let mut lines = Vec::new();
                    for field in [
                        "command", "filePath", "query", "url", "text", "output", "log",
                    ] {
                        if let Some(value) = tool
                            .detail
                            .fields
                            .get(field)
                            .and_then(serde_json::Value::as_str)
                        {
                            lines.push(value.to_owned());
                        }
                    }
                    if let Some(error) = tool.error.as_str() {
                        lines.push(error.to_owned());
                    }
                    lines.join("\n\n")
                }
                TimelineItem::OtherKnown(value) => {
                    match value.get("type").and_then(serde_json::Value::as_str) {
                        Some("error") => value
                            .get("message")
                            .or_else(|| value.get("error"))
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("会话发生错误")
                            .to_owned(),
                        Some("compaction") => "上下文整理".into(),
                        Some("todo") => "任务列表".into(),
                        Some("notification") => value
                            .get("message")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("会话通知")
                            .to_owned(),
                        Some("plugin") => "此插件内容尚未支持显示".into(),
                        _ => "此类会话内容尚未支持显示".into(),
                    }
                }
                TimelineItem::Unknown(_) => "此类会话内容尚未支持显示，请更新应用".into(),
            },
        }
    }
}

fn display_rows(history: Vec<Row>, pending: Vec<PendingMessage>) -> Vec<DisplayRow> {
    let pending_ids: std::collections::HashSet<_> = pending
        .iter()
        .map(|message| message.client_message_id.as_str())
        .collect();
    let mut rows: Vec<_> = history.into_iter().filter(|row| {
        !(row.provisional && matches!(&*row.item, TimelineItem::User(text) if text.client_message_id.as_deref().is_some_and(|id| pending_ids.contains(id))))
    }).map(DisplayRow::History).collect();
    rows.extend(pending.into_iter().map(DisplayRow::Pending));
    rows
}

struct Conversation {
    owner: Owner,
    active: Cell<bool>,
    client: Rc<Client>,
    id: String,
    storage_key: String,
    storage_valid: bool,
    timeline: RefCell<Timeline>,
    transactions: RefCell<ConversationState>,
    attention: RefCell<Attention>,
    attention_busy: Cell<bool>,
    attention_queued: Cell<Option<Trigger>>,
    attention_revision: Cell<u64>,
    live_revision: Cell<u64>,
    history_task: Rc<RequestTask>,
    jobs: RefCell<Vec<Spawned<()>>>,
    listeners: RefCell<Vec<EventRegistration>>,
    tokens: RefCell<Vec<String>>,
    rows: Signal<Vec<DisplayRow>>,
    permissions: Signal<Vec<PendingPermission>>,
    title: Signal<String>,
    draft: Signal<String>,
    sync_status: Signal<String>,
    history_status: Signal<String>,
    notice: Signal<String>,
    sending: Signal<bool>,
    ready: Signal<bool>,
    older: Signal<bool>,
    fetching: Signal<bool>,
    running: Signal<bool>,
    stopping: Signal<bool>,
}

impl Conversation {
    fn clear_attention(self: &Rc<Self>, trigger: Trigger) {
        if !self.active.get() || !self.client.is_connected() {
            return;
        }
        if !self.attention.borrow_mut().trigger(trigger) {
            return;
        }
        self.send_attention_clear(trigger);
    }

    fn send_attention_clear(self: &Rc<Self>, trigger: Trigger) {
        if self.attention_busy.replace(true) {
            self.attention_queued
                .set(Some(trigger.queued(self.attention_queued.get())));
            return;
        }
        let revision = self.attention_revision.get();
        let state = self.clone();
        self.job(async move {
            let result = state.client.clear_agent_attention(&state.id).await;
            state.attention_busy.set(false);
            if !state.active.get() {
                return;
            }
            if result.is_ok() && state.attention_revision.get() == revision {
                state
                    .attention_revision
                    .set(state.attention_revision.get().wrapping_add(1));
                state.attention.borrow_mut().update(false, false);
            }
            if let Some(trigger) = state.attention_queued.take() {
                state.clear_attention(trigger);
            }
        });
    }

    fn open_link(self: &Rc<Self>, url: String) {
        if !external_http_url(&url) {
            return;
        }
        let state = self.clone();
        self.job(async move {
            if !open_url(url).await && state.active.get() {
                state
                    .notice
                    .set("无法打开链接，请检查默认浏览器设置".into());
            }
        });
    }

    fn current(&self, generation: u64) -> bool {
        self.active.get() && self.timeline.borrow().generation == generation
    }

    fn job(&self, work: impl Future<Output = ()> + 'static) {
        let mut context = std::task::Context::from_waker(std::task::Waker::noop());
        self.jobs
            .borrow_mut()
            .retain_mut(|task| std::pin::Pin::new(task).poll(&mut context).is_pending());
        self.jobs.borrow_mut().push(self.owner.run(|| spawn(work)));
    }

    fn save(&self) -> bool {
        if !self.storage_valid {
            return false;
        }
        let saved = SavedComposer {
            draft: self.draft.get_untracked(),
            pending: self.transactions.borrow().pending_records(),
        };
        let result = serde_json::to_string(&saved)
            .ok()
            .is_some_and(|saved| Storage::set_item(&self.storage_key, saved).is_ok());
        if !result {
            self.notice
                .set("无法保存草稿与发送记录，请检查存储空间后重试".into());
        }
        result
    }

    fn publish(&self) {
        if !self.active.get() {
            return;
        }
        let (history, status, error, older) = {
            let timeline = self.timeline.borrow();
            (
                timeline.rows.clone(),
                timeline.status,
                timeline.error.clone(),
                timeline
                    .coverage
                    .as_ref()
                    .is_some_and(|coverage| coverage.contiguous.has_older),
            )
        };
        let (pending, permissions, stopping) = {
            let mut transactions = self.transactions.borrow_mut();
            for row in &history {
                if !row.provisional {
                    transactions.observe_user(&self.id, &row.item);
                }
            }
            let current_turn = transactions
                .turn(&self.id)
                .and_then(|turn| turn.turn_id.as_ref());
            let stopping = transactions.cancellation(&self.id).is_some_and(|cancel| {
                cancel.status == CancelStatus::Waiting
                    && cancel.target_turn_id.as_ref() == current_turn
            });
            (
                transactions
                    .pending_messages()
                    .iter()
                    .filter(|message| message.status != SendStatus::Observed)
                    .cloned()
                    .collect::<Vec<_>>(),
                transactions.permissions().to_vec(),
                stopping,
            )
        };
        self.history_status.set(format!(
            "已加载 {} 条历史消息",
            history.iter().filter(|row| !row.provisional).count()
        ));
        self.rows.set(display_rows(history, pending));
        self.permissions.set(permissions);
        self.stopping.set(stopping);
        self.older.set(older);
        self.fetching
            .set(matches!(status, Status::Bootstrapping | Status::CatchingUp));
        self.ready.set(matches!(status, Status::Current));
        self.sync_status.set(match status {
            Status::Unprepared | Status::Bootstrapping => "正在订阅会话…".into(),
            Status::CatchingUp => "正在同步会话…".into(),
            Status::Current => "会话已同步".into(),
            Status::Detached => "会话已关闭".into(),
            Status::Failed => format!("会话同步失败：{}", error.as_deref().unwrap_or("请重试")),
        });
        self.save();
    }

    fn dispose(&self) {
        if self.client.is_connected() && self.attention.borrow_mut().trigger(Trigger::AgentBlur) {
            self.client
                .clear_agent_attention_after_page(self.id.clone());
        }
        self.active.set(false);
        self.timeline.borrow_mut().detach();
        self.listeners.borrow_mut().clear();
        self.jobs.borrow_mut().clear();
        for token in self.tokens.take() {
            self.client.release_after_page(token);
        }
    }

    fn start(self: &Rc<Self>) {
        let action = self.timeline.borrow_mut().attach();
        self.actions(vec![action]);
    }

    fn snapshot(
        self: &Rc<Self>,
        snapshot: &Snapshot,
        revision: u64,
        permission_revision: u64,
        attention_revision: u64,
    ) {
        if attention_revision == self.attention_revision.get() {
            self.attention_revision
                .set(self.attention_revision.get().wrapping_add(1));
            let clear = self.attention.borrow_mut().snapshot(
                snapshot.agent.extra.get("requiresAttention")
                    == Some(&serde_json::Value::Bool(true)),
                snapshot
                    .agent
                    .extra
                    .get("attentionReason")
                    .and_then(serde_json::Value::as_str)
                    == Some("permission"),
            );
            if clear {
                self.send_attention_clear(Trigger::FocusEntry);
            }
        }
        if self.live_revision.get() == revision {
            self.transactions
                .borrow_mut()
                .snapshot_turn(&self.id, &snapshot.active_turn);
            self.running.set(matches!(
                snapshot.agent.status,
                AgentStatus::Initializing | AgentStatus::Running
            ));
        }
        self.title.set(
            snapshot
                .agent
                .title
                .clone()
                .unwrap_or_else(|| self.id.clone()),
        );
        self.transactions.borrow_mut().sync_permissions(
            &self.id,
            permission_revision,
            snapshot.pending_permissions.iter().cloned().map(Rc::new),
        );
    }

    fn event(self: &Rc<Self>, generation: u64, message: &ConversationMessage) {
        if !self.current(generation) {
            return;
        }
        let mut actions = Vec::new();
        match message {
            ConversationMessage::Stream(stream) => {
                if matches!(
                    &stream.event,
                    StreamEvent::Known(
                        KnownStreamEvent::TurnStarted { .. }
                            | KnownStreamEvent::TurnCompleted { .. }
                            | KnownStreamEvent::TurnFailed { .. }
                            | KnownStreamEvent::TurnCanceled { .. }
                    )
                ) {
                    self.live_revision
                        .set(self.live_revision.get().wrapping_add(1));
                }
                self.transactions.borrow_mut().stream(stream);
                if let StreamEvent::Known(event) = &stream.event {
                    match event {
                        KnownStreamEvent::TurnStarted { .. } => self.running.set(true),
                        KnownStreamEvent::TurnCompleted { .. }
                        | KnownStreamEvent::TurnCanceled { .. } => self
                            .running
                            .set(self.transactions.borrow().turn(&self.id).is_some()),
                        KnownStreamEvent::TurnFailed { error, .. } => {
                            self.running
                                .set(self.transactions.borrow().turn(&self.id).is_some());
                            self.notice.set(error.clone());
                        }
                        _ => {}
                    }
                }
                actions = self
                    .timeline
                    .borrow_mut()
                    .stream(generation, stream.clone());
            }
            ConversationMessage::TimelineReplacement(replacement) => {
                actions = self
                    .timeline
                    .borrow_mut()
                    .replacement(generation, replacement.clone())
            }
            ConversationMessage::PermissionRequested(request) => self
                .transactions
                .borrow_mut()
                .request_permission(&self.id, Rc::new(request.request.clone())),
            ConversationMessage::PermissionResolved(resolved) => self
                .transactions
                .borrow_mut()
                .resolve_permission(&self.id, &resolved.request_id),
            ConversationMessage::AttentionRequired(event) => {
                self.attention_revision
                    .set(self.attention_revision.get().wrapping_add(1));
                self.attention
                    .borrow_mut()
                    .update(true, matches!(event.reason, AttentionReason::Permission));
            }
            ConversationMessage::AgentStatus(update) => {
                self.live_revision
                    .set(self.live_revision.get().wrapping_add(1));
                let revision = self.live_revision.get();
                let permissions = self.transactions.borrow().permission_revision();
                self.snapshot(
                    &update.info,
                    revision,
                    permissions,
                    self.attention_revision.get(),
                );
            }
            ConversationMessage::AgentUpdate(AgentUpdate::Upsert { agent, .. }) => {
                self.live_revision
                    .set(self.live_revision.get().wrapping_add(1));
                let permissions = self.transactions.borrow().permission_revision();
                self.snapshot(
                    agent,
                    self.live_revision.get(),
                    permissions,
                    self.attention_revision.get(),
                );
            }
            ConversationMessage::AgentUpdate(AgentUpdate::Remove { .. }) => {
                self.timeline.borrow_mut().detach();
                self.notice
                    .set("此 Agent 已删除，已加载的内容仍可查看".into());
                self.running.set(false);
            }
            _ => {}
        }
        self.publish();
        self.actions(actions);
    }

    fn actions(self: &Rc<Self>, actions: Vec<Action>) {
        for action in actions {
            match action {
                Action::Subscribe { generation } => self.subscribe(generation),
                Action::Fetch {
                    generation,
                    operation_id,
                    plan,
                } => self.fetch(generation, operation_id, plan),
            }
        }
        self.publish();
    }

    fn subscribe(self: &Rc<Self>, generation: u64) {
        self.listeners.borrow_mut().clear();
        for token in self.tokens.take() {
            self.client.release_after_page(token);
        }
        let weak = Rc::downgrade(self);
        let timeline_listener = self.client.listen_agent(&self.id, None, move |message| {
            if matches!(
                message,
                ConversationMessage::Stream(_) | ConversationMessage::TimelineReplacement(_)
            ) {
                if let Some(state) = weak.upgrade() {
                    state.event(generation, message);
                }
            }
        });
        let weak = Rc::downgrade(self);
        let session_listener = self.client.listen_agent(&self.id, None, move |message| {
            if !matches!(
                message,
                ConversationMessage::Stream(_) | ConversationMessage::TimelineReplacement(_)
            ) {
                if let Some(state) = weak.upgrade() {
                    state.event(generation, message);
                }
            }
        });
        let (Ok(timeline_listener), Ok(session_listener)) = (timeline_listener, session_listener)
        else {
            self.timeline
                .borrow_mut()
                .subscription_failed(generation, "主机已断开，请重新连接".into());
            return;
        };
        self.listeners
            .borrow_mut()
            .extend([timeline_listener, session_listener]);
        let state = self.clone();
        self.job(async move {
            let result = state
                .client
                .set_session_events(
                    &[
                        ChatSessionEvent::PermissionRequest,
                        ChatSessionEvent::PermissionResolved,
                        ChatSessionEvent::AttentionRequired,
                    ],
                    Some(false),
                )
                .await;
            let session = match result {
                Ok(reply) => reply,
                Err(error) => {
                    state.subscription_error(generation, error.to_string());
                    return;
                }
            };
            if !state.current(generation) {
                if let Some(token) = session.subscription_id {
                    state.client.release_after_page(token);
                }
                return;
            }
            if let Some(token) = session.subscription_id {
                let _ = state.listeners.borrow()[1].set_subscription_id(Some(token.clone()));
                state.tokens.borrow_mut().push(token);
            }
            let result = state
                .client
                .set_timeline_subscription(&[state.id.clone()])
                .await;
            let timeline = match result {
                Ok(reply) => reply,
                Err(error) => {
                    state.subscription_error(generation, error.to_string());
                    return;
                }
            };
            if !state.current(generation) {
                if let Some(token) = timeline.subscription_id {
                    state.client.release_after_page(token);
                }
                return;
            }
            if !timeline.agent_ids.contains(&state.id) {
                state.subscription_error(generation, "主机未订阅此 Agent".into());
                return;
            }
            if let Some(token) = timeline.subscription_id {
                let _ = state.listeners.borrow()[0].set_subscription_id(Some(token.clone()));
                state.tokens.borrow_mut().push(token);
            }
            let actions = state.timeline.borrow_mut().subscribed(generation);
            state.actions(actions);
        });
    }

    fn subscription_error(&self, generation: u64, error: String) {
        if self.current(generation) {
            self.timeline
                .borrow_mut()
                .subscription_failed(generation, error);
            self.publish();
        }
    }

    fn fetch(self: &Rc<Self>, generation: u64, operation_id: String, plan: FetchPlan) {
        let options = history_options(
            plan,
            self.client
                .server_info()
                .features
                .as_ref()
                .and_then(|features| features.get("wholeTurnTimeline"))
                == Some(&serde_json::Value::Bool(true)),
        );
        let revision = self.live_revision.get();
        let permission_revision = self.transactions.borrow().permission_revision();
        let attention_revision = self.attention_revision.get();
        let state = self.clone();
        self.history_task.start(async move {
            let result = state.client.fetch_timeline(&state.id, &options).await;
            if !state.current(generation) {
                return;
            }
            let actions = match result {
                Ok(history) => {
                    if let Some(snapshot) = &history.agent {
                        state.snapshot(snapshot, revision, permission_revision, attention_revision);
                    }
                    state
                        .timeline
                        .borrow_mut()
                        .history(generation, &operation_id, history)
                }
                Err(error) => {
                    state.timeline.borrow_mut().history_failed(
                        generation,
                        &operation_id,
                        error.to_string(),
                    );
                    Vec::new()
                }
            };
            state.actions(actions);
        });
    }

    fn refresh(self: &Rc<Self>) {
        let actions = {
            let mut timeline = self.timeline.borrow_mut();
            if timeline.status == Status::Failed {
                timeline.retry()
            } else {
                let generation = timeline.generation;
                timeline.refresh(generation)
            }
        };
        self.actions(actions);
    }

    fn load_older(self: &Rc<Self>) {
        let actions = {
            let mut timeline = self.timeline.borrow_mut();
            let Some(coverage) = &timeline.coverage else {
                return;
            };
            let cursor = Cursor {
                epoch: coverage.epoch.clone(),
                seq: coverage.contiguous.span.start,
            };
            timeline.before(
                crate::state::timeline::Position {
                    epoch: cursor.epoch,
                    seq: cursor.seq,
                },
                false,
            )
        };
        self.actions(actions);
    }

    fn send(self: &Rc<Self>) {
        if self.sending.get_untracked()
            || !self.ready.get_untracked()
            || !self.storage_valid
            || self.stopping.get_untracked()
        {
            return;
        }
        let text = self.draft.get_untracked();
        if text.trim().is_empty() {
            return;
        }
        let id = uuid::Uuid::new_v4().to_string();
        let Some(attempt) = self
            .transactions
            .borrow_mut()
            .begin_send(&self.id, &id, &text)
        else {
            return;
        };
        if !self.save() {
            self.transactions.borrow_mut().send_unknown(
                &self.id,
                &id,
                attempt,
                "消息尚未发送，无法保存发送记录",
            );
            self.publish();
            return;
        }
        self.draft.set(String::new());
        self.save();
        self.send_attempt(id, text, attempt);
    }

    fn retry(self: &Rc<Self>, id: String) {
        if self.sending.get_untracked()
            || !self.ready.get_untracked()
            || !self.storage_valid
            || self.stopping.get_untracked()
        {
            return;
        }
        let message = self
            .transactions
            .borrow()
            .pending_messages()
            .iter()
            .find(|message| message.client_message_id == id)
            .cloned();
        let Some(message) = message else {
            return;
        };
        let text = message.text;
        let id = if message.status == SendStatus::Rejected {
            uuid::Uuid::new_v4().to_string()
        } else {
            id
        };
        let attempt = if message.status == SendStatus::Rejected {
            self.transactions
                .borrow_mut()
                .begin_send(&self.id, &id, &text)
        } else {
            self.transactions.borrow_mut().retry_send(&self.id, &id)
        };
        let Some(attempt) = attempt else {
            return;
        };
        if !self.save() {
            self.transactions.borrow_mut().send_unknown(
                &self.id,
                &id,
                attempt,
                "消息尚未发送，无法保存发送记录",
            );
            self.publish();
            return;
        }
        self.send_attempt(id, text, attempt);
    }

    fn send_attempt(self: &Rc<Self>, id: String, text: String, attempt: u64) {
        self.sending.set(true);
        self.notice.set(String::new());
        self.publish();
        let state = self.clone();
        self.job(async move {
            let result = state.client.send_message(&state.id, &id, &text, None).await;
            if !state.active.get() {
                return;
            }
            match result {
                Ok(reply) => {
                    state
                        .transactions
                        .borrow_mut()
                        .sent(&state.id, &id, attempt, &reply);
                    if reply.accepted {
                        state.clear_attention(Trigger::PromptSend);
                    }
                }
                Err(error) => state.transactions.borrow_mut().send_unknown(
                    &state.id,
                    &id,
                    attempt,
                    &error.to_string(),
                ),
            }
            state.sending.set(false);
            let actions = {
                let mut timeline = state.timeline.borrow_mut();
                let generation = timeline.generation;
                timeline.catch_up(generation)
            };
            state.actions(actions);
        });
    }

    fn stop(self: &Rc<Self>) {
        let Some(attempt) = self.transactions.borrow_mut().begin_cancel(&self.id) else {
            return;
        };
        self.publish();
        let state = self.clone();
        self.job(async move {
            let result = state.client.cancel_agent(&state.id).await;
            if !state.active.get() {
                return;
            }
            match result {
                Ok(reply) => state
                    .transactions
                    .borrow_mut()
                    .cancelled(&state.id, attempt, &reply),
                Err(error) => state.transactions.borrow_mut().cancel_unknown(
                    &state.id,
                    attempt,
                    &error.to_string(),
                ),
            }
            {
                let transactions = state.transactions.borrow();
                if let Some(cancel) = transactions.cancellation(&state.id) {
                    if cancel.status == CancelStatus::Confirmed
                        && transactions.turn(&state.id).is_none()
                    {
                        state.running.set(false);
                    }
                    if let Some(error) = &cancel.error {
                        state.notice.set(format!("停止结果尚未确认：{error}"));
                    }
                }
            }
            let actions = {
                let mut timeline = state.timeline.borrow_mut();
                let generation = timeline.generation;
                timeline.catch_up(generation)
            };
            state.actions(actions);
        });
    }

    fn respond(self: &Rc<Self>, id: String, response: PermissionResponse) {
        let Some(attempt) = self
            .transactions
            .borrow_mut()
            .begin_permission_response(&self.id, &id, response)
        else {
            return;
        };
        let response = self
            .transactions
            .borrow()
            .permissions()
            .iter()
            .find(|permission| permission.request.id == id)
            .and_then(|permission| permission.response.clone())
            .expect("a submitted permission stores its response");
        self.publish();
        let state = self.clone();
        self.job(async move {
            let result = state
                .client
                .respond_permission(&state.id, &id, &response)
                .await;
            if result.is_ok() {
                timer(Duration::from_secs(10)).await;
            }
            if !state.active.get() {
                return;
            }
            let error = result.err().map_or_else(
                || "尚未确认权限处理结果，请重试".to_owned(),
                |error| error.to_string(),
            );
            state
                .transactions
                .borrow_mut()
                .permission_response_failed(&state.id, &id, attempt, &error);
            state.publish();
        });
    }
}

fn history_options(plan: FetchPlan, whole_turns: bool) -> HistoryOptions {
    let (direction, cursor, merge_window) = match plan {
        FetchPlan::Tail => (Direction::Tail, None, None),
        FetchPlan::After(position) => (
            Direction::After,
            Some(Cursor {
                epoch: position.epoch,
                seq: position.seq,
            }),
            None,
        ),
        FetchPlan::Before {
            cursor,
            merge_window,
        } => (
            Direction::Before,
            Some(Cursor {
                epoch: cursor.epoch,
                seq: cursor.seq,
            }),
            Some(merge_window),
        ),
    };
    HistoryOptions {
        direction: Some(direction),
        cursor,
        merge_window,
        limit: Some(50),
        whole_turns: whole_turns.then_some(true),
    }
}

fn external_http_url(value: &str) -> bool {
    url::Url::parse(value).is_ok_and(|url| matches!(url.scheme(), "http" | "https"))
}

pub(super) fn page(client: Rc<Client>, id: String, back: impl Fn() + 'static) -> Screen {
    let storage_key = format!(
        "paseo.nsh.composer:{}",
        serde_json::json!([client.server_info().server_id, id])
    );
    let stored = Storage::get_item(&storage_key);
    let saved = stored
        .as_deref()
        .map(serde_json::from_str::<SavedComposer>)
        .transpose();
    let storage_valid = saved.is_ok();
    let saved = saved.ok().flatten().unwrap_or_default();
    let mut transactions = ConversationState::default();
    transactions.restore_pending(
        saved
            .pending
            .into_iter()
            .filter(|message| message.agent_id == id)
            .collect(),
    );
    let state = Rc::new(Conversation {
        owner: Owner::current(),
        active: Cell::new(true),
        client,
        id: id.clone(),
        storage_key,
        storage_valid,
        timeline: RefCell::new(Timeline::new(id.clone())),
        transactions: RefCell::new(transactions),
        attention: RefCell::new(Attention::default()),
        attention_busy: Cell::new(false),
        attention_queued: Cell::new(None),
        attention_revision: Cell::new(0),
        live_revision: Cell::new(0),
        history_task: RequestTask::new(),
        jobs: RefCell::new(Vec::new()),
        listeners: RefCell::new(Vec::new()),
        tokens: RefCell::new(Vec::new()),
        rows: signal(Vec::new()),
        permissions: signal(Vec::new()),
        title: signal(id),
        draft: signal(saved.draft),
        sync_status: signal(String::new()),
        history_status: signal(String::new()),
        notice: signal(if storage_valid {
            String::new()
        } else {
            "无法读取已保存的草稿，请保留数据并检查存储".into()
        }),
        sending: signal(false),
        ready: signal(false),
        older: signal(false),
        fetching: signal(false),
        running: signal(false),
        stopping: signal(false),
    });
    let cleanup_state = state.clone();
    cleanup(move || cleanup_state.dispose());
    state.start();
    let visibility_state = state.clone();
    effect(move || {
        let viewed = lifecycle() == nsh_core::platform::Lifecycle::Active;
        let clear = visibility_state.attention.borrow_mut().visibility(viewed);
        if clear && visibility_state.active.get() && visibility_state.client.is_connected() {
            visibility_state.send_attention_clear(Trigger::FocusEntry);
        }
    });
    let rows = state.rows;
    let permissions = state.permissions;
    let title = state.title;
    let draft = state.draft;
    let sync_status = state.sync_status;
    let history_status = state.history_status;
    let notice = state.notice;
    let sending = state.sending;
    let ready = state.ready;
    let older = state.older;
    let fetching = state.fetching;
    let running = state.running;
    let stopping = state.stopping;
    let refresh = state.clone();
    let previous = state.clone();
    let sender = state.clone();
    let stop = state.clone();
    let editor = state.clone();
    let focus = state.clone();
    let retry = state.clone();
    let permission_state = state.clone();
    nav_page().title("会话").child(
        view().flex_1().pb_keyboard().bg(move || theme().background)
            .child(view().p_2().gap_1()
                .child(view().flex_row().gap_2()
                    .child(button().on_press(back).p_2().child(text("返回 Agent 详情")))
                    .child(text(move || format!("会话：{}", title.get())).flex_1().number_of_lines(1).text_color(move || theme().foreground)))
                .child(view().flex_row().gap_2()
                    .child(text(move || sync_status.get()).text_color(move || theme().muted_foreground))
                    .child(text(move || history_status.get()).text_color(move || theme().muted_foreground)))
                .child(view().flex_row().gap_2()
                    .child(button().on_press(move || refresh.refresh()).p_2().child(text("刷新会话")))
                    .child(button().disabled(move || !older.get() || !ready.get() || fetching.get()).on_press(move || previous.load_older()).p_2().child(text("加载更早消息")))))
            .child(virtual_list(move || rows.with(Vec::len), move |at| {
                let row = computed(move || rows.with(|rows| rows.get(at.index()).cloned()));
                let retry = retry.clone();
                let links = retry.clone();
                view().p_3().gap_2()
                    .child(text(move || row.with(|row| row.as_ref().map(DisplayRow::label).unwrap_or_default())).text_color(move || theme().muted_foreground))
                    .child(text_view().markdown(move || row.with(|row| row.as_ref().map(|row| SharedString::from(row.body()))))
                        .options(move || MarkdownOptions { streaming: row.with(|row| matches!(row, Some(DisplayRow::History(row)) if row.active)), ..Default::default() })
                        .selectable(true).links(TextViewLinks::Report)
                        .on_link_press(move |event: &TextViewLinkPressEvent| links.open_link(event.url.to_string()))
                        .text_color(move || theme().foreground))
                    .child(text(move || row.with(|row| match row {
                        Some(DisplayRow::Pending(message)) => message.error.clone().unwrap_or_default(),
                        _ => String::new(),
                    })).text_color(move || theme().muted_foreground))
                    .child(when(move || row.with(|row| matches!(row, Some(DisplayRow::Pending(message)) if matches!(message.status, SendStatus::UnknownOutcome | SendStatus::Rejected))), move || {
                        let retry = retry.clone();
                        button().disabled(move || sending.get() || !ready.get()).on_press(move || {
                            if let Some(DisplayRow::Pending(message)) = row.get_untracked() { retry.retry(message.client_message_id); }
                        }).p_2().child(text("重试发送"))
                    }))
            }).keys(move || rows.with(|rows| rows.iter().map(DisplayRow::key).collect::<Vec<_>>()))
                .test_id("conversation-messages").estimated_item_size(px(90.0)).alignment(nsh_core::ListAlignment::Bottom).follow_end(px(60.0)).keyboard_taps(KeyboardTaps::Handled).flex_1())
            // Reserve the composer and a readable message viewport before sizing permission forms.
            .child(scroll().test_id("conversation-permissions").keyboard_taps(KeyboardTaps::Handled)
                .max_h(move || px((window().height.0 - keyboard_height().0 - 286.0).clamp(48.0, 220.0))).child(each(
                move || permissions.get(), |permission: &PendingPermission| permission.request.id.clone(),
                move |pending, _| permission::card(permission_state.clone(), pending),
            )))
            .child(view().p_2().gap_1().pb_safe_area_plus(px(8.0))
                .child(when(move || !notice.with(String::is_empty), move || scroll().max_h(px(48.0))
                    .child(text(move || notice.get()).text_color(move || theme().muted_foreground))))
                .child(input().label("消息").multiline(true).value(move || draft.get()).disabled(!storage_valid)
                    .on_focus(move |_: &()| focus.clear_attention(Trigger::InputFocus))
                    .on_change(move |value: &str| { draft.set(value.to_owned()); editor.save(); })
                    .p_2().max_h(px(100.0)).border_1().border_color(move || theme().input).text_color(move || theme().foreground))
                .child(view().flex_row().gap_2()
                    .child(button().disabled(move || !ready.get() || sending.get() || stopping.get() || !storage_valid || draft.with(|text| text.trim().is_empty())).on_press(move || sender.send()).p_2().child(text("发送")))
                    .child(button().disabled(move || stopping.get() || (!running.get() && permissions.with(Vec::is_empty))).on_press(move || stop.stop()).p_2().child(text(move || if stopping.get() { "正在停止…" } else { "停止" })))))
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::timeline::SourceRange;

    fn user(provisional: bool) -> Row {
        Row {
            id: 1,
            epoch: "epoch".into(),
            provider: "codex".into(),
            turn_id: None,
            timestamp: "now".into(),
            span: SourceRange { start: 1, end: 1 },
            sources: vec![SourceRange { start: 1, end: 1 }],
            collapsed: Rc::new(Vec::new()),
            item: Rc::new(
                serde_json::from_value(serde_json::json!({
                    "type":"user_message", "text":"hello", "clientMessageId":"client-message"
                }))
                .unwrap(),
            ),
            provisional,
            active: false,
        }
    }

    #[test]
    fn provisional_echo_displays_once_without_losing_the_durable_send_identity() {
        let mut transactions = ConversationState::default();
        transactions.begin_send("agent", "client-message", "hello");
        let rows = display_rows(vec![user(true)], transactions.pending_records());
        assert_eq!(rows.len(), 1);
        assert!(matches!(&rows[0], DisplayRow::Pending(_)));
        assert_eq!(transactions.pending_records().len(), 1);
        let committed = user(false);
        transactions.observe_user("agent", &committed.item);
        let rows = display_rows(vec![committed], transactions.pending_records());
        assert_eq!(rows.len(), 1);
        assert!(matches!(&rows[0], DisplayRow::History(_)));
        assert!(transactions.pending_records().is_empty());
    }

    #[test]
    fn history_requests_preserve_projected_paging_and_feature_gates() {
        let options = history_options(
            FetchPlan::Before {
                cursor: crate::state::timeline::Position {
                    epoch: "epoch".into(),
                    seq: 51,
                },
                merge_window: false,
            },
            false,
        );
        let wire: serde_json::Value = serde_json::from_str(
            &crate::protocol::conversation::fetch_timeline("request", "agent", &options).unwrap(),
        )
        .unwrap();
        assert_eq!(
            wire["message"]["cursor"],
            serde_json::json!({"epoch":"epoch","seq":51})
        );
        assert_eq!(wire["message"]["projection"], "projected");
        assert!(wire["message"].get("wholeTurns").is_none());
        assert_eq!(options.limit, Some(50));
    }

    #[test]
    fn message_links_keep_the_upstream_http_only_opener_boundary() {
        assert!(external_http_url("https://example.com/path"));
        assert!(external_http_url("http://localhost:6767"));
        for value in [
            "javascript:alert(1)",
            "file:///etc/passwd",
            "paseo://open",
            "mailto:a@example.com",
            "#heading",
            "/workspace/file",
        ] {
            assert!(!external_http_url(value));
        }
    }
}
