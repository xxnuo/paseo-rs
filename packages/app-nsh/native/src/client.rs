//! Direct RPC client with one root-owned WebSocket reader.

use crate::host::ParsedHost;
use crate::protocol::catalog::{
    self, AgentResult, Agents, CatalogResponse, ProviderFeatures, Providers, Workspaces,
};
use crate::protocol::{self, HelloRejection, ProjectList, ServerEvent, ServerInfo};
use nsh_core::{
    element::{Spawned, spawn, timer},
    websocket::{WebSocket, WebSocketData},
};
use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
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
        })
    }
}

impl std::error::Error for ClientError {}

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
            _ => Err(ClientError::InvalidProtocol),
        }
    }

    async fn request(
        &self,
        expected: ReplyKind,
        request_id: &str,
        wire: String,
    ) -> Result<PendingReply, ClientError> {
        let (guard, reply) = self.pending.register(request_id.to_owned(), expected)?;
        let result = with_timeout(async {
            if self.socket.write(WebSocketData::Text(wire)).await.is_err() {
                fail_connection(&self.pending, &self.on_state, ClientError::Transport);
                return Err(ClientError::Transport);
            }
            reply.await
        })
        .await;
        drop(guard);
        result
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        self.pending.close(ClientError::Closed);
        if let Some(reader) = self.reader.take() {
            reader.cancel();
        }
    }
}

#[derive(Default)]
struct PendingRequests {
    slots: RefCell<HashMap<String, Rc<RefCell<PendingSlot>>>>,
    failure: Cell<Option<ClientError>>,
}

struct PendingSlot {
    expected: ReplyKind,
    result: Option<Result<PendingReply, ClientError>>,
    waker: Option<Waker>,
}

struct PendingGuard {
    pending: Rc<PendingRequests>,
    request_id: String,
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        self.pending.slots.borrow_mut().remove(&self.request_id);
    }
}

struct Response(Rc<RefCell<PendingSlot>>);

impl Future for Response {
    type Output = Result<PendingReply, ClientError>;

    fn poll(self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<Self::Output> {
        let mut reply = self.0.borrow_mut();
        match reply.result.take() {
            Some(result) => Poll::Ready(result),
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
            },
            Response(slot),
        ))
    }

    fn dispatch(&self, event: ServerEvent) -> Result<(), ClientError> {
        let reply = match event {
            ServerEvent::Projects(value) => PendingReply::Projects(value),
            ServerEvent::Catalog(value) => match value {
                CatalogResponse::Providers(value) => PendingReply::Providers(value),
                CatalogResponse::ProviderFeatures(value) => PendingReply::Features(value),
                CatalogResponse::Workspaces(value) => PendingReply::Workspaces(value),
                CatalogResponse::Agents(value) => PendingReply::Agents(value),
                CatalogResponse::Agent(value) => PendingReply::Agent(value),
            },
            ServerEvent::HelloRejected(reason) => return Err(reason.into()),
            // No event subscriptions are advertised yet. Unknown frames never resolve RPC slots.
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
            deliver(&slot, Ok(reply));
        }
        Ok(())
    }

    fn close(&self, error: ClientError) -> bool {
        if self.failure.get().is_some() {
            return false;
        }
        self.failure.set(Some(error));
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
}

enum PendingReply {
    Projects(ProjectList),
    Providers(Providers),
    Features(ProviderFeatures),
    Workspaces(Workspaces),
    Agents(Agents),
    Agent(AgentResult),
}

impl PendingReply {
    fn kind(&self) -> ReplyKind {
        match self {
            Self::Projects(_) => ReplyKind::Projects,
            Self::Providers(_) => ReplyKind::Providers,
            Self::Features(_) => ReplyKind::Features,
            Self::Workspaces(_) => ReplyKind::Workspaces,
            Self::Agents(_) => ReplyKind::Agents,
            Self::Agent(_) => ReplyKind::Agent,
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
