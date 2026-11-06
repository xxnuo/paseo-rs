//! Catalog projection of messages.ts; identifiers stay open strings and requests are not subscriptions.

use super::{ProjectKind, ProtocolError, decode, malformed, present};
use serde::{Deserialize, Deserializer};
use serde_json::{Value, json};

fn session(message: Value) -> String {
    json!({"type":"session","message":message}).to_string()
}

pub(crate) fn providers(request_id: &str, cwd: &str) -> String {
    session(json!({"type":"get_providers_snapshot_request","requestId":request_id,"cwd":cwd}))
}

pub(crate) fn provider_features(request_id: &str, provider: &str, cwd: &str) -> String {
    session(
        json!({"type":"list_provider_features_request","requestId":request_id,
        "draftConfig":{"provider":provider,"cwd":cwd}}),
    )
}

fn page(cursor: Option<&str>) -> Result<Value, ProtocolError> {
    let mut page = json!({"limit":50});
    if let Some(cursor) = cursor {
        if cursor.is_empty() {
            return Err(malformed());
        }
        page["cursor"] = json!(cursor);
    }
    Ok(page)
}

pub(crate) fn workspaces_for_project(
    request_id: &str,
    project_id: &str,
    cursor: Option<&str>,
) -> Result<String, ProtocolError> {
    Ok(session(
        json!({"type":"fetch_workspaces_request","requestId":request_id,
        "filter":{"projectId":project_id},"page":page(cursor)?}),
    ))
}

pub(crate) fn agents_for_project(
    request_id: &str,
    project_id: &str,
    cursor: Option<&str>,
) -> Result<String, ProtocolError> {
    // The wire calls these projectKeys, but the agent directory matches projectId (catalog.json).
    Ok(session(
        json!({"type":"fetch_agents_request","requestId":request_id,
        "filter":{"projectKeys":[project_id]},"page":page(cursor)?}),
    ))
}

pub(crate) fn agent(request_id: &str, agent_id: &str) -> String {
    session(json!({"type":"fetch_agent_request","requestId":request_id,"agentId":agent_id}))
}

pub(crate) enum CatalogResponse {
    Providers(Providers),
    ProviderFeatures(ProviderFeatures),
    Workspaces(Workspaces),
    Agents(Agents),
    Agent(AgentResult),
}

impl CatalogResponse {
    pub(crate) fn request_id(&self) -> &str {
        match self {
            Self::Providers(value) => &value.request_id,
            Self::ProviderFeatures(value) => &value.request_id,
            Self::Workspaces(value) => &value.request_id,
            Self::Agents(value) => &value.request_id,
            Self::Agent(value) => &value.request_id,
        }
    }
}

pub(crate) fn parse_response(
    kind: &str,
    payload: Value,
) -> Result<Option<CatalogResponse>, ProtocolError> {
    let response = match kind {
        "get_providers_snapshot_response" => {
            // Our hello does not negotiate compact snapshots or conditional snapshot references.
            if payload.get("compactSnapshot").is_some()
                || payload.get("notModified") == Some(&Value::Bool(true))
            {
                return Err(malformed());
            }
            CatalogResponse::Providers(decode(payload)?)
        }
        "list_provider_features_response" => CatalogResponse::ProviderFeatures(decode(payload)?),
        "fetch_workspaces_response" => CatalogResponse::Workspaces(decode(payload)?),
        "fetch_agents_response" => CatalogResponse::Agents(decode(payload)?),
        "fetch_agent_response" => CatalogResponse::Agent(decode(payload)?),
        _ => return Ok(None),
    };
    Ok(Some(response))
}

// Optional nullable fields have three wire states; None alone would erase missing versus null.
#[derive(PartialEq)]
pub(crate) enum Field<T> {
    Missing,
    Null,
    Value(T),
}

impl<T> Default for Field<T> {
    fn default() -> Self {
        Self::Missing
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Field<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(match Option::<T>::deserialize(deserializer)? {
            Some(value) => Self::Value(value),
            None => Self::Null,
        })
    }
}

fn nullable<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Providers {
    pub(crate) request_id: String,
    pub(crate) entries: Vec<Provider>,
}

#[derive(Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Provider {
    pub(crate) provider: String,
    pub(crate) status: ProviderStatus,
    #[serde(default = "enabled")]
    pub(crate) enabled: bool,
    #[serde(default, deserialize_with = "present")]
    pub(crate) label: Option<String>,
    #[serde(default, deserialize_with = "present")]
    pub(crate) error: Option<String>,
    #[serde(default, deserialize_with = "present")]
    pub(crate) models: Option<Vec<Model>>,
    #[serde(default, deserialize_with = "present")]
    pub(crate) modes: Option<Vec<Mode>>,
    #[serde(default)]
    pub(crate) default_mode_id: Field<String>,
}

fn enabled() -> bool {
    true
}

#[derive(Deserialize, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ProviderStatus {
    Ready,
    Loading,
    Error,
    Unavailable,
}

#[derive(Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Model {
    pub(crate) provider: String,
    pub(crate) id: String,
    pub(crate) label: String,
    #[serde(default, deserialize_with = "present")]
    pub(crate) is_default: Option<bool>,
    #[serde(default, deserialize_with = "present")]
    pub(crate) is_selectable: Option<bool>,
}

#[derive(Deserialize, PartialEq)]
pub(crate) struct Mode {
    pub(crate) id: String,
    pub(crate) label: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ProviderFeatures {
    pub(crate) request_id: String,
    pub(crate) provider: String,
    #[serde(default, deserialize_with = "present")]
    pub(crate) features: Option<Vec<Feature>>,
    #[serde(default)]
    pub(crate) error: Field<String>,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum Feature {
    Toggle {
        id: String,
        label: String,
        value: bool,
    },
    Select {
        id: String,
        label: String,
        #[serde(deserialize_with = "nullable")]
        value: Option<String>,
        options: Vec<SelectOption>,
    },
}

#[derive(Deserialize)]
pub(crate) struct SelectOption {
    pub(crate) id: String,
    pub(crate) label: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PageInfo {
    #[serde(deserialize_with = "nullable")]
    pub(crate) next_cursor: Option<String>,
    #[serde(deserialize_with = "nullable")]
    pub(crate) prev_cursor: Option<String>,
    pub(crate) has_more: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Workspaces {
    pub(crate) request_id: String,
    pub(crate) entries: Vec<Workspace>,
    pub(crate) page_info: PageInfo,
}

#[derive(Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Workspace {
    pub(crate) id: String,
    pub(crate) project_id: String,
    pub(crate) project_display_name: String,
    pub(crate) project_root_path: String,
    #[serde(default, deserialize_with = "present")]
    pub(crate) workspace_directory: Option<String>,
    pub(crate) name: String,
    pub(crate) status: WorkspaceStatus,
    pub(crate) project_kind: ProjectKind,
    pub(crate) workspace_kind: WorkspaceKind,
}

impl Workspace {
    pub(crate) fn directory(&self) -> &str {
        self.workspace_directory
            .as_deref()
            .unwrap_or(&self.project_root_path)
    }
}

#[derive(Deserialize, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WorkspaceStatus {
    NeedsInput,
    Failed,
    Running,
    Attention,
    Done,
}

#[derive(Deserialize, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WorkspaceKind {
    Directory,
    LocalCheckout,
    Checkout,
    Worktree,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Agents {
    pub(crate) request_id: String,
    pub(crate) entries: Vec<AgentEntry>,
    pub(crate) page_info: PageInfo,
}

#[derive(Deserialize, PartialEq)]
pub(crate) struct AgentEntry {
    pub(crate) agent: Agent,
    pub(crate) project: ProjectPlacement,
}

#[derive(Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Agent {
    pub(crate) id: String,
    pub(crate) provider: String,
    pub(crate) cwd: String,
    #[serde(default, deserialize_with = "present")]
    pub(crate) workspace_id: Option<String>,
    #[serde(deserialize_with = "nullable")]
    pub(crate) model: Option<String>,
    #[serde(deserialize_with = "nullable")]
    pub(crate) title: Option<String>,
    #[serde(deserialize_with = "nullable")]
    pub(crate) current_mode_id: Option<String>,
    pub(crate) status: AgentStatus,
    // Full snapshots carry pending permissions, active turns and provider metadata needed on reopen.
    // Retain these without logging or exposing raw values in the UI.
    #[serde(flatten)]
    pub(crate) extra: serde_json::Map<String, Value>,
}

#[derive(Deserialize, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AgentStatus {
    Initializing,
    Idle,
    Running,
    Error,
    Closed,
}

#[derive(Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ProjectPlacement {
    pub(crate) project_key: String,
    pub(crate) project_name: String,
    #[serde(default)]
    pub(crate) workspace_name: Field<String>,
    pub(crate) checkout: Checkout,
}

#[derive(Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Checkout {
    pub(crate) cwd: String,
    pub(crate) is_git: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AgentResult {
    pub(crate) request_id: String,
    #[serde(deserialize_with = "nullable")]
    pub(crate) agent: Option<Agent>,
    #[serde(default)]
    pub(crate) project: Field<ProjectPlacement>,
    #[serde(deserialize_with = "nullable")]
    pub(crate) error: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixtures() -> Vec<Value> {
        serde_json::from_str(include_str!("../../../protocol-fixtures/catalog.json")).unwrap()
    }

    fn message(request_id: &str, direction: &str) -> Value {
        fixtures()
            .into_iter()
            .find(|entry| {
                entry["direction"] == direction
                    && (entry["frame"]["message"]["requestId"] == request_id
                        || entry["frame"]["message"]["payload"]["requestId"] == request_id)
            })
            .unwrap()["frame"]["message"]
            .clone()
    }

    fn response(request_id: &str) -> CatalogResponse {
        let message = message(request_id, "server");
        let response = parse_response(
            message["type"].as_str().unwrap(),
            message["payload"].clone(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(response.request_id(), request_id);
        response
    }

    #[test]
    fn requests_match_real_sdk_frames_and_use_the_agent_directory_project_id() {
        for (id, wire) in [
            (
                "catalog-providers",
                providers("catalog-providers", "/fixture/project"),
            ),
            (
                "catalog-features",
                provider_features("catalog-features", "codex", "/fixture/project"),
            ),
            (
                "catalog-workspaces",
                workspaces_for_project("catalog-workspaces", "fixture-project", None).unwrap(),
            ),
            (
                "catalog-agents",
                agents_for_project("catalog-agents", "fixture-project", None).unwrap(),
            ),
            ("catalog-agent", agent("catalog-agent", "fixture-agent-a")),
        ] {
            let request: Value = serde_json::from_str(&wire).unwrap();
            assert_eq!(request["message"], message(id, "client"));
        }
        assert!(agents_for_project("page", "fixture-project", Some("")).is_err());
        let page: Value = serde_json::from_str(
            &workspaces_for_project("page", "fixture-project", Some("opaque-cursor")).unwrap(),
        )
        .unwrap();
        assert_eq!(page["message"]["page"]["cursor"], "opaque-cursor");
    }

    #[test]
    fn real_catalog_retains_models_modes_features_and_workspace_agent_associations() {
        let CatalogResponse::Providers(providers) = response("catalog-providers") else {
            panic!("wrong response")
        };
        let codex = providers
            .entries
            .iter()
            .find(|entry| entry.provider == "codex")
            .unwrap();
        assert_eq!(codex.status, ProviderStatus::Ready);
        assert!(codex.enabled);
        assert_eq!(codex.models.as_ref().unwrap()[0].id, "gpt-5.4-mini");
        assert_eq!(codex.models.as_ref().unwrap()[0].is_default, Some(true));
        let CatalogResponse::ProviderFeatures(features) = response("catalog-features") else {
            panic!("wrong response")
        };
        assert_eq!(features.provider, "codex");
        assert!(features.features.unwrap().is_empty());
        assert!(matches!(features.error, Field::Null));
        let live_feature: Feature =
            decode(message("catalog-agent", "server")["payload"]["agent"]["features"][0].clone())
                .unwrap();
        assert!(
            matches!(live_feature, Feature::Toggle { id, value: false, .. } if id == "test_feature")
        );
        let CatalogResponse::Workspaces(workspaces) = response("catalog-workspaces") else {
            panic!("wrong response")
        };
        assert_eq!(workspaces.entries.len(), 2);
        assert!(!workspaces.page_info.has_more);
        assert_eq!(workspaces.page_info.next_cursor, None);
        let CatalogResponse::Agents(agents) = response("catalog-agents") else {
            panic!("wrong response")
        };
        assert_eq!(agents.entries.len(), 2);
        for entry in agents.entries {
            assert_eq!(entry.project.project_key, "fixture-project");
            assert!(
                workspaces
                    .entries
                    .iter()
                    .any(|workspace| Some(&workspace.id) == entry.agent.workspace_id.as_ref())
            );
            assert_eq!(entry.agent.status, AgentStatus::Idle);
        }
        let CatalogResponse::Agent(detail) = response("catalog-agent") else {
            panic!("wrong response")
        };
        assert_eq!(detail.agent.unwrap().id, "fixture-agent-a");
        assert_eq!(detail.error, None);
        let CatalogResponse::Agent(missing) = response("catalog-missing-agent") else {
            panic!("wrong response")
        };
        assert!(missing.agent.is_none());
        assert!(matches!(missing.project, Field::Null));
    }

    #[test]
    fn provider_and_model_ids_are_open_and_missing_is_distinct_from_null() {
        let mut payload = message("catalog-providers", "server")["payload"].clone();
        payload["entries"][0]["provider"] = json!("third-party-provider");
        payload["entries"][0]["models"][0]["id"] = json!("vendor/model:custom");
        let CatalogResponse::Providers(providers) =
            parse_response("get_providers_snapshot_response", payload.clone())
                .unwrap()
                .unwrap()
        else {
            panic!("wrong response")
        };
        assert_eq!(providers.entries[0].provider, "third-party-provider");
        assert_eq!(
            providers.entries[0].models.as_ref().unwrap()[0].id,
            "vendor/model:custom"
        );
        payload["entries"][0]
            .as_object_mut()
            .unwrap()
            .remove("models");
        payload["entries"][0]
            .as_object_mut()
            .unwrap()
            .remove("defaultModeId");
        let CatalogResponse::Providers(providers) =
            parse_response("get_providers_snapshot_response", payload.clone())
                .unwrap()
                .unwrap()
        else {
            panic!("wrong response")
        };
        assert!(providers.entries[0].models.is_none());
        assert!(matches!(
            providers.entries[0].default_mode_id,
            Field::Missing
        ));
        payload["entries"][0]["models"] = Value::Null;
        assert!(parse_response("get_providers_snapshot_response", payload).is_err());
        let mut agent = message("catalog-agent", "server")["payload"].clone();
        agent["agent"]["model"] = Value::Null;
        assert!(parse_response("fetch_agent_response", agent.clone()).is_ok());
        agent["agent"].as_object_mut().unwrap().remove("model");
        assert!(parse_response("fetch_agent_response", agent).is_err());
    }

    #[test]
    fn legacy_workspace_directory_falls_back_and_parse_errors_do_not_echo_input() {
        let mut payload = message("catalog-workspaces", "server")["payload"].clone();
        payload["entries"][0]
            .as_object_mut()
            .unwrap()
            .remove("workspaceDirectory");
        let CatalogResponse::Workspaces(workspaces) =
            parse_response("fetch_workspaces_response", payload.clone())
                .unwrap()
                .unwrap()
        else {
            panic!("wrong response")
        };
        assert_eq!(workspaces.entries[0].directory(), "/fixture/project");
        payload["pageInfo"]
            .as_object_mut()
            .unwrap()
            .remove("nextCursor");
        let error = parse_response("fetch_workspaces_response", payload)
            .err()
            .unwrap();
        assert!(!error.to_string().contains("/fixture"));
        assert!(
            parse_response("future.catalog.response", json!({"credential":"secret"}))
                .unwrap()
                .is_none()
        );
        let error = parse_response(
            "get_providers_snapshot_response",
            json!({"requestId":"secret","entries":null}),
        )
        .err()
        .unwrap();
        assert!(!format!("{error:?}").contains("secret"));
    }
}
