//! Direct RPC client with one root-owned WebSocket reader.

use crate::host::ParsedHost;
use crate::protocol::catalog::{
    self, AgentResult, Agents, CatalogResponse, ProviderFeatures, Providers, Workspaces,
};
use crate::protocol::conversation::{
    self, ActiveTurnBehavior, AgentConfig, Cancelled, ChatSessionEvent, ConversationMessage,
    Created, CreationKind, CreationSubscription, History, HistoryOptions, PermissionResponse,
    RpcError, Sent, SessionSubscription, SubscriptionReleased, TimelineSubscription,
};
use crate::protocol::{self, HelloRejection, ProjectList, ServerEvent, ServerInfo};
use nsh_core::{
    element::{Owner, Spawned, spawn, timer},
    websocket::{WebSocket, WebSocketData},
};
use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
    fmt,
    future::Future,
    pin::pin,
    rc::Rc,
    task::{Poll, Waker},
    time::Duration,
};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ClientError {
    Timeout,
    Closed,
    InvalidProtocol,
    Transport,
    PasswordRequired,
    IncorrectPassword,
    RequestRejected,
}

impl ClientError {
    pub(crate) fn code(self) -> &'static str {
        match self {
            Self::Timeout => "timeout",
            Self::Closed => "closed",
            Self::InvalidProtocol => "invalid_protocol",
            Self::Transport => "transport",
            Self::PasswordRequired => "password_required",
            Self::IncorrectPassword => "incorrect_password",
            Self::RequestRejected => "request_rejected",
        }
    }
}

impl fmt::Display for ClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Timeout => "主机响应超时，请检查地址与网络后重试",
            Self::Closed => "主机已断开连接，请重新连接",
            Self::InvalidProtocol => "主机协议不兼容或消息格式无效，请检查主机版本",
            Self::Transport => "无法与主机通信，请检查地址、网络与访问权限",
            Self::PasswordRequired => "主机需要密码，请填写密码后重新连接",
            Self::IncorrectPassword => "主机密码不正确，请修改后重新连接",
            Self::RequestRejected => "主机拒绝了请求，请检查主机状态后重试",
        })
    }
}

impl std::error::Error for ClientError {}

// Debug diagnostics never print daemon payloads, messages or provider options.
pub(crate) enum ConversationError {
    Client(ClientError),
    Rpc(RpcError),
    Unsupported(&'static str),
}

impl From<ClientError> for ConversationError {
    fn from(error: ClientError) -> Self {
        Self::Client(error)
    }
}

impl fmt::Display for ConversationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Client(error) => fmt::Display::fmt(error, f),
            Self::Rpc(error) => f.write_str(&error.error),
            Self::Unsupported(message) => f.write_str(message),
        }
    }
}

impl fmt::Debug for ConversationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Client(_) => "ConversationError::Client",
            Self::Rpc(_) => "ConversationError::Rpc",
            Self::Unsupported(_) => "ConversationError::Unsupported",
        })
    }
}

impl std::error::Error for ConversationError {}

impl From<HelloRejection> for ClientError {
    fn from(reason: HelloRejection) -> Self {
        match reason {
            HelloRejection::PasswordRequired => Self::PasswordRequired,
            HelloRejection::IncorrectPassword => Self::IncorrectPassword,
            HelloRejection::IncompatibleProtocol => Self::InvalidProtocol,
        }
    }
}

// No Debug: the socket and future protocol fields can carry credentials.
pub(crate) struct Client {
    owner: Owner,
    releases: Rc<RefCell<Vec<Spawned<()>>>>,
    socket: Rc<WebSocket>,
    pending: Rc<PendingRequests>,
    reader: Option<Spawned<()>>,
    on_state: Rc<dyn Fn(ClientError)>,
    server_info: ServerInfo,
}

impl Client {
    pub(crate) async fn connect(
        host: ParsedHost,
        client_id: &str,
        on_state: impl Fn(ClientError) + 'static,
    ) -> Result<Self, ClientError> {
        with_timeout(async {
            let hello = protocol::hello(client_id, host.password.as_deref())
                .map_err(|_| ClientError::InvalidProtocol)?;
            let socket = WebSocket::connect(host.url)
                .await
                .map_err(|_| ClientError::Transport)?;
            socket
                .write(WebSocketData::Text(hello))
                .await
                .map_err(|_| ClientError::Transport)?;
            loop {
                if let Some(server_info) = handshake_event(read_event(&socket).await?)? {
                    let socket = Rc::new(socket);
                    let pending = Rc::new(PendingRequests::default());
                    let on_state: Rc<dyn Fn(ClientError)> = Rc::new(on_state);
                    let owner = Owner::current();
                    let releases = Rc::new(RefCell::new(Vec::new()));
                    let weak_pending = Rc::downgrade(&pending);
                    let weak_releases = Rc::downgrade(&releases);
                    let release_socket = socket.clone();
                    let release_state = on_state.clone();
                    *pending.orphan_cleanup.borrow_mut() = Some(Rc::new(move |token| {
                        if let (Some(pending), Some(releases)) =
                            (weak_pending.upgrade(), weak_releases.upgrade())
                        {
                            release_on(
                                owner,
                                &releases,
                                release_socket.clone(),
                                pending,
                                release_state.clone(),
                                token,
                            );
                        }
                    }));
                    let read_socket = socket.clone();
                    let read_pending = pending.clone();
                    let read_state = on_state.clone();
                    let reader = spawn(async move {
                        loop {
                            let result = match read_event(&read_socket).await {
                                Ok(event) => read_pending.dispatch(event),
                                Err(error) => Err(error),
                            };
                            if let Err(error) = result {
                                fail_connection(&read_pending, &read_state, error);
                                return;
                            }
                        }
                    });
                    return Ok(Self {
                        owner,
                        releases,
                        socket,
                        pending,
                        reader: Some(reader),
                        on_state,
                        server_info,
                    });
                }
            }
        })
        .await
    }

    pub(crate) fn server_info(&self) -> &ServerInfo {
        &self.server_info
    }

    pub(crate) fn is_connected(&self) -> bool {
        self.pending.failure.get().is_none()
    }

    pub(crate) async fn projects(&self) -> Result<ProjectList, ClientError> {
        let request_id = uuid::Uuid::new_v4().to_string();
        match self
            .request(
                ReplyKind::Projects,
                &request_id,
                protocol::list_projects(&request_id),
            )
            .await?
        {
            PendingReply::Projects(value) => Ok(value),
            PendingReply::RpcError(_) => Err(ClientError::RequestRejected),
            _ => Err(ClientError::InvalidProtocol),
        }
    }

    pub(crate) async fn providers(&self, cwd: &str) -> Result<Providers, ClientError> {
        let request_id = uuid::Uuid::new_v4().to_string();
        match self
            .request(
                ReplyKind::Providers,
                &request_id,
                catalog::providers(&request_id, cwd),
            )
            .await?
        {
            PendingReply::Providers(value) => Ok(value),
            PendingReply::RpcError(_) => Err(ClientError::RequestRejected),
            _ => Err(ClientError::InvalidProtocol),
        }
    }

    pub(crate) async fn provider_features(
        &self,
        provider: &str,
        cwd: &str,
    ) -> Result<ProviderFeatures, ClientError> {
        let request_id = uuid::Uuid::new_v4().to_string();
        match self
            .request(
                ReplyKind::Features,
                &request_id,
                catalog::provider_features(&request_id, provider, cwd),
            )
            .await?
        {
            PendingReply::Features(value) => Ok(value),
            PendingReply::RpcError(_) => Err(ClientError::RequestRejected),
            _ => Err(ClientError::InvalidProtocol),
        }
    }

    pub(crate) async fn workspaces(
        &self,
        project_id: &str,
        cursor: Option<&str>,
    ) -> Result<Workspaces, ClientError> {
        let request_id = uuid::Uuid::new_v4().to_string();
        let wire = catalog::workspaces_for_project(&request_id, project_id, cursor)
            .map_err(|_| ClientError::InvalidProtocol)?;
        match self
            .request(ReplyKind::Workspaces, &request_id, wire)
            .await?
        {
            PendingReply::Workspaces(value) => Ok(value),
            PendingReply::RpcError(_) => Err(ClientError::RequestRejected),
            _ => Err(ClientError::InvalidProtocol),
        }
    }

    pub(crate) async fn agents(
        &self,
        project_id: &str,
        cursor: Option<&str>,
    ) -> Result<Agents, ClientError> {
        let request_id = uuid::Uuid::new_v4().to_string();
        let wire = catalog::agents_for_project(&request_id, project_id, cursor)
            .map_err(|_| ClientError::InvalidProtocol)?;
        match self.request(ReplyKind::Agents, &request_id, wire).await? {
            PendingReply::Agents(value) => Ok(value),
            PendingReply::RpcError(_) => Err(ClientError::RequestRejected),
            _ => Err(ClientError::InvalidProtocol),
        }
    }

    pub(crate) async fn agent(&self, agent_id: &str) -> Result<AgentResult, ClientError> {
        let request_id = uuid::Uuid::new_v4().to_string();
        match self
            .request(
                ReplyKind::Agent,
                &request_id,
                catalog::agent(&request_id, agent_id),
            )
            .await?
        {
            PendingReply::Agent(value) => Ok(value),
            PendingReply::RpcError(_) => Err(ClientError::RequestRejected),
            _ => Err(ClientError::InvalidProtocol),
        }
    }

    pub(crate) async fn create_agent(
        &self,
        config: &AgentConfig,
        workspace_id: Option<&str>,
        idempotency_key: Option<&str>,
        subscribe: Option<bool>,
    ) -> Result<Created, ConversationError> {
        self.require_feature("creationLifecycle", "请更新主机后再创建或恢复会话")?;
        let id = uuid::Uuid::new_v4().to_string();
        let wire =
            conversation::create_agent(&id, config, workspace_id, idempotency_key, subscribe)
                .map_err(|_| ClientError::InvalidProtocol)?;
        match self.chat_request(ReplyKind::Created, &id, wire).await? {
            PendingReply::Created(value) => Ok(value),
            _ => Err(ClientError::InvalidProtocol.into()),
        }
    }

    pub(crate) async fn subscribe_creation(
        &self,
        kind: CreationKind,
        idempotency_key: &str,
        subscribe: Option<bool>,
    ) -> Result<CreationSubscription, ConversationError> {
        self.require_feature("creationLifecycle", "请更新主机后再创建或恢复会话")?;
        let id = uuid::Uuid::new_v4().to_string();
        let wire = conversation::subscribe_creation(&id, kind, idempotency_key, subscribe)
            .map_err(|_| ClientError::InvalidProtocol)?;
        match self
            .chat_request(ReplyKind::CreationSubscription, &id, wire)
            .await?
        {
            PendingReply::CreationSubscription(value) => Ok(value),
            _ => Err(ClientError::InvalidProtocol.into()),
        }
    }

    // These are connection-wide, last-query-wins selections while owned_subscriptions is absent
    // from hello. A returned token does not turn a local listener into an independent subscription.
    pub(crate) async fn set_session_events(
        &self,
        events: &[ChatSessionEvent],
        notifications: Option<bool>,
    ) -> Result<SessionSubscription, ConversationError> {
        self.require_feature("explicitEventSubscriptions", "请更新主机后再订阅会话事件")?;
        let id = uuid::Uuid::new_v4().to_string();
        let wire = conversation::subscribe_session_events(&id, events, notifications);
        match self
            .chat_request(ReplyKind::SessionSubscription, &id, wire)
            .await?
        {
            PendingReply::SessionSubscription(value) => Ok(value),
            _ => Err(ClientError::InvalidProtocol.into()),
        }
    }

    pub(crate) async fn set_timeline_subscription(
        &self,
        agent_ids: &[String],
    ) -> Result<TimelineSubscription, ConversationError> {
        self.require_feature("selectiveAgentTimeline", "请更新主机后再订阅会话时间线")?;
        let id = uuid::Uuid::new_v4().to_string();
        let wire = conversation::subscribe_timeline(&id, agent_ids);
        match self
            .chat_request(ReplyKind::TimelineSubscription, &id, wire)
            .await?
        {
            PendingReply::TimelineSubscription(value) => Ok(value),
            _ => Err(ClientError::InvalidProtocol.into()),
        }
    }

    pub(crate) async fn release_subscription(
        &self,
        subscription_id: &str,
    ) -> Result<SubscriptionReleased, ConversationError> {
        // Stop local delivery before waiting for the daemon to acknowledge release.
        self.pending.release_token(subscription_id);
        let id = uuid::Uuid::new_v4().to_string();
        let wire = conversation::release_subscription(&id, subscription_id);
        let result = self
            .chat_request(ReplyKind::SubscriptionReleased, &id, wire)
            .await;
        self.pending
            .releasing_subscriptions
            .borrow_mut()
            .remove(subscription_id);
        match result? {
            PendingReply::SubscriptionReleased(value)
                if value.subscription_id == subscription_id =>
            {
                Ok(value)
            }
            _ => Err(ClientError::InvalidProtocol.into()),
        }
    }

    /// Page disposal stops local delivery immediately; the connection owns the bounded release RPC.
    pub(crate) fn release_after_page(&self, subscription_id: String) {
        release_on(
            self.owner,
            &self.releases,
            self.socket.clone(),
            self.pending.clone(),
            self.on_state.clone(),
            subscription_id,
        );
    }

    pub(crate) async fn fetch_timeline(
        &self,
        agent_id: &str,
        options: &HistoryOptions,
    ) -> Result<History, ConversationError> {
        if options.whole_turns.is_some() {
            self.require_feature("wholeTurnTimeline", "请更新主机后再按完整回合加载时间线")?;
        }
        let id = uuid::Uuid::new_v4().to_string();
        let wire = conversation::fetch_timeline(&id, agent_id, options)
            .map_err(|_| ClientError::InvalidProtocol)?;
        match self.chat_request(ReplyKind::History, &id, wire).await? {
            PendingReply::History(value) => Ok(value),
            _ => Err(ClientError::InvalidProtocol.into()),
        }
    }

    pub(crate) async fn send_message(
        &self,
        agent_id: &str,
        message_id: &str,
        text: &str,
        active_turn_behavior: Option<ActiveTurnBehavior>,
    ) -> Result<Sent, ConversationError> {
        let id = uuid::Uuid::new_v4().to_string();
        let wire =
            conversation::send_message(&id, agent_id, message_id, text, active_turn_behavior);
        // Keep accepted/error/deliveryStatus intact. An RPC reply is not a completed agent turn.
        match self.chat_request(ReplyKind::Sent, &id, wire).await? {
            PendingReply::Sent(value) => Ok(value),
            _ => Err(ClientError::InvalidProtocol.into()),
        }
    }

    pub(crate) async fn cancel_agent(
        &self,
        agent_id: &str,
    ) -> Result<Cancelled, ConversationError> {
        let id = uuid::Uuid::new_v4().to_string();
        match self
            .chat_request(
                ReplyKind::Cancelled,
                &id,
                conversation::cancel_agent(&id, agent_id),
            )
            .await?
        {
            PendingReply::Cancelled(value) => Ok(value),
            _ => Err(ClientError::InvalidProtocol.into()),
        }
    }

    pub(crate) async fn respond_permission(
        &self,
        agent_id: &str,
        permission_request_id: &str,
        response: &PermissionResponse,
    ) -> Result<(), ClientError> {
        // This only confirms the socket write. permission_resolved carries the actual resolution.
        if let Some(error) = self.pending.failure.get() {
            return Err(error);
        }
        let wire = conversation::respond_permission(agent_id, permission_request_id, response);
        if self.socket.write(WebSocketData::Text(wire)).await.is_err() {
            fail_connection(&self.pending, &self.on_state, ClientError::Transport);
            return Err(ClientError::Transport);
        }
        Ok(())
    }

    pub(crate) fn listen_agent(
        &self,
        agent_id: &str,
        subscription_id: Option<&str>,
        handler: impl Fn(&ConversationMessage) + 'static,
    ) -> Result<EventRegistration, ClientError> {
        if let Some(error) = self.pending.failure.get() {
            return Err(error);
        }
        Ok(self.pending.events.register(
            EventTarget::Agent(agent_id.to_owned()),
            subscription_id,
            handler,
        ))
    }

    pub(crate) fn listen_creation(
        &self,
        kind: CreationKind,
        idempotency_key: &str,
        subscription_id: Option<&str>,
        handler: impl Fn(&ConversationMessage) + 'static,
    ) -> Result<EventRegistration, ClientError> {
        if let Some(error) = self.pending.failure.get() {
            return Err(error);
        }
        Ok(self.pending.events.register(
            EventTarget::Creation {
                kind,
                key: idempotency_key.to_owned(),
            },
            subscription_id,
            handler,
        ))
    }

    fn require_feature(
        &self,
        feature: &str,
        message: &'static str,
    ) -> Result<(), ConversationError> {
        if self
            .server_info
            .features
            .as_ref()
            .and_then(|features| features.get(feature))
            == Some(&serde_json::Value::Bool(true))
        {
            Ok(())
        } else {
            Err(ConversationError::Unsupported(message))
        }
    }

    async fn chat_request(
        &self,
        kind: ReplyKind,
        id: &str,
        wire: String,
    ) -> Result<PendingReply, ConversationError> {
        match self.request(kind, id, wire).await? {
            PendingReply::RpcError(error) => Err(ConversationError::Rpc(error)),
            value => Ok(value),
        }
    }

    async fn request(
        &self,
        expected: ReplyKind,
        request_id: &str,
        wire: String,
    ) -> Result<PendingReply, ClientError> {
        request_on(
            &self.socket,
            &self.pending,
            &self.on_state,
            expected,
            request_id,
            wire,
        )
        .await
    }
}

fn release_on(
    owner: Owner,
    releases: &RefCell<Vec<Spawned<()>>>,
    socket: Rc<WebSocket>,
    pending: Rc<PendingRequests>,
    on_state: Rc<dyn Fn(ClientError)>,
    subscription_id: String,
) {
    pending.release_token(&subscription_id);
    if !owner.is_live() || pending.failure.get().is_some() {
        return;
    }
    let mut context = std::task::Context::from_waker(std::task::Waker::noop());
    releases
        .borrow_mut()
        .retain_mut(|task| std::pin::Pin::new(task).poll(&mut context).is_pending());
    let task = owner.run(|| {
        spawn(async move {
            let id = uuid::Uuid::new_v4().to_string();
            let _ = request_on(
                &socket,
                &pending,
                &on_state,
                ReplyKind::SubscriptionReleased,
                &id,
                conversation::release_subscription(&id, &subscription_id),
            )
            .await;
            pending
                .releasing_subscriptions
                .borrow_mut()
                .remove(&subscription_id);
        })
    });
    releases.borrow_mut().push(task);
}

async fn request_on(
    socket: &WebSocket,
    pending: &Rc<PendingRequests>,
    on_state: &Rc<dyn Fn(ClientError)>,
    expected: ReplyKind,
    request_id: &str,
    wire: String,
) -> Result<PendingReply, ClientError> {
    let (guard, reply) = pending.register(request_id.to_owned(), expected)?;
    let result = with_timeout(async {
        if socket.write(WebSocketData::Text(wire)).await.is_err() {
            fail_connection(pending, on_state, ClientError::Transport);
            return Err(ClientError::Transport);
        }
        reply.await
    })
    .await;
    drop(guard);
    result
}

impl Drop for Client {
    fn drop(&mut self) {
        self.pending.close(ClientError::Closed);
        if let Some(reader) = self.reader.take() {
            reader.cancel();
        }
    }
}

// This owns only a local callback. Drop/stop never claim that the daemon subscription was released;
// the caller also awaits Client::release_subscription when it owns a returned server token.
pub(crate) struct EventRegistration {
    events: Rc<EventListeners>,
    id: String,
    listener: Rc<EventListener>,
}

impl EventRegistration {
    pub(crate) fn set_subscription_id(
        &self,
        subscription_id: Option<String>,
    ) -> Result<(), ClientError> {
        if !self.listener.active.get() {
            return Err(ClientError::Closed);
        }
        *self.listener.subscription_id.borrow_mut() = subscription_id;
        Ok(())
    }

    pub(crate) fn stop(&self) {
        self.listener.active.set(false);
        self.events.listeners.borrow_mut().remove(&self.id);
    }
}

impl Drop for EventRegistration {
    fn drop(&mut self) {
        self.stop();
    }
}

enum EventTarget {
    Agent(String),
    Creation { kind: CreationKind, key: String },
}

struct EventListener {
    target: EventTarget,
    subscription_id: RefCell<Option<String>>,
    active: Cell<bool>,
    handler: Box<dyn Fn(&ConversationMessage)>,
}

#[derive(Default)]
struct EventListeners {
    listeners: RefCell<HashMap<String, Rc<EventListener>>>,
}

impl EventListeners {
    fn register(
        self: &Rc<Self>,
        target: EventTarget,
        subscription_id: Option<&str>,
        handler: impl Fn(&ConversationMessage) + 'static,
    ) -> EventRegistration {
        let id = uuid::Uuid::new_v4().to_string();
        let listener = Rc::new(EventListener {
            target,
            subscription_id: RefCell::new(subscription_id.map(str::to_owned)),
            active: Cell::new(true),
            handler: Box::new(handler),
        });
        self.listeners
            .borrow_mut()
            .insert(id.clone(), listener.clone());
        EventRegistration {
            events: self.clone(),
            id,
            listener,
        }
    }

    fn dispatch(&self, message: &ConversationMessage) {
        // Never retain a RefCell borrow across UI callbacks: a callback may stop a listener.
        let listeners: Vec<_> = self.listeners.borrow().values().cloned().collect();
        for listener in listeners {
            if listener.active.get() && listener.matches(message) {
                (listener.handler)(message);
            }
        }
    }

    fn release_token(&self, token: &str) {
        self.listeners.borrow_mut().retain(|_, listener| {
            if listener.subscription_id.borrow().as_deref() == Some(token) {
                listener.active.set(false);
                false
            } else {
                true
            }
        });
    }

    fn close(&self) {
        let listeners = std::mem::take(&mut *self.listeners.borrow_mut());
        for listener in listeners.into_values() {
            listener.active.set(false);
        }
    }
}

impl EventListener {
    fn matches(&self, message: &ConversationMessage) -> bool {
        let (matches_target, token) = match (&self.target, message) {
            (EventTarget::Creation { kind, key }, ConversationMessage::CreationUpdate(value)) => (
                &value.kind == kind && &value.idempotency_key == key,
                value.subscription_id.as_deref(),
            ),
            (EventTarget::Agent(id), ConversationMessage::Stream(value)) => {
                (id == &value.agent_id, value.subscription_id.as_deref())
            }
            (EventTarget::Agent(id), ConversationMessage::PermissionRequested(value)) => {
                (id == &value.agent_id, value.subscription_id.as_deref())
            }
            (EventTarget::Agent(id), ConversationMessage::PermissionResolved(value)) => {
                (id == &value.agent_id, value.subscription_id.as_deref())
            }
            (EventTarget::Agent(id), ConversationMessage::AttentionRequired(value)) => {
                (id == &value.agent_id, value.subscription_id.as_deref())
            }
            (EventTarget::Agent(id), ConversationMessage::TimelineReplacement(value)) => {
                (id == &value.agent_id, value.subscription_id.as_deref())
            }
            (EventTarget::Agent(id), ConversationMessage::AgentStatus(value)) => {
                (id == &value.agent_id, None)
            }
            (
                EventTarget::Agent(id),
                ConversationMessage::AgentUpdate(conversation::AgentUpdate::Upsert {
                    agent,
                    subscription_id,
                    ..
                }),
            ) => (id == &agent.agent.id, subscription_id.as_deref()),
            (
                EventTarget::Agent(id),
                ConversationMessage::AgentUpdate(conversation::AgentUpdate::Remove {
                    agent_id,
                    subscription_id,
                    ..
                }),
            ) => (id == agent_id, subscription_id.as_deref()),
            _ => return false,
        };
        // Legacy delivery omits tags, even when subscription responses return server IDs. A tag,
        // when present, must match the explicitly bound subscription, never merely the agent ID.
        matches_target
            && token.is_none_or(|token| self.subscription_id.borrow().as_deref() == Some(token))
    }
}

#[derive(Default)]
struct PendingRequests {
    slots: RefCell<HashMap<String, Rc<RefCell<PendingSlot>>>>,
    failure: Cell<Option<ClientError>>,
    events: Rc<EventListeners>,
    // Buffered replies have not reached their caller yet; duplicate frames must retain them too.
    active_subscriptions: RefCell<HashSet<String>>,
    buffered_subscriptions: RefCell<HashMap<String, usize>>,
    releasing_subscriptions: RefCell<HashSet<String>>,
    orphan_cleanup: RefCell<Option<Rc<dyn Fn(String)>>>,
}

struct PendingSlot {
    expected: ReplyKind,
    result: Option<Result<PendingReply, ClientError>>,
    waker: Option<Waker>,
}

struct PendingGuard {
    pending: Rc<PendingRequests>,
    request_id: String,
    slot: Rc<RefCell<PendingSlot>>,
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        let mut slots = self.pending.slots.borrow_mut();
        if slots
            .get(&self.request_id)
            .is_some_and(|slot| Rc::ptr_eq(slot, &self.slot))
        {
            slots.remove(&self.request_id);
        }
        drop(slots);
        let abandoned = self.slot.borrow_mut().result.take();
        if let Some(Ok(reply)) = abandoned
            && let Some(token) = reply.subscription_id()
        {
            self.pending.unbuffer_token(token);
            self.pending.cleanup_orphan(token);
        }
    }
}

struct Response(Rc<RefCell<PendingSlot>>, Rc<PendingRequests>);

impl Future for Response {
    type Output = Result<PendingReply, ClientError>;

    fn poll(self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<Self::Output> {
        let mut reply = self.0.borrow_mut();
        match reply.result.take() {
            Some(result) => {
                drop(reply);
                if let Ok(reply) = &result
                    && let Some(token) = reply.subscription_id()
                {
                    self.1.unbuffer_token(token);
                    if self.1.failure.get().is_none()
                        && !self.1.releasing_subscriptions.borrow().contains(token)
                    {
                        self.1
                            .active_subscriptions
                            .borrow_mut()
                            .insert(token.into());
                    }
                }
                Poll::Ready(result)
            }
            None => {
                reply.waker = Some(cx.waker().clone());
                Poll::Pending
            }
        }
    }
}

impl PendingRequests {
    fn register(
        self: &Rc<Self>,
        request_id: String,
        expected: ReplyKind,
    ) -> Result<(PendingGuard, Response), ClientError> {
        if let Some(error) = self.failure.get() {
            return Err(error);
        }
        if self.slots.borrow().contains_key(&request_id) {
            return Err(ClientError::InvalidProtocol);
        }
        let slot = Rc::new(RefCell::new(PendingSlot {
            expected,
            result: None,
            waker: None,
        }));
        self.slots
            .borrow_mut()
            .insert(request_id.clone(), slot.clone());
        Ok((
            PendingGuard {
                pending: self.clone(),
                request_id,
                slot: slot.clone(),
            },
            Response(slot, self.clone()),
        ))
    }

    fn dispatch(&self, event: ServerEvent) -> Result<(), ClientError> {
        if let Some(error) = self.failure.get() {
            return Err(error);
        }
        let reply = match event {
            ServerEvent::Projects(value) => PendingReply::Projects(value),
            ServerEvent::Catalog(value) => match value {
                CatalogResponse::Providers(value) => PendingReply::Providers(value),
                CatalogResponse::ProviderFeatures(value) => PendingReply::Features(value),
                CatalogResponse::Workspaces(value) => PendingReply::Workspaces(value),
                CatalogResponse::Agents(value) => PendingReply::Agents(value),
                CatalogResponse::Agent(value) => PendingReply::Agent(value),
            },
            ServerEvent::Conversation(value) => match value {
                ConversationMessage::Created(value) => PendingReply::Created(value),
                ConversationMessage::CreationSubscription(value) => {
                    PendingReply::CreationSubscription(value)
                }
                ConversationMessage::SessionSubscription(value) => {
                    PendingReply::SessionSubscription(value)
                }
                ConversationMessage::TimelineSubscription(value) => {
                    PendingReply::TimelineSubscription(value)
                }
                ConversationMessage::SubscriptionReleased(value) => {
                    PendingReply::SubscriptionReleased(value)
                }
                ConversationMessage::History(value) => PendingReply::History(value),
                ConversationMessage::Sent(value) => PendingReply::Sent(value),
                ConversationMessage::Cancelled(value) => PendingReply::Cancelled(value),
                ConversationMessage::RpcError(error) => {
                    let slot = {
                        let mut slots = self.slots.borrow_mut();
                        if slots.get(&error.request_id).is_some_and(|slot| {
                            error
                                .request_type
                                .as_deref()
                                .is_none_or(|kind| kind == slot.borrow().expected.request_type())
                        }) {
                            slots.remove(&error.request_id)
                        } else {
                            None
                        }
                    };
                    if let Some(slot) = slot {
                        deliver(&slot, Ok(PendingReply::RpcError(error)));
                    }
                    return Ok(());
                }
                event => {
                    self.events.dispatch(&event);
                    return Ok(());
                }
            },
            ServerEvent::HelloRejected(reason) => return Err(reason.into()),
            // Unknown frames never resolve RPC slots or become successful chat events.
            ServerEvent::Unknown(_) | ServerEvent::Pong | ServerEvent::ServerInfo(_) => {
                return Ok(());
            }
        };
        let slot = {
            let mut slots = self.slots.borrow_mut();
            if slots
                .get(reply.request_id())
                .is_some_and(|slot| slot.borrow().expected == reply.kind())
            {
                slots.remove(reply.request_id())
            } else {
                None
            }
        };
        if let Some(slot) = slot {
            if let Some(token) = reply.subscription_id() {
                *self
                    .buffered_subscriptions
                    .borrow_mut()
                    .entry(token.into())
                    .or_default() += 1;
            }
            deliver(&slot, Ok(reply));
        } else if let Some(token) = reply.subscription_id() {
            self.cleanup_orphan(token);
        }
        Ok(())
    }

    fn unbuffer_token(&self, token: &str) {
        let mut buffered = self.buffered_subscriptions.borrow_mut();
        if let Some(count) = buffered.get_mut(token) {
            *count -= 1;
            if *count == 0 {
                buffered.remove(token);
            }
        }
    }

    fn release_token(&self, token: &str) {
        self.active_subscriptions.borrow_mut().remove(token);
        self.releasing_subscriptions
            .borrow_mut()
            .insert(token.into());
        self.events.release_token(token);
    }

    fn cleanup_orphan(&self, token: &str) {
        if self.failure.get().is_some()
            || self.active_subscriptions.borrow().contains(token)
            || self.buffered_subscriptions.borrow().contains_key(token)
            || !self
                .releasing_subscriptions
                .borrow_mut()
                .insert(token.into())
        {
            return;
        }
        let cleanup = self.orphan_cleanup.borrow().clone();
        // Schedule on the root owner: the single reader must keep dispatching the release reply.
        if let Some(cleanup) = cleanup {
            cleanup(token.into());
        }
    }

    fn close(&self, error: ClientError) -> bool {
        if self.failure.get().is_some() {
            return false;
        }
        self.failure.set(Some(error));
        self.orphan_cleanup.borrow_mut().take();
        self.active_subscriptions.borrow_mut().clear();
        self.buffered_subscriptions.borrow_mut().clear();
        self.releasing_subscriptions.borrow_mut().clear();
        self.events.close();
        let slots = std::mem::take(&mut *self.slots.borrow_mut());
        for slot in slots.into_values() {
            deliver(&slot, Err(error));
        }
        true
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReplyKind {
    Projects,
    Providers,
    Features,
    Workspaces,
    Agents,
    Agent,
    Created,
    CreationSubscription,
    SessionSubscription,
    TimelineSubscription,
    SubscriptionReleased,
    History,
    Sent,
    Cancelled,
    RpcError,
}

impl ReplyKind {
    fn request_type(self) -> &'static str {
        match self {
            Self::Projects => "project.list.request",
            Self::Providers => "get_providers_snapshot_request",
            Self::Features => "list_provider_features_request",
            Self::Workspaces => "fetch_workspaces_request",
            Self::Agents => "fetch_agents_request",
            Self::Agent => "fetch_agent_request",
            Self::Created => "agent.create.request",
            Self::CreationSubscription => "creation.subscribe.request",
            Self::SessionSubscription => "session.events.set_subscription.request",
            Self::TimelineSubscription => "agent.timeline.set_subscription.request",
            Self::SubscriptionReleased => "subscription.release.request",
            Self::History => "fetch_agent_timeline_request",
            Self::Sent => "send_agent_message_request",
            Self::Cancelled => "cancel_agent_request",
            Self::RpcError => "",
        }
    }
}

enum PendingReply {
    Projects(ProjectList),
    Providers(Providers),
    Features(ProviderFeatures),
    Workspaces(Workspaces),
    Agents(Agents),
    Agent(AgentResult),
    Created(Created),
    CreationSubscription(CreationSubscription),
    SessionSubscription(SessionSubscription),
    TimelineSubscription(TimelineSubscription),
    SubscriptionReleased(SubscriptionReleased),
    History(History),
    Sent(Sent),
    Cancelled(Cancelled),
    RpcError(RpcError),
}

impl PendingReply {
    fn subscription_id(&self) -> Option<&str> {
        match self {
            Self::Created(value) => value.creation.as_ref()?.subscription_id.as_deref(),
            Self::CreationSubscription(value) => value
                .subscription_id
                .as_deref()
                .or_else(|| value.snapshot.as_ref()?.subscription_id.as_deref()),
            Self::SessionSubscription(value) => value.subscription_id.as_deref(),
            Self::TimelineSubscription(value) => value.subscription_id.as_deref(),
            _ => None,
        }
    }

    fn kind(&self) -> ReplyKind {
        match self {
            Self::Projects(_) => ReplyKind::Projects,
            Self::Providers(_) => ReplyKind::Providers,
            Self::Features(_) => ReplyKind::Features,
            Self::Workspaces(_) => ReplyKind::Workspaces,
            Self::Agents(_) => ReplyKind::Agents,
            Self::Agent(_) => ReplyKind::Agent,
            Self::Created(_) => ReplyKind::Created,
            Self::CreationSubscription(_) => ReplyKind::CreationSubscription,
            Self::SessionSubscription(_) => ReplyKind::SessionSubscription,
            Self::TimelineSubscription(_) => ReplyKind::TimelineSubscription,
            Self::SubscriptionReleased(_) => ReplyKind::SubscriptionReleased,
            Self::History(_) => ReplyKind::History,
            Self::Sent(_) => ReplyKind::Sent,
            Self::Cancelled(_) => ReplyKind::Cancelled,
            Self::RpcError(_) => ReplyKind::RpcError,
        }
    }

    fn request_id(&self) -> &str {
        match self {
            Self::Projects(value) => &value.request_id,
            Self::Providers(value) => &value.request_id,
            Self::Features(value) => &value.request_id,
            Self::Workspaces(value) => &value.request_id,
            Self::Agents(value) => &value.request_id,
            Self::Agent(value) => &value.request_id,
            Self::Created(value) => &value.request_id,
            Self::CreationSubscription(value) => &value.request_id,
            Self::SessionSubscription(value) => &value.request_id,
            Self::TimelineSubscription(value) => &value.request_id,
            Self::SubscriptionReleased(value) => &value.request_id,
            Self::History(value) => &value.request_id,
            Self::Sent(value) => &value.request_id,
            Self::Cancelled(value) => &value.request_id,
            Self::RpcError(value) => &value.request_id,
        }
    }
}

fn deliver(slot: &RefCell<PendingSlot>, result: Result<PendingReply, ClientError>) {
    let waker = {
        let mut reply = slot.borrow_mut();
        reply.result = Some(result);
        reply.waker.take()
    };
    if let Some(waker) = waker {
        waker.wake();
    }
}

fn fail_connection(
    pending: &PendingRequests,
    on_state: &Rc<dyn Fn(ClientError)>,
    error: ClientError,
) {
    if pending.close(error) {
        on_state(error);
    }
}

async fn read_event(socket: &WebSocket) -> Result<ServerEvent, ClientError> {
    match socket.read().await.map_err(|_| ClientError::Transport)? {
        Some(WebSocketData::Text(text)) => {
            protocol::parse_server_event(&text).map_err(|_| ClientError::InvalidProtocol)
        }
        // This hello never advertises binary chunking; these RPCs have text responses only.
        Some(WebSocketData::Bytes(_)) => Err(ClientError::InvalidProtocol),
        None => Err(ClientError::Closed),
    }
}

fn handshake_event(event: ServerEvent) -> Result<Option<ServerInfo>, ClientError> {
    match event {
        ServerEvent::ServerInfo(info) if info.protocol_version == Some(1) => Ok(Some(info)),
        ServerEvent::ServerInfo(_) => Err(ClientError::InvalidProtocol),
        ServerEvent::HelloRejected(reason) => Err(reason.into()),
        _ => Ok(None),
    }
}

// The one timer spans connect/write/all reads, so unrelated events never extend the deadline.
async fn with_timeout<T>(
    operation: impl Future<Output = Result<T, ClientError>>,
) -> Result<T, ClientError> {
    race_deadline(operation, timer(REQUEST_TIMEOUT)).await
}

async fn race_deadline<T>(
    operation: impl Future<Output = Result<T, ClientError>>,
    deadline: impl Future<Output = ()>,
) -> Result<T, ClientError> {
    let mut operation = pin!(operation);
    let mut deadline = pin!(deadline);
    std::future::poll_fn(|cx| {
        if deadline.as_mut().poll(cx).is_ready() {
            Poll::Ready(Err(ClientError::Timeout))
        } else {
            operation.as_mut().poll(cx)
        }
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use std::{
        future,
        task::{Context, Waker},
    };

    fn fixtures() -> Vec<Value> {
        serde_json::from_str(include_str!("../../protocol-fixtures/direct-session.json")).unwrap()
    }

    fn event(frame: &Value) -> ServerEvent {
        protocol::parse_server_event(&frame.to_string()).unwrap()
    }

    fn catalog_frame(request_id: &str) -> Value {
        let capture: Vec<Value> =
            serde_json::from_str(include_str!("../../protocol-fixtures/catalog.json")).unwrap();
        capture
            .into_iter()
            .find(|entry| {
                entry["direction"] == "server"
                    && entry["frame"]["message"]["payload"]["requestId"] == request_id
            })
            .unwrap()["frame"]
            .clone()
    }

    fn conversation_capture() -> Vec<Value> {
        serde_json::from_str(include_str!("../../protocol-fixtures/conversation.json")).unwrap()
    }

    fn conversation_frame(request_id: &str) -> Value {
        conversation_capture()
            .into_iter()
            .find(|entry| {
                entry["direction"] == "server"
                    && entry["frame"]["message"]["payload"]["requestId"] == request_id
            })
            .unwrap()["frame"]
            .clone()
    }

    fn stream_frame(agent_id: &str, subscription_id: Option<&str>) -> Value {
        let mut frame = conversation_capture()
            .into_iter()
            .find(|entry| entry["frame"]["message"]["type"] == "agent_stream")
            .unwrap()["frame"]
            .clone();
        frame["message"]["payload"]["agentId"] = json!(agent_id);
        if let Some(id) = subscription_id {
            frame["message"]["payload"]["subscriptionId"] = json!(id);
        } else {
            frame["message"]["payload"]
                .as_object_mut()
                .unwrap()
                .remove("subscriptionId");
        }
        frame
    }

    fn subscription_frame(id: &str, token: &str) -> Value {
        let mut frame = conversation_frame(id);
        if id == "chat-create" {
            frame["message"]["payload"]["creation"]["subscriptionId"] = json!(token);
        } else {
            frame["message"]["payload"]["subscriptionId"] = json!(token);
        }
        frame
    }

    fn capture_orphans(pending: &PendingRequests) -> Rc<RefCell<Vec<String>>> {
        let tokens = Rc::new(RefCell::new(Vec::new()));
        let captured = tokens.clone();
        *pending.orphan_cleanup.borrow_mut() = Some(Rc::new(move |token| {
            captured.borrow_mut().push(token);
        }));
        tokens
    }

    #[test]
    fn cancelled_subscriptions_release_all_late_tokens_once() {
        for (id, kind) in [
            ("chat-create", ReplyKind::Created),
            ("chat-observe", ReplyKind::CreationSubscription),
            ("chat-events", ReplyKind::SessionSubscription),
            ("chat-timeline", ReplyKind::TimelineSubscription),
        ] {
            let pending = Rc::new(PendingRequests::default());
            let orphans = capture_orphans(&pending);
            let (guard, response) = pending.register(id.into(), kind).unwrap();
            let mut request = Box::pin(async move {
                let _guard = guard;
                response.await
            });
            assert!(
                request
                    .as_mut()
                    .poll(&mut Context::from_waker(Waker::noop()))
                    .is_pending()
            );
            drop(request);
            let frame = subscription_frame(id, "orphan");
            pending.dispatch(event(&frame)).unwrap();
            pending.dispatch(event(&frame)).unwrap();
            assert_eq!(*orphans.borrow(), ["orphan"]);
            assert!(pending.active_subscriptions.borrow().is_empty());
            assert!(pending.buffered_subscriptions.borrow().is_empty());
        }
    }

    #[test]
    fn timeout_releases_a_subscription_created_after_the_deadline() {
        let pending = Rc::new(PendingRequests::default());
        let orphans = capture_orphans(&pending);
        let (guard, response) = pending
            .register("chat-events".into(), ReplyKind::SessionSubscription)
            .unwrap();
        let expired = Cell::new(false);
        let operation = async move {
            let _guard = guard;
            response.await
        };
        let deadline = future::poll_fn(|_| {
            if expired.get() {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        });
        let mut request = Box::pin(race_deadline(operation, deadline));
        let mut cx = Context::from_waker(Waker::noop());
        assert!(request.as_mut().poll(&mut cx).is_pending());
        expired.set(true);
        assert!(matches!(
            request.as_mut().poll(&mut cx),
            Poll::Ready(Err(ClientError::Timeout))
        ));
        assert!(pending.slots.borrow().is_empty());
        pending
            .dispatch(event(&subscription_frame("chat-events", "late")))
            .unwrap();
        assert_eq!(*orphans.borrow(), ["late"]);
    }

    #[test]
    fn buffered_response_is_kept_until_consumed_or_its_guard_is_dropped() {
        let pending = Rc::new(PendingRequests::default());
        let orphans = capture_orphans(&pending);
        let (guard, response) = pending
            .register("chat-timeline".into(), ReplyKind::TimelineSubscription)
            .unwrap();
        let frame = subscription_frame("chat-timeline", "buffered");
        pending.dispatch(event(&frame)).unwrap();
        pending.dispatch(event(&frame)).unwrap();
        assert!(orphans.borrow().is_empty());
        drop(response);
        drop(guard);
        assert_eq!(*orphans.borrow(), ["buffered"]);
        assert!(pending.buffered_subscriptions.borrow().is_empty());
    }

    #[test]
    fn duplicate_delivered_responses_never_release_an_active_page_token() {
        let pending = Rc::new(PendingRequests::default());
        let orphans = capture_orphans(&pending);
        let (guard, response) = pending
            .register("chat-events".into(), ReplyKind::SessionSubscription)
            .unwrap();
        let frame = subscription_frame("chat-events", "active");
        pending.dispatch(event(&frame)).unwrap();
        assert!(matches!(
            pin!(response)
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop())),
            Poll::Ready(Ok(_))
        ));
        drop(guard);
        pending.dispatch(event(&frame)).unwrap();
        assert!(orphans.borrow().is_empty());
        assert!(pending.active_subscriptions.borrow().contains("active"));
        let listener =
            pending
                .events
                .register(EventTarget::Agent("agent".into()), Some("active"), |_| {});
        pending.release_token("active");
        pending.dispatch(event(&frame)).unwrap();
        assert!(!pending.active_subscriptions.borrow().contains("active"));
        assert!(!listener.listener.active.get());
        assert!(orphans.borrow().is_empty());
    }

    #[test]
    fn cancelling_one_buffered_response_does_not_release_another_owner_of_the_same_token() {
        let pending = Rc::new(PendingRequests::default());
        let orphans = capture_orphans(&pending);
        let (first_guard, first_response) = pending
            .register("first".into(), ReplyKind::SessionSubscription)
            .unwrap();
        let (_second_guard, second_response) = pending
            .register("second".into(), ReplyKind::SessionSubscription)
            .unwrap();
        let mut frame = subscription_frame("chat-events", "shared");
        frame["message"]["payload"]["requestId"] = json!("first");
        pending.dispatch(event(&frame)).unwrap();
        frame["message"]["payload"]["requestId"] = json!("second");
        pending.dispatch(event(&frame)).unwrap();
        drop(first_response);
        drop(first_guard);
        assert!(orphans.borrow().is_empty());
        assert!(matches!(
            pin!(second_response)
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop())),
            Poll::Ready(Ok(_))
        ));
        pending.dispatch(event(&frame)).unwrap();
        assert!(orphans.borrow().is_empty());
        assert!(pending.active_subscriptions.borrow().contains("shared"));
    }

    #[test]
    fn nested_creation_token_is_reclaimed_and_close_forgets_all_ownership() {
        let pending = Rc::new(PendingRequests::default());
        let orphans = capture_orphans(&pending);
        let mut frame = subscription_frame("chat-observe", "nested");
        frame["message"]["payload"]
            .as_object_mut()
            .unwrap()
            .remove("subscriptionId");
        frame["message"]["payload"]["snapshot"]["subscriptionId"] = json!("nested");
        pending.dispatch(event(&frame)).unwrap();
        assert_eq!(*orphans.borrow(), ["nested"]);
        let (guard, response) = pending
            .register("chat-events".into(), ReplyKind::SessionSubscription)
            .unwrap();
        pending
            .dispatch(event(&subscription_frame("chat-events", "buffered")))
            .unwrap();
        pending.close(ClientError::Closed);
        drop(response);
        drop(guard);
        assert!(pending.orphan_cleanup.borrow().is_none());
        assert!(pending.active_subscriptions.borrow().is_empty());
        assert!(pending.buffered_subscriptions.borrow().is_empty());
        assert!(pending.releasing_subscriptions.borrow().is_empty());
        assert_eq!(*orphans.borrow(), ["nested"]);
    }

    #[test]
    fn chat_and_subscription_replies_use_the_existing_single_reader_dispatch() {
        let pending = Rc::new(PendingRequests::default());
        let mut cx = Context::from_waker(Waker::noop());
        for (id, expected) in [
            ("chat-create", ReplyKind::Created),
            ("chat-observe", ReplyKind::CreationSubscription),
            ("chat-events", ReplyKind::SessionSubscription),
            ("chat-timeline", ReplyKind::TimelineSubscription),
            ("chat-send", ReplyKind::Sent),
            ("chat-rejected", ReplyKind::Sent),
            ("chat-cancel", ReplyKind::Cancelled),
            ("chat-cancel-missing", ReplyKind::Cancelled),
        ] {
            let (_guard, response) = pending.register(id.into(), expected).unwrap();
            let mut response = pin!(response);
            pending.dispatch(event(&conversation_frame(id))).unwrap();
            let Poll::Ready(Ok(reply)) = response.as_mut().poll(&mut cx) else {
                panic!("chat reply not delivered")
            };
            assert_eq!(reply.kind(), expected);
            if id == "chat-rejected" {
                let PendingReply::Sent(sent) = reply else {
                    panic!("not sent")
                };
                assert!(sent.accepted().is_err());
            } else if id == "chat-cancel-missing" {
                let PendingReply::Cancelled(cancelled) = reply else {
                    panic!("not cancel")
                };
                assert!(matches!(
                    cancelled.state(),
                    conversation::CancelState::Failed(_)
                ));
            }
        }
        let timeline: Vec<Value> =
            serde_json::from_str(include_str!("../../protocol-fixtures/timeline.json")).unwrap();
        let (_guard, response) = pending
            .register("fixture-history".into(), ReplyKind::History)
            .unwrap();
        pending
            .dispatch(event(&timeline.last().unwrap()["frame"]))
            .unwrap();
        assert!(matches!(
            pin!(response).as_mut().poll(&mut cx),
            Poll::Ready(Ok(PendingReply::History(_)))
        ));
        let (_guard, response) = pending
            .register("release".into(), ReplyKind::SubscriptionReleased)
            .unwrap();
        pending
            .dispatch(event(
                &json!({"type":"session","message":{"type":"subscription.release.response",
            "payload":{"requestId":"release","subscriptionId":"server-token"}}}),
            ))
            .unwrap();
        assert!(matches!(
            pin!(response).as_mut().poll(&mut cx),
            Poll::Ready(Ok(PendingReply::SubscriptionReleased(_)))
        ));
        assert!(pending.slots.borrow().is_empty());
    }

    #[test]
    fn permission_identity_wrong_reply_kind_and_unknown_updates_never_complete_chat_rpcs() {
        let pending = Rc::new(PendingRequests::default());
        let (_guard, response) = pending
            .register("chat-send".into(), ReplyKind::Sent)
            .unwrap();
        let mut response = pin!(response);
        let mut cx = Context::from_waker(Waker::noop());
        let mut wrong = conversation_frame("chat-cancel");
        wrong["message"]["payload"]["requestId"] = json!("chat-send");
        pending.dispatch(event(&wrong)).unwrap();
        pending.dispatch(event(&json!({"type":"session","message":{"type":"agent_permission_resolved",
            "payload":{"requestId":"chat-send","agentId":"agent","resolution":{"behavior":"allow"}}}}))).unwrap();
        pending
            .dispatch(event(
                &json!({"type":"session","message":{"type":"future.chat.response",
            "payload":{"requestId":"chat-send","accepted":true}}}),
            ))
            .unwrap();
        pending
            .dispatch(event(&stream_frame("agent", None)))
            .unwrap();
        assert!(response.as_mut().poll(&mut cx).is_pending());
        assert_eq!(pending.slots.borrow().len(), 1);
        pending
            .dispatch(event(&conversation_frame("chat-send")))
            .unwrap();
        assert!(matches!(
            response.as_mut().poll(&mut cx),
            Poll::Ready(Ok(PendingReply::Sent(_)))
        ));
    }

    #[test]
    fn cancelled_chat_requests_ignore_late_replies_without_an_old_guard_removing_a_new_slot() {
        let pending = Rc::new(PendingRequests::default());
        let (guard, response) = pending
            .register("chat-send".into(), ReplyKind::Sent)
            .unwrap();
        assert_eq!(
            pending.register("chat-send".into(), ReplyKind::Sent).err(),
            Some(ClientError::InvalidProtocol)
        );
        let mut request = Box::pin(async move {
            let _guard = guard;
            response.await
        });
        let mut cx = Context::from_waker(Waker::noop());
        assert!(request.as_mut().poll(&mut cx).is_pending());
        drop(request);
        pending
            .dispatch(event(&conversation_frame("chat-send")))
            .unwrap();
        assert!(pending.slots.borrow().is_empty());
        let (old_guard, old_response) = pending
            .register("chat-send".into(), ReplyKind::Sent)
            .unwrap();
        pending
            .dispatch(event(&conversation_frame("chat-send")))
            .unwrap();
        assert!(matches!(
            pin!(old_response).as_mut().poll(&mut cx),
            Poll::Ready(Ok(_))
        ));
        let (_new_guard, new_response) = pending
            .register("chat-send".into(), ReplyKind::Sent)
            .unwrap();
        drop(old_guard);
        assert_eq!(pending.slots.borrow().len(), 1);
        pending
            .dispatch(event(&conversation_frame("chat-send")))
            .unwrap();
        assert!(matches!(
            pin!(new_response).as_mut().poll(&mut cx),
            Poll::Ready(Ok(_))
        ));
    }

    #[test]
    fn correlated_rpc_errors_are_failures_and_cannot_reject_a_different_operation() {
        let pending = Rc::new(PendingRequests::default());
        let (_guard, response) = pending
            .register("chat-send".into(), ReplyKind::Sent)
            .unwrap();
        let mut response = pin!(response);
        let mut cx = Context::from_waker(Waker::noop());
        let mut frame = json!({"type":"session","message":{"type":"rpc_error","payload":{
            "requestId":"chat-send","requestType":"cancel_agent_request","error":"Provider refused","code":"denied"}}});
        pending.dispatch(event(&frame)).unwrap();
        assert!(response.as_mut().poll(&mut cx).is_pending());
        frame["message"]["payload"]["requestType"] = json!("send_agent_message_request");
        pending.dispatch(event(&frame)).unwrap();
        let Poll::Ready(Ok(PendingReply::RpcError(error))) = response.as_mut().poll(&mut cx) else {
            panic!("not RPC failure")
        };
        let error = ConversationError::Rpc(error);
        assert_eq!(error.to_string(), "Provider refused");
        assert!(!format!("{error:?}").contains("Provider refused"));
        let (_guard, response) = pending
            .register("chat-send".into(), ReplyKind::Sent)
            .unwrap();
        frame["message"]["payload"]
            .as_object_mut()
            .unwrap()
            .remove("requestType");
        pending.dispatch(event(&frame)).unwrap();
        assert!(matches!(
            pin!(response).as_mut().poll(&mut cx),
            Poll::Ready(Ok(PendingReply::RpcError(_)))
        ));
    }

    #[test]
    fn local_listeners_filter_agent_and_subscription_and_stop_before_server_release() {
        let pending = Rc::new(PendingRequests::default());
        let seen = Rc::new(Cell::new(0));
        let captured = seen.clone();
        let listener =
            pending
                .events
                .register(EventTarget::Agent("agent-a".into()), None, move |_| {
                    captured.set(captured.get() + 1)
                });
        pending
            .dispatch(event(&stream_frame("agent-b", None)))
            .unwrap();
        pending
            .dispatch(event(&stream_frame("agent-a", Some("unknown-token"))))
            .unwrap();
        assert_eq!(seen.get(), 0);
        // Legacy events can arrive before a subscription acknowledgement, without tags.
        pending
            .dispatch(event(&stream_frame("agent-a", None)))
            .unwrap();
        listener
            .set_subscription_id(Some("token-a".into()))
            .unwrap();
        pending
            .dispatch(event(&stream_frame("agent-a", Some("token-b"))))
            .unwrap();
        pending
            .dispatch(event(&stream_frame("agent-a", Some("token-a"))))
            .unwrap();
        pending
            .dispatch(event(&stream_frame("agent-a", None)))
            .unwrap();
        assert_eq!(seen.get(), 3);
        pending.events.release_token("token-b");
        assert_eq!(pending.events.listeners.borrow().len(), 1);
        pending.events.release_token("token-a");
        pending
            .dispatch(event(&stream_frame("agent-a", None)))
            .unwrap();
        assert_eq!(seen.get(), 3);
        assert_eq!(listener.set_subscription_id(None), Err(ClientError::Closed));
        assert!(pending.events.listeners.borrow().is_empty());
    }

    #[test]
    fn creation_listeners_receive_early_updates_by_key_and_kind_and_drop_locally() {
        let pending = Rc::new(PendingRequests::default());
        let seen = Rc::new(Cell::new(0));
        let captured = seen.clone();
        let listener = pending.events.register(
            EventTarget::Creation {
                kind: CreationKind::Agent,
                key: "conversation-create".into(),
            },
            None,
            move |_| captured.set(captured.get() + 1),
        );
        let mut update = conversation_capture()
            .into_iter()
            .find(|entry| entry["frame"]["message"]["type"] == "agent.create.update")
            .unwrap()["frame"]
            .clone();
        pending.dispatch(event(&update)).unwrap();
        assert_eq!(seen.get(), 1);
        update["message"]["payload"]["idempotencyKey"] = json!("different-create");
        pending.dispatch(event(&update)).unwrap();
        update["message"]["payload"]["idempotencyKey"] = json!("conversation-create");
        update["message"]["payload"]["kind"] = json!("workspace");
        pending.dispatch(event(&update)).unwrap();
        assert_eq!(seen.get(), 1);
        update["message"]["payload"]["kind"] = json!("agent");
        drop(listener);
        pending.dispatch(event(&update)).unwrap();
        assert_eq!(seen.get(), 1);
        assert!(pending.events.listeners.borrow().is_empty());
    }

    #[test]
    fn event_callbacks_can_unregister_reentrantly_and_connection_failure_stops_all_delivery() {
        let pending = Rc::new(PendingRequests::default());
        let handle = Rc::new(RefCell::new(None::<EventRegistration>));
        let captured_handle = handle.clone();
        let seen = Rc::new(Cell::new(0));
        let captured_seen = seen.clone();
        *handle.borrow_mut() = Some(pending.events.register(
            EventTarget::Agent("agent".into()),
            None,
            move |_| {
                captured_seen.set(captured_seen.get() + 1);
                captured_handle.borrow_mut().take();
            },
        ));
        pending
            .dispatch(event(&stream_frame("agent", None)))
            .unwrap();
        pending
            .dispatch(event(&stream_frame("agent", None)))
            .unwrap();
        assert_eq!(seen.get(), 1);
        let listener = pending
            .events
            .register(EventTarget::Agent("agent".into()), None, |_| {
                panic!("closed callback invoked")
            });
        pending.close(ClientError::Closed);
        assert_eq!(
            pending.dispatch(event(&stream_frame("agent", None))),
            Err(ClientError::Closed)
        );
        assert_eq!(listener.set_subscription_id(None), Err(ClientError::Closed));
        assert!(pending.events.listeners.borrow().is_empty());
    }

    #[test]
    fn catalog_fixtures_dispatch_only_to_the_registered_response_type() {
        let pending = Rc::new(PendingRequests::default());
        let mut cx = Context::from_waker(Waker::noop());
        for (id, expected) in [
            ("catalog-providers", ReplyKind::Providers),
            ("catalog-features", ReplyKind::Features),
            ("catalog-workspaces", ReplyKind::Workspaces),
            ("catalog-agents", ReplyKind::Agents),
            ("catalog-agent", ReplyKind::Agent),
            ("catalog-missing-agent", ReplyKind::Agent),
        ] {
            let (_guard, reply) = pending.register(id.to_owned(), expected).unwrap();
            let mut reply = pin!(reply);
            assert!(reply.as_mut().poll(&mut cx).is_pending());
            assert!(
                handshake_event(event(&catalog_frame(id)))
                    .unwrap()
                    .is_none()
            );
            pending.dispatch(event(&catalog_frame(id))).unwrap();
            let Poll::Ready(Ok(reply)) = reply.as_mut().poll(&mut cx) else {
                panic!("catalog reply not delivered")
            };
            assert_eq!(reply.kind(), expected);
            assert_eq!(reply.request_id(), id);
            if id == "catalog-missing-agent" {
                let PendingReply::Agent(reply) = reply else {
                    panic!("wrong reply kind")
                };
                assert!(reply.agent.is_none());
                assert!(reply.error.is_none());
            }
        }
        assert!(pending.slots.borrow().is_empty());
    }

    #[test]
    fn matching_request_id_with_the_wrong_response_type_does_not_complete_or_remove_the_slot() {
        let pending = Rc::new(PendingRequests::default());
        let (_guard, reply) = pending
            .register("request".to_owned(), ReplyKind::Workspaces)
            .unwrap();
        let mut reply = pin!(reply);
        let mut cx = Context::from_waker(Waker::noop());
        let mut wrong = catalog_frame("catalog-agents");
        wrong["message"]["payload"]["requestId"] = json!("request");
        pending.dispatch(event(&wrong)).unwrap();
        assert!(reply.as_mut().poll(&mut cx).is_pending());
        assert_eq!(pending.slots.borrow().len(), 1);
        let mut correct = catalog_frame("catalog-workspaces");
        correct["message"]["payload"]["requestId"] = json!("request");
        pending.dispatch(event(&correct)).unwrap();
        assert!(matches!(
            reply.as_mut().poll(&mut cx),
            Poll::Ready(Ok(PendingReply::Workspaces(_)))
        ));
        assert!(pending.slots.borrow().is_empty());
    }

    #[test]
    fn a_cancelled_catalog_request_ignores_late_replies_and_preserves_server_error_fields() {
        let pending = Rc::new(PendingRequests::default());
        let (guard, response) = pending
            .register("catalog-providers".to_owned(), ReplyKind::Providers)
            .unwrap();
        let mut cancelled = Box::pin(async move {
            let _guard = guard;
            response.await
        });
        let mut cx = Context::from_waker(Waker::noop());
        assert!(cancelled.as_mut().poll(&mut cx).is_pending());
        drop(cancelled);
        pending
            .dispatch(event(&catalog_frame("catalog-providers")))
            .unwrap();
        assert!(pending.slots.borrow().is_empty());

        let (_guard, response) = pending
            .register("catalog-features".to_owned(), ReplyKind::Features)
            .unwrap();
        let mut response = pin!(response);
        let mut error = catalog_frame("catalog-features");
        error["message"]["payload"]
            .as_object_mut()
            .unwrap()
            .remove("features");
        error["message"]["payload"]["error"] = json!("Provider reported a catalog failure");
        pending.dispatch(event(&error)).unwrap();
        let Poll::Ready(Ok(PendingReply::Features(features))) = response.as_mut().poll(&mut cx)
        else {
            panic!("feature response not delivered")
        };
        assert!(features.features.is_none());
        assert!(
            matches!(features.error, catalog::Field::Value(error) if error == "Provider reported a catalog failure")
        );
    }

    #[test]
    fn only_a_compatible_server_info_completes_the_handshake() {
        let mut frame = fixtures()[1]["frame"].clone();
        let info = handshake_event(event(&frame)).unwrap().unwrap();
        assert_eq!(info.server_id, "fixture-server");
        for version in [json!(2), Value::Null] {
            if version.is_null() {
                frame["message"]["payload"]
                    .as_object_mut()
                    .unwrap()
                    .remove("protocolVersion");
            } else {
                frame["message"]["payload"]["protocolVersion"] = version;
            }
            assert_eq!(
                handshake_event(event(&frame)).err(),
                Some(ClientError::InvalidProtocol)
            );
        }
        assert!(
            handshake_event(event(&json!({"type":"pong"})))
                .unwrap()
                .is_none()
        );
        assert!(
            handshake_event(event(&json!({"type":"future.event"})))
                .unwrap()
                .is_none()
        );
        assert!(
            handshake_event(event(&fixtures()[3]["frame"]))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn projects_accepts_only_the_requested_response_identity() {
        let pending = Rc::new(PendingRequests::default());
        let (_guard, response) = pending
            .register("projects-populated".to_owned(), ReplyKind::Projects)
            .unwrap();
        let mut response = pin!(response);
        let mut cx = Context::from_waker(Waker::noop());
        let mut unrelated = fixtures()[5]["frame"].clone();
        unrelated["message"]["payload"]["requestId"] = json!("another-request");
        pending.dispatch(event(&unrelated)).unwrap();
        pending.dispatch(event(&json!({"type":"session","message":{"type":"future.update","payload":{"requestId":"projects-populated"}}}))).unwrap();
        assert!(response.as_mut().poll(&mut cx).is_pending());
        let frame = &fixtures()[5]["frame"];
        pending.dispatch(event(frame)).unwrap();
        let Poll::Ready(Ok(PendingReply::Projects(projects))) = response.as_mut().poll(&mut cx)
        else {
            panic!("matching response not delivered")
        };
        assert_eq!(projects.projects[0].project_id, "fixture-project");
        assert!(pending.slots.borrow().is_empty());
    }

    #[test]
    fn cancelling_or_timing_out_a_request_removes_its_slot_and_ignores_late_responses() {
        let pending = Rc::new(PendingRequests::default());
        let (guard, response) = pending
            .register("cancelled".to_owned(), ReplyKind::Projects)
            .unwrap();
        let mut cancelled = Box::pin(async move {
            let _guard = guard;
            response.await
        });
        let mut cx = Context::from_waker(Waker::noop());
        assert!(cancelled.as_mut().poll(&mut cx).is_pending());
        drop(cancelled);
        assert!(pending.slots.borrow().is_empty());

        let (guard, response) = pending
            .register("expired".to_owned(), ReplyKind::Projects)
            .unwrap();
        let expired = Cell::new(false);
        let deadline = std::future::poll_fn(|_| {
            if expired.get() {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        });
        let mut request = Box::pin(race_deadline(
            async move {
                let _guard = guard;
                response.await
            },
            deadline,
        ));
        assert!(request.as_mut().poll(&mut cx).is_pending());
        expired.set(true);
        assert!(matches!(
            request.as_mut().poll(&mut cx),
            Poll::Ready(Err(ClientError::Timeout))
        ));
        assert!(pending.slots.borrow().is_empty());
        assert_eq!(pending.failure.get(), None);

        let (_guard, response) = pending
            .register("projects-populated".to_owned(), ReplyKind::Projects)
            .unwrap();
        let mut response = pin!(response);
        let mut late = fixtures()[5]["frame"].clone();
        late["message"]["payload"]["requestId"] = json!("expired");
        pending.dispatch(event(&late)).unwrap();
        assert!(response.as_mut().poll(&mut cx).is_pending());
        pending.dispatch(event(&fixtures()[5]["frame"])).unwrap();
        assert!(matches!(
            response.as_mut().poll(&mut cx),
            Poll::Ready(Ok(_))
        ));
    }

    #[test]
    fn terminal_failure_notifies_once_and_finishes_all_pending_requests() {
        let pending = Rc::new(PendingRequests::default());
        let (_first, first) = pending
            .register("first".to_owned(), ReplyKind::Projects)
            .unwrap();
        let (_second, second) = pending
            .register("second".to_owned(), ReplyKind::Projects)
            .unwrap();
        let notices = Rc::new(Cell::new(0));
        let callback_notices = notices.clone();
        let callback: Rc<dyn Fn(ClientError)> =
            Rc::new(move |_| callback_notices.set(callback_notices.get() + 1));
        fail_connection(&pending, &callback, ClientError::Closed);
        fail_connection(&pending, &callback, ClientError::Transport);
        let mut cx = Context::from_waker(Waker::noop());
        assert!(matches!(
            pin!(first).as_mut().poll(&mut cx),
            Poll::Ready(Err(ClientError::Closed))
        ));
        assert!(matches!(
            pin!(second).as_mut().poll(&mut cx),
            Poll::Ready(Err(ClientError::Closed))
        ));
        assert_eq!(notices.get(), 1);
        assert!(pending.slots.borrow().is_empty());
        assert_eq!(
            pending
                .register("after-close".to_owned(), ReplyKind::Projects)
                .err(),
            Some(ClientError::Closed)
        );
    }

    #[test]
    fn auth_failures_have_safe_chinese_messages_and_stable_codes() {
        for (reason, expected) in [
            ("password_required", ClientError::PasswordRequired),
            ("incorrect_password", ClientError::IncorrectPassword),
            ("incompatible_protocol", ClientError::InvalidProtocol),
        ] {
            let frame = json!({"type":"hello.rejected","reason":reason,"accepts":["password"],"url":"wss://secret?password=secret"});
            let failure = handshake_event(event(&frame)).err().unwrap();
            assert_eq!(failure, expected);
            assert!(!failure.to_string().contains("secret"));
            assert!(!format!("{failure:?}").contains("secret"));
            assert!(!failure.code().is_empty());
        }
        assert_eq!(
            ClientError::IncorrectPassword.to_string(),
            "主机密码不正确，请修改后重新连接"
        );
    }

    #[test]
    fn the_deadline_wins_over_a_pending_or_simultaneously_ready_operation() {
        let mut cx = Context::from_waker(Waker::noop());
        let mut pending = pin!(race_deadline(
            future::pending::<Result<(), ClientError>>(),
            future::ready(())
        ));
        assert_eq!(
            pending.as_mut().poll(&mut cx),
            Poll::Ready(Err(ClientError::Timeout))
        );
        let mut simultaneous = pin!(race_deadline(future::ready(Ok(())), future::ready(())));
        assert_eq!(
            simultaneous.as_mut().poll(&mut cx),
            Poll::Ready(Err(ClientError::Timeout))
        );
        let mut success = pin!(race_deadline(future::ready(Ok(42)), future::pending()));
        assert_eq!(success.as_mut().poll(&mut cx), Poll::Ready(Ok(42)));
    }
}
