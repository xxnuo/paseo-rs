//! Direct WebSocket protocol projection; wire authority is packages/protocol/src/messages.ts.

use serde::{Deserialize, Deserializer, de::DeserializeOwned};
use serde_json::{Value, json};
use std::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ProtocolDiagnostic {
    InvalidClientId,
    MalformedJson,
    MalformedMessage,
}

#[derive(Debug)]
pub(crate) struct ProtocolError {
    pub(crate) diagnostic: ProtocolDiagnostic,
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self.diagnostic {
            ProtocolDiagnostic::InvalidClientId => "The client identity is empty",
            ProtocolDiagnostic::MalformedJson | ProtocolDiagnostic::MalformedMessage => {
                "The daemon sent an invalid message"
            }
        })
    }
}

impl std::error::Error for ProtocolError {}

fn malformed() -> ProtocolError {
    ProtocolError {
        diagnostic: ProtocolDiagnostic::MalformedMessage,
    }
}

fn decode<T: DeserializeOwned>(value: Value) -> Result<T, ProtocolError> {
    serde_json::from_value(value).map_err(|_| malformed())
}

pub(crate) fn hello(client_id: &str, password: Option<&str>) -> Result<String, ProtocolError> {
    let client_id = client_id.trim();
    if client_id.is_empty() {
        return Err(ProtocolError {
            diagnostic: ProtocolDiagnostic::InvalidClientId,
        });
    }
    // Advertise only implemented capabilities: binary chunking and subscriptions are not implied.
    let mut frame = json!({
        "type": "hello",
        "clientId": client_id,
        "clientType": "browser",
        "protocolVersion": 1,
        "capabilities": { "hello_rejection": true }
    });
    if let Some(password) = password {
        frame["auth"] = json!({ "kind": "password", "password": password });
    }
    Ok(frame.to_string())
}

pub(crate) fn list_projects(request_id: &str) -> String {
    json!({
        "type": "session",
        "message": { "type": "project.list.request", "requestId": request_id }
    })
    .to_string()
}

// No Debug: an unknown frame may contain credentials and must never be dumped into logs or UI.
#[allow(dead_code)] // Preserve unhandled frames without exposing them in diagnostics.
pub(crate) enum ServerEvent {
    ServerInfo(ServerInfo),
    Projects(ProjectList),
    HelloRejected(HelloRejection),
    Pong,
    Unknown(Value),
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)] // Validate known wire fields even when this screen consumes only a subset.
pub(crate) struct ServerInfo {
    #[serde(deserialize_with = "nonempty_string")]
    pub(crate) server_id: String,
    #[serde(default, deserialize_with = "present")]
    pub(crate) protocol_version: Option<i64>,
    #[serde(default, deserialize_with = "metadata_text")]
    pub(crate) hostname: Option<String>,
    #[serde(default, deserialize_with = "metadata_text")]
    pub(crate) version: Option<String>,
    #[serde(default, deserialize_with = "present")]
    pub(crate) features: Option<serde_json::Map<String, Value>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ProjectList {
    pub(crate) request_id: String,
    pub(crate) projects: Vec<Project>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)] // Validate known wire fields even when this screen consumes only a subset.
pub(crate) struct Project {
    pub(crate) project_id: String,
    pub(crate) project_display_name: String,
    pub(crate) project_root_path: String,
    pub(crate) project_kind: ProjectKind,
    #[serde(default, deserialize_with = "present")]
    pub(crate) project_key: Option<String>,
    pub(crate) project_custom_name: Option<String>,
    pub(crate) project_custom_icon_revision: Option<String>,
    #[serde(default, deserialize_with = "present")]
    pub(crate) project_icon_revision: Option<String>,
}

#[derive(Deserialize, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ProjectKind {
    Git,
    NonGit,
    Directory,
}

#[derive(Deserialize, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum HelloRejection {
    PasswordRequired,
    IncorrectPassword,
    IncompatibleProtocol,
}

impl fmt::Display for HelloRejection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::PasswordRequired => "This host requires a password",
            Self::IncorrectPassword => "The host password is incorrect",
            Self::IncompatibleProtocol => "The host protocol version is incompatible",
        })
    }
}

#[derive(Deserialize)]
enum AcceptedAuth {
    #[serde(rename = "password")]
    Password,
}

#[derive(Deserialize)]
struct Rejected {
    reason: HelloRejection,
    #[serde(rename = "accepts")]
    _accepts: Vec<AcceptedAuth>,
}

#[derive(Deserialize)]
struct Envelope {
    #[serde(rename = "type")]
    kind: String,
    message: Option<Value>,
}

#[derive(Deserialize)]
struct Message {
    #[serde(rename = "type")]
    kind: String,
    payload: Option<Value>,
}

#[derive(Deserialize)]
struct Status {
    status: String,
}

pub(crate) fn parse_server_event(text: &str) -> Result<ServerEvent, ProtocolError> {
    let frame: Value = serde_json::from_str(text).map_err(|_| ProtocolError {
        diagnostic: ProtocolDiagnostic::MalformedJson,
    })?;
    let envelope: Envelope = decode(frame.clone())?;
    match envelope.kind.as_str() {
        "pong" => Ok(ServerEvent::Pong),
        "hello.rejected" => {
            let rejected: Rejected = decode(frame)?;
            Ok(ServerEvent::HelloRejected(rejected.reason))
        }
        "session" => {
            let message: Message = decode(envelope.message.ok_or_else(malformed)?)?;
            match message.kind.as_str() {
                "status" => {
                    let payload = message.payload.ok_or_else(malformed)?;
                    let status: Status = decode(payload.clone())?;
                    if status.status == "server_info" {
                        Ok(ServerEvent::ServerInfo(decode(payload)?))
                    } else {
                        Ok(ServerEvent::Unknown(frame))
                    }
                }
                "project.list.response" => Ok(ServerEvent::Projects(decode(
                    message.payload.ok_or_else(malformed)?,
                )?)),
                _ => Ok(ServerEvent::Unknown(frame)),
            }
        }
        _ => Ok(ServerEvent::Unknown(frame)),
    }
}

// Zod .optional() allows absence, but present null is invalid unless the field is also .nullable().
fn present<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}

fn nonempty_string<'de, D: Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    let text = String::deserialize(deserializer)?;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err(serde::de::Error::custom("empty server identity"));
    }
    Ok(trimmed.to_owned())
}

// ServerInfoHostnameSchema/ServerInfoVersionSchema normalize missing, non-string and blank to null.
fn metadata_text<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<String>, D::Error> {
    let value = Value::deserialize(deserializer)?;
    Ok(value
        .as_str()
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_owned))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixtures() -> Vec<Value> {
        serde_json::from_str(include_str!("../../protocol-fixtures/direct-session.json")).unwrap()
    }

    fn parsed(value: &Value) -> ServerEvent {
        parse_server_event(&value.to_string()).unwrap()
    }

    #[test]
    fn minimal_hello_does_not_advertise_unimplemented_sdk_capabilities() {
        let wire: Value = serde_json::from_str(&hello("rust-client", None).unwrap()).unwrap();
        assert_eq!(
            wire,
            json!({"type":"hello","clientId":"rust-client","clientType":"browser",
            "protocolVersion":1,"capabilities":{"hello_rejection":true}})
        );
        let wire: Value =
            serde_json::from_str(&hello("rust-client", Some("secret")).unwrap()).unwrap();
        assert_eq!(wire["auth"], json!({"kind":"password","password":"secret"}));
        assert!(hello("  ", None).is_err());
    }

    #[test]
    fn parses_real_sdk_daemon_handshake_and_project_fixtures() {
        let capture = fixtures();
        let ServerEvent::ServerInfo(info) = parsed(&capture[1]["frame"]) else {
            panic!("not server_info")
        };
        assert_eq!(info.server_id, "fixture-server");
        assert_eq!(info.protocol_version, Some(1));
        assert_eq!(info.hostname.as_deref(), Some("fixture-host"));
        assert_eq!(info.version.as_deref(), Some("0.15.2"));
        assert_eq!(info.features.unwrap()["projectList"], true);
        let request: Value = serde_json::from_str(&list_projects("projects-empty")).unwrap();
        assert_eq!(request, capture[2]["frame"]);
        let ServerEvent::Projects(empty) = parsed(&capture[3]["frame"]) else {
            panic!("not projects")
        };
        assert_eq!(empty.request_id, "projects-empty");
        assert!(empty.projects.is_empty());
        let ServerEvent::Projects(populated) = parsed(&capture[5]["frame"]) else {
            panic!("not projects")
        };
        assert_eq!(populated.request_id, "projects-populated");
        let project = &populated.projects[0];
        assert_eq!(project.project_id, "fixture-project");
        assert_eq!(project.project_root_path, "/fixture/project");
        assert_eq!(project.project_kind, ProjectKind::NonGit);
        assert_eq!(project.project_display_name, "project");
        assert_eq!(project.project_key.as_deref(), Some("fixture-project-key"));
        assert_eq!(project.project_custom_name, None);
        assert_eq!(project.project_custom_icon_revision, None);
        assert_eq!(
            project.project_icon_revision.as_deref(),
            Some("automatic:none:v1")
        );
    }

    #[test]
    fn future_events_do_not_complete_a_handshake() {
        for frame in [
            json!({"type":"future.event","token":"secret"}),
            json!({"type":"session","message":{"type":"future.response","payload":{}}}),
            json!({"type":"session","message":{"type":"status","payload":{"status":"ready"}}}),
        ] {
            let ServerEvent::Unknown(retained) = parsed(&frame) else {
                panic!("not unknown")
            };
            assert_eq!(retained, frame);
        }
        assert!(matches!(parsed(&json!({"type":"pong"})), ServerEvent::Pong));
    }

    #[test]
    fn missing_null_and_unknown_fields_follow_the_projection_schema() {
        let minimal = json!({"type":"session","message":{"type":"status","payload":
            {"status":"server_info","serverId":" fixture-server ","future":"ignored","hostname":42}}});
        let ServerEvent::ServerInfo(info) = parsed(&minimal) else {
            panic!("not server_info")
        };
        assert_eq!(info.server_id, "fixture-server");
        assert_eq!(info.protocol_version, None);
        assert_eq!(info.hostname, None);
        assert_eq!(info.version, None);
        for payload in [
            json!({"status":"server_info"}),
            json!({"status":"server_info","serverId":null}),
            json!({"status":"server_info","serverId":" "}),
            json!({"status":"server_info","serverId":"server","protocolVersion":null}),
        ] {
            assert!(
                parse_server_event(
                    &json!({"type":"session","message":{"type":"status","payload":payload}})
                        .to_string()
                )
                .is_err()
            );
        }
        let mut response = fixtures()[5]["frame"].clone();
        response["message"]["payload"]["projects"][0]["projectKey"] = Value::Null;
        assert!(parse_server_event(&response.to_string()).is_err());
        assert!(parse_server_event(r#"{"type":"session"}"#).is_err());
    }

    #[test]
    fn auth_rejections_and_invalid_messages_never_echo_credentials() {
        let frame = json!({"type":"hello.rejected","reason":"incorrect_password","accepts":["password"],"secret":"do-not-log"});
        let ServerEvent::HelloRejected(reason) = parsed(&frame) else {
            panic!("not rejection")
        };
        assert_eq!(reason, HelloRejection::IncorrectPassword);
        assert_eq!(reason.to_string(), "The host password is incorrect");
        let error =
            parse_server_event(r#"{"type":"hello.rejected","reason":"do-not-log","accepts":[]}"#)
                .err()
                .unwrap();
        assert_eq!(error.diagnostic, ProtocolDiagnostic::MalformedMessage);
        assert_eq!(error.to_string(), "The daemon sent an invalid message");
        assert!(!format!("{error:?}").contains("do-not-log"));
    }
}
