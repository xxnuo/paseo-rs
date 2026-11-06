//! Direct, sequential RPC client; call it inside `nsh_core::element::spawn`.

use crate::host::ParsedHost;
use crate::protocol::{self, HelloRejection, ProjectList, ServerEvent, ServerInfo};
use nsh_core::{
    element::timer,
    websocket::{WebSocket, WebSocketData},
};
use std::{fmt, future::Future, pin::pin, task::Poll, time::Duration};

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
    socket: Option<WebSocket>,
    server_info: ServerInfo,
}

impl Client {
    pub(crate) async fn connect(host: ParsedHost, client_id: &str) -> Result<Self, ClientError> {
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
                    return Ok(Self {
                        socket: Some(socket),
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

    pub(crate) async fn projects(&mut self, request_id: &str) -> Result<ProjectList, ClientError> {
        let result = with_timeout(async {
            let socket = self.socket.as_ref().ok_or(ClientError::Closed)?;
            socket
                .write(WebSocketData::Text(protocol::list_projects(request_id)))
                .await
                .map_err(|_| ClientError::Transport)?;
            loop {
                if let Some(projects) = projects_event(read_event(socket).await?, request_id)? {
                    return Ok(projects);
                }
            }
        })
        .await;
        // A timed-out read/RPC leaves uncertain transport state; dropping cancels its I/O.
        if result.is_err() {
            self.socket.take();
        }
        result
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

fn projects_event(
    event: ServerEvent,
    request_id: &str,
) -> Result<Option<ProjectList>, ClientError> {
    match event {
        ServerEvent::Projects(projects) if projects.request_id == request_id => Ok(Some(projects)),
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
        let frame = &fixtures()[5]["frame"];
        assert!(
            projects_event(event(frame), "another-request")
                .unwrap()
                .is_none()
        );
        let projects = projects_event(event(frame), "projects-populated")
            .unwrap()
            .unwrap();
        assert_eq!(projects.projects[0].project_id, "fixture-project");
        assert!(
            projects_event(event(&json!({"type":"future.event"})), "projects-populated")
                .unwrap()
                .is_none()
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
