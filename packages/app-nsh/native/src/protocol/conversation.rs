//! Chat wire projection of messages.ts and daemon-client.ts, not a timeline reducer.
//! Snapshot/tool/timeline extras are retained, but this module validates only the displayed
//! timeline fields below, not every AgentTimelineItemPayloadSchema detail. No DTO is Debug:
//! messages, tool input, provider options and unknown payloads may contain private data.

use super::catalog::{Agent, AgentStatus, Field, ProjectPlacement, Workspace};
use super::{ProtocolError, decode, malformed, present};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value, json};

type Object = Map<String, Value>;
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

fn session(message: Value) -> String {
    json!({"type":"session","message":message}).to_string()
}

fn nullable<'de, D, T>(d: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(d)
}

fn safe_integer<'de, D: Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
    // JSON 1, 1.0 and 1e0 are the same JS number at the upstream Zod boundary.
    let value = serde_json::Number::deserialize(d)?
        .as_f64()
        .ok_or_else(|| serde::de::Error::custom("invalid sequence"))?;
    if value < 0.0 || value > MAX_SAFE_INTEGER as f64 || value.fract() != 0.0 {
        return Err(serde::de::Error::custom("invalid sequence"));
    }
    Ok(value as u64)
}

fn missing<T>(field: &Field<T>) -> bool {
    matches!(field, Field::Missing)
}
fn serialize_field<S: Serializer, T: Serialize>(field: &Field<T>, s: S) -> Result<S::Ok, S::Error> {
    match field {
        Field::Value(value) => value.serialize(s),
        Field::Null | Field::Missing => s.serialize_none(),
    }
}

#[derive(Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AgentConfig {
    pub(crate) provider: String,
    pub(crate) cwd: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) mode_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) thinking_option_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) feature_values: Option<Object>,
    #[serde(skip_serializing_if = "missing", serialize_with = "serialize_field")]
    pub(crate) title: Field<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) provider_options: Option<Object>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) system_prompt: Option<String>,
}

// This builder deliberately creates an ordinary chat agent, without worktree/MCP/attachment APIs.
// Gate creationLifecycle once on server_info.features before using the modern creation RPC.
pub(crate) fn create_agent(
    request_id: &str,
    config: &AgentConfig,
    workspace_id: Option<&str>,
    idempotency_key: Option<&str>,
    subscribe: Option<bool>,
) -> Result<String, ProtocolError> {
    let mut config = serde_json::to_value(config).map_err(|_| malformed())?;
    if let Some(title) = config.get("title").and_then(Value::as_str) {
        // Match ECMAScript String.trim (BOM is whitespace; U+0085 is not).
        let trimmed = title.trim_matches(|ch| {
            matches!(ch, '\u{9}'..='\u{d}' | '\u{20}' | '\u{a0}' |
            '\u{1680}' | '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}' |
            '\u{202f}' | '\u{205f}' | '\u{3000}' | '\u{feff}')
        });
        if trimmed.is_empty() || trimmed.encode_utf16().count() > 200 {
            return Err(malformed());
        }
        config["title"] = json!(trimmed);
    }
    let mut message =
        json!({"type":"agent.create.request","requestId":request_id,"config":config,"labels":{}});
    if let Some(value) = workspace_id {
        message["workspaceId"] = json!(value);
    }
    if let Some(value) = idempotency_key {
        validate_key(value)?;
        message["idempotencyKey"] = json!(value);
    }
    if let Some(value) = subscribe {
        message["subscribe"] = json!(value);
    }
    Ok(session(message))
}

fn validate_key(key: &str) -> Result<(), ProtocolError> {
    if key.is_empty() || key.encode_utf16().count() > 512 {
        return Err(malformed());
    }
    Ok(())
}

#[derive(Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CreationKind {
    Workspace,
    Agent,
}

pub(crate) fn subscribe_creation(
    request_id: &str,
    kind: CreationKind,
    idempotency_key: &str,
    subscribe: Option<bool>,
) -> Result<String, ProtocolError> {
    validate_key(idempotency_key)?;
    let mut message = json!({"type":"creation.subscribe.request","requestId":request_id,
        "kind":kind,"idempotencyKey":idempotency_key});
    if let Some(value) = subscribe {
        message["subscribe"] = json!(value);
    }
    Ok(session(message))
}

// Chat needs only these session event channels; the timeline has separate membership.
#[derive(Serialize)]
pub(crate) enum ChatSessionEvent {
    #[serde(rename = "agent_permission_request")]
    PermissionRequest,
    #[serde(rename = "agent_permission_resolved")]
    PermissionResolved,
    #[serde(rename = "agent_attention_required")]
    AttentionRequired,
}

pub(crate) fn subscribe_session_events(
    request_id: &str,
    events: &[ChatSessionEvent],
    notifications: Option<bool>,
) -> String {
    let mut message = json!({"type":"session.events.set_subscription.request","requestId":request_id,"events":events});
    if let Some(value) = notifications {
        message["notifications"] = json!(value);
    }
    session(message)
}

pub(crate) fn subscribe_timeline(request_id: &str, agent_ids: &[String]) -> String {
    session(
        json!({"type":"agent.timeline.set_subscription.request","requestId":request_id,"agentIds":agent_ids}),
    )
}

pub(crate) fn release_subscription(request_id: &str, subscription_id: &str) -> String {
    session(
        json!({"type":"subscription.release.request","requestId":request_id,"subscriptionId":subscription_id}),
    )
}

#[derive(Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Direction {
    Tail,
    Before,
    After,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct Cursor {
    pub(crate) epoch: String,
    #[serde(deserialize_with = "safe_integer")]
    pub(crate) seq: u64,
}

#[derive(Default)]
pub(crate) struct HistoryOptions {
    pub(crate) direction: Option<Direction>,
    pub(crate) cursor: Option<Cursor>,
    pub(crate) limit: Option<u64>,
    pub(crate) merge_window: Option<bool>,
    // Caller must gate on features.wholeTurnTimeline; None sends no capability-dependent field.
    pub(crate) whole_turns: Option<bool>,
}

pub(crate) fn fetch_timeline(
    request_id: &str,
    agent_id: &str,
    options: &HistoryOptions,
) -> Result<String, ProtocolError> {
    if options.limit.is_some_and(|n| n > MAX_SAFE_INTEGER)
        || options
            .cursor
            .as_ref()
            .is_some_and(|c| c.seq > MAX_SAFE_INTEGER)
    {
        return Err(malformed());
    }
    let mut message = json!({"type":"fetch_agent_timeline_request","requestId":request_id,
        "agentId":agent_id,"projection":"projected"});
    if let Some(value) = &options.direction {
        message["direction"] = json!(value);
    }
    if let Some(value) = &options.cursor {
        message["cursor"] = json!(value);
    }
    if let Some(value) = options.limit {
        message["limit"] = json!(value);
    }
    if let Some(value) = options.merge_window {
        message["mergeWindow"] = json!(value);
    }
    if let Some(value) = options.whole_turns {
        message["wholeTurns"] = json!(value);
    }
    Ok(session(message))
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ActiveTurnBehavior {
    Interrupt,
    Steer,
}

pub(crate) fn send_message(
    request_id: &str,
    agent_id: &str,
    message_id: &str,
    text: &str,
    active_turn_behavior: Option<ActiveTurnBehavior>,
) -> String {
    let mut message = json!({"type":"send_agent_message_request","requestId":request_id,
        "agentId":agent_id,"messageId":message_id,"text":text});
    if let Some(value) = active_turn_behavior {
        message["activeTurnBehavior"] = json!(value);
    }
    session(message)
}

pub(crate) fn cancel_agent(request_id: &str, agent_id: &str) -> String {
    session(json!({"type":"cancel_agent_request","requestId":request_id,"agentId":agent_id}))
}

pub(crate) fn respond_permission(
    agent_id: &str,
    permission_request_id: &str,
    response: &PermissionResponse,
) -> String {
    // requestId identifies the permission, not an RPC receipt. Wait for permission_resolved.
    session(
        json!({"type":"agent_permission_response","agentId":agent_id,
        "requestId":permission_request_id,"response":response}),
    )
}

pub(crate) enum ConversationMessage {
    Created(Created),
    CreationSubscription(CreationSubscription),
    CreationUpdate(CreationSnapshot),
    SessionSubscription(SessionSubscription),
    TimelineSubscription(TimelineSubscription),
    SubscriptionReleased(SubscriptionReleased),
    History(History),
    Sent(Sent),
    Cancelled(Cancelled),
    Stream(Stream),
    PermissionRequested(PermissionRequested),
    PermissionResolved(PermissionResolved),
    AttentionRequired(AttentionRequired),
    TimelineReplacement(TimelineReplacement),
    AgentStatus(AgentStatusUpdate),
    AgentUpdate(AgentUpdate),
    RpcError(RpcError),
}

impl ConversationMessage {
    pub(crate) fn request_id(&self) -> Option<&str> {
        Some(match self {
            Self::Created(v) => &v.request_id,
            Self::CreationSubscription(v) => &v.request_id,
            Self::SessionSubscription(v) => &v.request_id,
            Self::TimelineSubscription(v) => &v.request_id,
            Self::SubscriptionReleased(v) => &v.request_id,
            Self::History(v) => &v.request_id,
            Self::Sent(v) => &v.request_id,
            Self::Cancelled(v) => &v.request_id,
            Self::RpcError(v) => &v.request_id,
            // PermissionResolved.requestId is a permission identity, never a pending RPC ID.
            _ => return None,
        })
    }
}

pub(crate) fn parse_message(
    kind: &str,
    payload: Value,
) -> Result<Option<ConversationMessage>, ProtocolError> {
    Ok(Some(match kind {
        "agent.create.response" => ConversationMessage::Created(decode(payload)?),
        "creation.subscribe.response" => {
            ConversationMessage::CreationSubscription(decode(payload)?)
        }
        "agent.create.update" | "workspace.create.update" => {
            ConversationMessage::CreationUpdate(decode(payload)?)
        }
        "session.events.set_subscription.response" => {
            ConversationMessage::SessionSubscription(decode(payload)?)
        }
        "agent.timeline.set_subscription.response" => {
            ConversationMessage::TimelineSubscription(decode(payload)?)
        }
        "subscription.release.response" => {
            ConversationMessage::SubscriptionReleased(decode(payload)?)
        }
        "fetch_agent_timeline_response" => ConversationMessage::History(decode(payload)?),
        "send_agent_message_response" => ConversationMessage::Sent(decode(payload)?),
        "cancel_agent_response" => ConversationMessage::Cancelled(decode(payload)?),
        "agent_stream" => ConversationMessage::Stream(decode(payload)?),
        "agent_permission_request" => ConversationMessage::PermissionRequested(decode(payload)?),
        "agent_permission_resolved" => ConversationMessage::PermissionResolved(decode(payload)?),
        "agent_attention_required" => ConversationMessage::AttentionRequired(decode(payload)?),
        "agent.timeline.replacement" => ConversationMessage::TimelineReplacement(decode(payload)?),
        "agent_status" => ConversationMessage::AgentStatus(decode(payload)?),
        "agent_update" => ConversationMessage::AgentUpdate(decode(payload)?),
        "rpc_error" => ConversationMessage::RpcError(decode(payload)?),
        _ => return Ok(None),
    }))
}

// Extend the shared catalog projection without duplicating the snapshot schema. Raw extras
// retain features/capabilities/runtime metadata for later consumers; they are not fully validated.
pub(crate) struct Snapshot {
    pub(crate) agent: Agent,
    pub(crate) pending_permissions: Vec<PermissionRequest>,
    pub(crate) active_turn: Field<ActiveTurn>,
}

impl Snapshot {
    pub(crate) fn from_agent(agent: Agent) -> Result<Self, ProtocolError> {
        let pending_permissions = decode(
            agent
                .extra
                .get("pendingPermissions")
                .cloned()
                .ok_or_else(malformed)?,
        )?;
        let active_turn = match agent.extra.get("activeTurn") {
            None => Field::Missing,
            Some(Value::Null) => Field::Null,
            Some(value) => Field::Value(decode(value.clone())?),
        };
        Ok(Self {
            agent,
            pending_permissions,
            active_turn,
        })
    }
}

impl<'de> Deserialize<'de> for Snapshot {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Self::from_agent(Agent::deserialize(d)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ActiveTurn {
    pub(crate) turn_id: String,
    #[serde(deserialize_with = "nullable")]
    pub(crate) started_at: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Created {
    pub(crate) request_id: String,
    #[serde(deserialize_with = "nullable")]
    pub(crate) agent: Option<Snapshot>,
    #[serde(deserialize_with = "nullable")]
    pub(crate) error: Option<String>,
    #[serde(default, deserialize_with = "present")]
    pub(crate) creation: Option<CreationSnapshot>,
}

#[derive(Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CreationPhase {
    Accepted,
    WorkspaceReady,
    AgentReady,
    PromptStarted,
    Completed,
    Failed,
}

#[derive(Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FailedStage {
    Workspace,
    Agent,
    Prompt,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CreationSnapshot {
    pub(crate) kind: CreationKind,
    pub(crate) idempotency_key: String,
    #[serde(deserialize_with = "safe_integer")]
    pub(crate) revision: u64,
    pub(crate) phase: CreationPhase,
    #[serde(deserialize_with = "nullable")]
    pub(crate) workspace_id: Option<String>,
    #[serde(deserialize_with = "nullable")]
    pub(crate) agent_id: Option<String>,
    #[serde(default, deserialize_with = "present")]
    pub(crate) workspace: Option<Workspace>,
    #[serde(default, deserialize_with = "present")]
    pub(crate) agent: Option<Snapshot>,
    #[serde(deserialize_with = "nullable")]
    pub(crate) error: Option<String>,
    #[serde(default, deserialize_with = "present")]
    pub(crate) setup_skipped_reason: Option<String>,
    #[serde(default, deserialize_with = "present")]
    pub(crate) error_code: Option<String>,
    #[serde(default, deserialize_with = "present")]
    pub(crate) failed_stage: Option<FailedStage>,
    #[serde(default, deserialize_with = "present")]
    pub(crate) outcome_unknown: Option<bool>,
    #[serde(default, deserialize_with = "present")]
    pub(crate) subscription_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CreationSubscription {
    pub(crate) request_id: String,
    #[serde(default, deserialize_with = "present")]
    pub(crate) subscription_id: Option<String>,
    #[serde(deserialize_with = "nullable")]
    pub(crate) snapshot: Option<CreationSnapshot>,
    #[serde(deserialize_with = "nullable")]
    pub(crate) error: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SessionSubscription {
    pub(crate) request_id: String,
    #[serde(default, deserialize_with = "present")]
    pub(crate) subscription_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TimelineSubscription {
    pub(crate) request_id: String,
    pub(crate) agent_ids: Vec<String>,
    #[serde(default, deserialize_with = "present")]
    pub(crate) subscription_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SubscriptionReleased {
    pub(crate) request_id: String,
    pub(crate) subscription_id: String,
}

#[derive(Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Projection {
    Projected,
    Canonical,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct History {
    pub(crate) request_id: String,
    pub(crate) agent_id: String,
    #[serde(deserialize_with = "nullable")]
    pub(crate) agent: Option<Snapshot>,
    pub(crate) direction: Direction,
    pub(crate) projection: Projection,
    pub(crate) epoch: String,
    pub(crate) reset: bool,
    pub(crate) stale_cursor: bool,
    pub(crate) gap: bool,
    pub(crate) window: TimelineWindow,
    #[serde(deserialize_with = "nullable")]
    pub(crate) start_cursor: Option<Cursor>,
    #[serde(deserialize_with = "nullable")]
    pub(crate) end_cursor: Option<Cursor>,
    pub(crate) has_older: bool,
    pub(crate) has_newer: bool,
    #[serde(default, deserialize_with = "present")]
    pub(crate) merge_window: Option<bool>,
    pub(crate) entries: Vec<TimelineEntry>,
    #[serde(deserialize_with = "nullable")]
    pub(crate) error: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TimelineWindow {
    #[serde(deserialize_with = "safe_integer")]
    pub(crate) min_seq: u64,
    #[serde(deserialize_with = "safe_integer")]
    pub(crate) max_seq: u64,
    #[serde(deserialize_with = "safe_integer")]
    pub(crate) next_seq: u64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SeqRange {
    #[serde(deserialize_with = "safe_integer")]
    pub(crate) start_seq: u64,
    #[serde(deserialize_with = "safe_integer")]
    pub(crate) end_seq: u64,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Collapse {
    AssistantMerge,
    ReasoningMerge,
    ToolLifecycle,
    Identity,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TimelineEntry {
    pub(crate) provider: String,
    pub(crate) item: TimelineItem,
    #[serde(default, deserialize_with = "present")]
    pub(crate) turn_id: Option<String>,
    pub(crate) timestamp: String,
    #[serde(deserialize_with = "safe_integer")]
    pub(crate) seq_start: u64,
    #[serde(deserialize_with = "safe_integer")]
    pub(crate) seq_end: u64,
    pub(crate) source_seq_ranges: Vec<SeqRange>,
    pub(crate) collapsed: Vec<Collapse>,
}

#[derive(Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DeliveryStatus {
    Queued,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Sent {
    pub(crate) request_id: String,
    pub(crate) agent_id: String,
    pub(crate) accepted: bool,
    #[serde(default, deserialize_with = "present")]
    pub(crate) delivery_status: Option<DeliveryStatus>,
    #[serde(deserialize_with = "nullable")]
    pub(crate) error: Option<String>,
}

impl Sent {
    // Acceptance is a delivery receipt, not completion of an agent turn.
    pub(crate) fn accepted(&self) -> Result<Option<&DeliveryStatus>, &str> {
        if !self.accepted {
            return Err(self.error.as_deref().unwrap_or("sendAgentMessage rejected"));
        }
        Ok(self.delivery_status.as_ref())
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Cancelled {
    pub(crate) request_id: String,
    pub(crate) agent_id: String,
    #[serde(deserialize_with = "nullable")]
    pub(crate) agent: Option<Snapshot>,
    #[serde(default)]
    pub(crate) error: Field<String>,
}

pub(crate) enum CancelState<'a> {
    Failed(&'a str),
    MissingAgent,
    Pending,
    Stopped(&'a AgentStatus),
}

impl Cancelled {
    pub(crate) fn state(&self) -> CancelState<'_> {
        if let Field::Value(error) = &self.error {
            return CancelState::Failed(error);
        }
        let Some(snapshot) = &self.agent else {
            return CancelState::MissingAgent;
        };
        if matches!(
            snapshot.agent.status,
            AgentStatus::Running | AgentStatus::Initializing
        ) || matches!(&snapshot.active_turn, Field::Value(_))
        {
            return CancelState::Pending;
        }
        CancelState::Stopped(&snapshot.agent.status)
    }
}

#[derive(Deserialize, Serialize, PartialEq, Eq, Clone)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PermissionKind {
    Tool,
    Plan,
    Question,
    Mode,
    Other,
}

#[derive(Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PermissionRequest {
    pub(crate) id: String,
    pub(crate) provider: String,
    pub(crate) name: String,
    pub(crate) kind: PermissionKind,
    #[serde(default, deserialize_with = "present")]
    pub(crate) title: Option<String>,
    #[serde(default, deserialize_with = "present")]
    pub(crate) description: Option<String>,
    #[serde(default, deserialize_with = "present")]
    pub(crate) input: Option<Object>,
    #[serde(default, deserialize_with = "present")]
    pub(crate) detail: Option<ToolDetail>,
    #[serde(default, deserialize_with = "present")]
    pub(crate) suggestions: Option<Vec<Object>>,
    #[serde(default, deserialize_with = "present")]
    pub(crate) actions: Option<Vec<PermissionAction>>,
    #[serde(default, deserialize_with = "present")]
    pub(crate) metadata: Option<Object>,
    #[serde(flatten)]
    pub(crate) extra: Object,
}

#[derive(Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PermissionAction {
    pub(crate) id: String,
    pub(crate) label: String,
    pub(crate) behavior: PermissionBehavior,
    #[serde(default, deserialize_with = "present")]
    pub(crate) variant: Option<ActionVariant>,
    #[serde(default, deserialize_with = "present")]
    pub(crate) intent: Option<ActionIntent>,
}

#[derive(Deserialize, Clone)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PermissionBehavior {
    Allow,
    Deny,
}
#[derive(Deserialize, Clone)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ActionVariant {
    Primary,
    Secondary,
    Danger,
}
#[derive(Deserialize, Clone)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ActionIntent {
    Implement,
    ImplementResume,
    Dismiss,
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(tag = "behavior", rename_all = "snake_case")]
pub(crate) enum PermissionResponse {
    #[serde(rename_all = "camelCase")]
    Allow {
        #[serde(
            default,
            deserialize_with = "present",
            skip_serializing_if = "Option::is_none"
        )]
        selected_action_id: Option<String>,
        #[serde(
            default,
            deserialize_with = "present",
            skip_serializing_if = "Option::is_none"
        )]
        updated_input: Option<Object>,
        #[serde(
            default,
            deserialize_with = "present",
            skip_serializing_if = "Option::is_none"
        )]
        updated_permissions: Option<Vec<Object>>,
    },
    #[serde(rename_all = "camelCase")]
    Deny {
        #[serde(
            default,
            deserialize_with = "present",
            skip_serializing_if = "Option::is_none"
        )]
        selected_action_id: Option<String>,
        #[serde(
            default,
            deserialize_with = "present",
            skip_serializing_if = "Option::is_none"
        )]
        message: Option<String>,
        #[serde(
            default,
            deserialize_with = "present",
            skip_serializing_if = "Option::is_none"
        )]
        interrupt: Option<bool>,
    },
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PermissionRequested {
    pub(crate) agent_id: String,
    pub(crate) request: PermissionRequest,
    #[serde(default, deserialize_with = "present")]
    pub(crate) subscription_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PermissionResolved {
    pub(crate) agent_id: String,
    pub(crate) request_id: String,
    pub(crate) resolution: PermissionResponse,
    #[serde(default, deserialize_with = "present")]
    pub(crate) subscription_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AttentionRequired {
    pub(crate) agent_id: String,
    pub(crate) reason: AttentionReason,
    pub(crate) timestamp: String,
    pub(crate) should_notify: bool,
    #[serde(default, deserialize_with = "present")]
    pub(crate) notification: Option<Notification>,
    #[serde(default, deserialize_with = "present")]
    pub(crate) subscription_id: Option<String>,
}

#[derive(Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TimelineReplacement {
    pub(crate) agent_id: String,
    pub(crate) epoch: String,
    #[serde(default, deserialize_with = "present")]
    pub(crate) subscription_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AgentStatusUpdate {
    pub(crate) agent_id: String,
    pub(crate) status: String,
    pub(crate) info: Snapshot,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum AgentUpdate {
    #[serde(rename_all = "camelCase")]
    Upsert {
        agent: Snapshot,
        #[serde(default)]
        project: Field<ProjectPlacement>,
        #[serde(default, deserialize_with = "present")]
        subscription_id: Option<String>,
        #[serde(default, deserialize_with = "present")]
        generation: Option<String>,
        #[serde(default, deserialize_with = "positive_seq")]
        seq: Option<u64>,
    },
    #[serde(rename_all = "camelCase")]
    Remove {
        agent_id: String,
        #[serde(default, deserialize_with = "present")]
        subscription_id: Option<String>,
        #[serde(default, deserialize_with = "present")]
        generation: Option<String>,
        #[serde(default, deserialize_with = "positive_seq")]
        seq: Option<u64>,
    },
}

fn positive_seq<'de, D: Deserializer<'de>>(d: D) -> Result<Option<u64>, D::Error> {
    let value = safe_integer(d)?;
    if value == 0 {
        return Err(serde::de::Error::custom("invalid directory sequence"));
    }
    Ok(Some(value))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RpcError {
    pub(crate) request_id: String,
    #[serde(default, deserialize_with = "present")]
    pub(crate) request_type: Option<String>,
    pub(crate) error: String,
    #[serde(default, deserialize_with = "present")]
    pub(crate) code: Option<String>,
}

#[derive(Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Stream {
    pub(crate) agent_id: String,
    pub(crate) event: StreamEvent,
    pub(crate) timestamp: String,
    #[serde(default, deserialize_with = "present")]
    pub(crate) subscription_id: Option<String>,
    #[serde(default, deserialize_with = "optional_seq")]
    pub(crate) seq: Option<u64>,
    #[serde(default, deserialize_with = "present")]
    pub(crate) epoch: Option<String>,
}

fn optional_seq<'de, D: Deserializer<'de>>(d: D) -> Result<Option<u64>, D::Error> {
    safe_integer(d).map(Some)
}

#[derive(Clone)]
pub(crate) enum StreamEvent {
    Known(KnownStreamEvent),
    // Never interpreted as completion/acceptance. A newer daemon may add event types.
    Unknown(Value),
}

#[derive(Deserialize, Clone)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum KnownStreamEvent {
    #[serde(rename_all = "camelCase")]
    ThreadStarted {
        provider: String,
        session_id: String,
    },
    #[serde(rename_all = "camelCase")]
    TurnStarted {
        provider: String,
        #[serde(default, deserialize_with = "present")]
        turn_id: Option<String>,
    },
    #[serde(rename_all = "camelCase")]
    TurnCompleted {
        provider: String,
        #[serde(default, deserialize_with = "present")]
        turn_id: Option<String>,
        #[serde(default, deserialize_with = "present")]
        usage: Option<Usage>,
    },
    #[serde(rename_all = "camelCase")]
    TurnFailed {
        provider: String,
        #[serde(default, deserialize_with = "present")]
        turn_id: Option<String>,
        error: String,
        #[serde(default, deserialize_with = "present")]
        code: Option<String>,
        #[serde(default, deserialize_with = "present")]
        diagnostic: Option<String>,
    },
    #[serde(rename_all = "camelCase")]
    TurnCanceled {
        provider: String,
        #[serde(default, deserialize_with = "present")]
        turn_id: Option<String>,
        reason: String,
    },
    #[serde(rename_all = "camelCase")]
    Timeline {
        provider: String,
        item: TimelineItem,
        #[serde(default, deserialize_with = "present")]
        turn_id: Option<String>,
    },
    PermissionRequested {
        provider: String,
        request: PermissionRequest,
    },
    #[serde(rename_all = "camelCase")]
    PermissionResolved {
        provider: String,
        request_id: String,
        resolution: PermissionResponse,
    },
    #[serde(rename_all = "camelCase")]
    AttentionRequired {
        provider: String,
        reason: AttentionReason,
        timestamp: String,
        should_notify: bool,
        #[serde(default, deserialize_with = "present")]
        notification: Option<Notification>,
    },
}

impl<'de> Deserialize<'de> for StreamEvent {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(d)?;
        let kind = value
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| serde::de::Error::custom("missing event type"))?;
        match kind {
            "thread_started"
            | "turn_started"
            | "turn_completed"
            | "turn_failed"
            | "turn_canceled"
            | "timeline"
            | "permission_requested"
            | "permission_resolved"
            | "attention_required" => serde_json::from_value(value)
                .map(Self::Known)
                .map_err(serde::de::Error::custom),
            _ => Ok(Self::Unknown(value)),
        }
    }
}

#[derive(Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Usage {
    #[serde(default, deserialize_with = "present")]
    pub(crate) input_tokens: Option<f64>,
    #[serde(default, deserialize_with = "present")]
    pub(crate) cached_input_tokens: Option<f64>,
    #[serde(default, deserialize_with = "present")]
    pub(crate) output_tokens: Option<f64>,
    #[serde(default, deserialize_with = "present")]
    pub(crate) total_cost_usd: Option<f64>,
    #[serde(default, deserialize_with = "present")]
    pub(crate) context_window_max_tokens: Option<f64>,
    #[serde(default, deserialize_with = "present")]
    pub(crate) context_window_used_tokens: Option<f64>,
}

#[derive(Deserialize, Clone)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AttentionReason {
    Finished,
    Error,
    Permission,
}
#[derive(Deserialize, Clone)]
pub(crate) struct Notification {
    pub(crate) title: String,
    pub(crate) body: String,
    pub(crate) data: NotificationData,
}
#[derive(Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub(crate) struct NotificationData {
    pub(crate) server_id: String,
    pub(crate) agent_id: String,
    pub(crate) reason: AttentionReason,
    #[serde(default, deserialize_with = "present")]
    pub(crate) workspace_id: Option<String>,
}

#[derive(Clone)]
pub(crate) enum TimelineItem {
    User(TimelineText),
    Assistant(TimelineText),
    Reasoning(TimelineText),
    Tool(ToolCall),
    // Known non-chat items are preserved for a later renderer, never coerced to a text reply.
    OtherKnown(Value),
    Unknown(Value),
}

#[derive(Clone)]
pub(crate) struct TimelineText {
    pub(crate) text: String,
    pub(crate) message_id: Option<String>,
    pub(crate) client_message_id: Option<String>,
    pub(crate) extra: Object,
}

impl<'de> Deserialize<'de> for TimelineText {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let mut extra = Object::deserialize(d)?;
        let text = extra
            .remove("text")
            .and_then(|value| value.as_str().map(str::to_owned))
            .ok_or_else(|| serde::de::Error::custom("missing timeline text"))?;
        let kind = extra
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        // A field defined only on another variant is an unknown extension, not a nullable ID.
        let message_id = if kind == "user_message" || kind == "assistant_message" {
            extra
                .remove("messageId")
                .map(serde_json::from_value::<String>)
                .transpose()
                .map_err(serde::de::Error::custom)?
        } else {
            None
        };
        let client_message_id = if kind == "user_message" {
            extra
                .remove("clientMessageId")
                .map(serde_json::from_value::<String>)
                .transpose()
                .map_err(serde::de::Error::custom)?
        } else {
            None
        };
        Ok(Self {
            text,
            message_id,
            client_message_id,
            extra,
        })
    }
}

impl<'de> Deserialize<'de> for TimelineItem {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(d)?;
        let kind = value
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| serde::de::Error::custom("missing timeline item type"))?;
        let parsed = match kind {
            "user_message" => serde_json::from_value(value).map(Self::User),
            "assistant_message" => serde_json::from_value(value).map(Self::Assistant),
            "reasoning" => serde_json::from_value(value).map(Self::Reasoning),
            "tool_call" => serde_json::from_value::<ToolCall>(value).and_then(|tool| {
                if (tool.status == ToolStatus::Failed) == tool.error.is_null() {
                    return Err(serde::de::Error::custom("invalid tool error"));
                }
                Ok(Self::Tool(tool))
            }),
            "todo" | "error" | "notification" | "compaction" | "plugin" => {
                Ok(Self::OtherKnown(value))
            }
            _ => Ok(Self::Unknown(value)),
        };
        parsed.map_err(serde::de::Error::custom)
    }
}

#[derive(Deserialize, PartialEq, Eq, Clone)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ToolStatus {
    Running,
    Completed,
    Failed,
    Canceled,
}

#[derive(Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ToolCall {
    pub(crate) call_id: String,
    pub(crate) name: String,
    pub(crate) status: ToolStatus,
    pub(crate) detail: ToolDetail,
    #[serde(deserialize_with = "Value::deserialize")]
    pub(crate) error: Value,
    #[serde(default, deserialize_with = "present")]
    pub(crate) metadata: Option<Object>,
    #[serde(flatten)]
    pub(crate) extra: Object,
}

// Critical display fields are checked; other platform/provider-specific detail is kept losslessly.
#[derive(Clone)]
pub(crate) struct ToolDetail {
    pub(crate) kind: String,
    pub(crate) fields: Object,
}

impl<'de> Deserialize<'de> for ToolDetail {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let fields = Object::deserialize(d)?;
        let kind = fields
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| serde::de::Error::custom("missing tool detail type"))?
            .to_owned();
        let required: &[&str] = match kind.as_str() {
            "shell" => &["command"],
            "read" | "edit" | "write" => &["filePath"],
            "search" => &["query"],
            "fetch" => &["url"],
            "plan" => &["text"],
            "worktree_setup" => &["worktreePath", "branchName", "log"],
            "sub_agent" | "plain_text" | "unknown" => &[],
            _ => return Err(serde::de::Error::custom("unknown tool detail type")),
        };
        if required
            .iter()
            .any(|key| !fields.get(*key).is_some_and(Value::is_string))
            || (kind == "unknown"
                && (!fields.contains_key("input") || !fields.contains_key("output")))
        {
            return Err(serde::de::Error::custom("missing tool detail field"));
        }
        Ok(Self { kind, fields })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn capture() -> Vec<Value> {
        serde_json::from_str(include_str!("../../../protocol-fixtures/conversation.json")).unwrap()
    }
    fn timeline() -> Vec<Value> {
        serde_json::from_str(include_str!("../../../protocol-fixtures/timeline.json")).unwrap()
    }
    fn message(id: &str, direction: &str) -> Value {
        capture()
            .into_iter()
            .find(|entry| {
                entry["direction"] == direction
                    && (entry["frame"]["message"]["requestId"] == id
                        || entry["frame"]["message"]["payload"]["requestId"] == id)
            })
            .unwrap()["frame"]["message"]
            .clone()
    }
    fn response(id: &str) -> ConversationMessage {
        let value = message(id, "server");
        let parsed = parse_message(value["type"].as_str().unwrap(), value["payload"].clone())
            .unwrap()
            .unwrap();
        assert_eq!(parsed.request_id(), Some(id));
        parsed
    }
    fn wire(text: String) -> Value {
        serde_json::from_str(&text).unwrap()
    }
    fn config() -> AgentConfig {
        AgentConfig {
            provider: "codex".into(),
            cwd: "/fixture/project".into(),
            mode_id: Some("full-access".into()),
            title: Field::Value("Conversation fixture".into()),
            ..Default::default()
        }
    }

    #[test]
    fn builders_match_real_daemon_wire_and_can_be_exported_to_the_node_schema_oracle() {
        let ConversationMessage::Created(created) = response("chat-create") else {
            panic!("not created")
        };
        let agent = created.agent.unwrap().agent;
        let history_request = timeline()
            .into_iter()
            .find(|entry| entry["direction"] == "client")
            .unwrap()["frame"]
            .clone();
        let mut options = HistoryOptions::default();
        // The older captured SDK omitted projection; the app now explicitly requests projected.
        options.direction = Some(Direction::Tail);
        options.limit = history_request["message"]["limit"].as_u64();
        let mut existing_workspace_config = config();
        existing_workspace_config.mode_id = Some("default".into());
        existing_workspace_config.title = Field::Missing;
        let requests = [
            (
                "chat-create",
                create_agent(
                    "chat-create",
                    &config(),
                    None,
                    Some("conversation-create"),
                    Some(true),
                )
                .unwrap(),
            ),
            (
                "chat-workspace-create",
                create_agent(
                    "chat-workspace-create",
                    &existing_workspace_config,
                    agent.workspace_id.as_deref(),
                    None,
                    Some(false),
                )
                .unwrap(),
            ),
            (
                "chat-observe",
                subscribe_creation(
                    "chat-observe",
                    CreationKind::Agent,
                    "conversation-create",
                    Some(true),
                )
                .unwrap(),
            ),
            (
                "chat-observe-missing",
                subscribe_creation(
                    "chat-observe-missing",
                    CreationKind::Agent,
                    "conversation-not-created",
                    Some(false),
                )
                .unwrap(),
            ),
            (
                "chat-events",
                subscribe_session_events(
                    "chat-events",
                    &[
                        ChatSessionEvent::PermissionRequest,
                        ChatSessionEvent::PermissionResolved,
                    ],
                    Some(false),
                ),
            ),
            (
                "chat-timeline",
                subscribe_timeline("chat-timeline", &[agent.id.clone()]),
            ),
            (
                "chat-send",
                send_message(
                    "chat-send",
                    &agent.id,
                    "chat-message",
                    "Respond with exactly: CHAT_CANCEL_READY",
                    None,
                ),
            ),
            (
                "chat-followup",
                send_message(
                    "chat-followup",
                    &agent.id,
                    "chat-followup-message",
                    "Respond with exactly: CHAT_FOLLOWUP",
                    None,
                ),
            ),
            ("chat-cancel", cancel_agent("chat-cancel", &agent.id)),
        ];
        let mut exported = Vec::new();
        for (id, request) in requests {
            let value = wire(request);
            assert_eq!(value["message"], message(id, "client"), "{id}");
            exported.push(value);
        }
        let history = wire(fetch_timeline("fixture-history", "fixture-agent", &options).unwrap());
        let mut projected_request = history_request;
        projected_request["message"]["projection"] = json!("projected");
        assert_eq!(history, projected_request);
        exported.push(history);
        exported.push(wire(release_subscription("release", "owned-subscription")));
        exported.push(wire(respond_permission(
            "agent",
            "permission",
            &PermissionResponse::Deny {
                selected_action_id: Some("deny-action".into()),
                message: Some("No".into()),
                interrupt: Some(true),
            },
        )));
        exported.push(wire(respond_permission(
            "agent",
            "permission",
            &PermissionResponse::Allow {
                selected_action_id: Some("implement".into()),
                updated_input: Some(Map::new()),
                updated_permissions: Some(vec![Map::new()]),
            },
        )));
        let custom_config = AgentConfig {
            provider: "third-party/provider".into(),
            cwd: "/project".into(),
            model: Some("vendor/model:custom".into()),
            title: Field::Null,
            ..Default::default()
        };
        exported.push(wire(
            create_agent("custom", &custom_config, Some("workspace"), None, None).unwrap(),
        ));
        exported.push(wire(
            fetch_timeline(
                "after",
                "agent",
                &HistoryOptions {
                    direction: Some(Direction::After),
                    cursor: Some(Cursor {
                        epoch: "epoch".into(),
                        seq: 0,
                    }),
                    limit: Some(0),
                    merge_window: Some(true),
                    whole_turns: Some(false),
                },
            )
            .unwrap(),
        ));
        exported.push(wire(send_message(
            "steer",
            "agent",
            "stable-message",
            "",
            Some(ActiveTurnBehavior::Steer),
        )));
        // The parent runs Rust tests first, then validates these actual serialized Rust frames with Zod.
        if let Ok(path) = std::env::var("PASEO_CONVERSATION_REQUESTS_OUT") {
            std::fs::write(path, serde_json::to_string_pretty(&exported).unwrap()).unwrap();
        }
    }

    #[test]
    fn real_responses_streams_and_permission_history_parse_without_coercing_unknowns() {
        for entry in capture().into_iter().chain(timeline()) {
            if entry["direction"] != "server" {
                continue;
            }
            let message = &entry["frame"]["message"];
            assert!(
                parse_message(
                    message["type"].as_str().unwrap(),
                    message["payload"].clone()
                )
                .unwrap()
                .is_some()
            );
        }
        assert!(
            parse_message(
                "future.chat.response",
                json!({"requestId":"chat-send","accepted":true})
            )
            .unwrap()
            .is_none()
        );
        let event: StreamEvent = decode(json!({"type":"future.event","secret":"private"})).unwrap();
        assert!(matches!(event, StreamEvent::Unknown(value) if value["secret"] == "private"));
        let item: TimelineItem =
            decode(json!({"type":"future.item","text":"not a reply"})).unwrap();
        assert!(matches!(item, TimelineItem::Unknown(_)));
        // A malformed known event must never fall back to Unknown.
        for value in [
            json!({"type":"turn_completed"}),
            json!({"type":"timeline","provider":"p","item":{"type":"assistant_message","text":null}}),
        ] {
            assert!(decode::<StreamEvent>(value).is_err());
        }
    }

    #[test]
    fn history_preserves_epoch_cursor_windows_source_ranges_and_shared_text_shape() {
        let fixture = timeline();
        let payload = fixture.last().unwrap()["frame"]["message"]["payload"].clone();
        let ConversationMessage::History(history) =
            parse_message("fetch_agent_timeline_response", payload.clone())
                .unwrap()
                .unwrap()
        else {
            panic!("not history")
        };
        assert!(history.error.is_none());
        assert!(history.projection == Projection::Projected);
        assert!(history.direction == Direction::Tail);
        assert_eq!(history.epoch, payload["epoch"].as_str().unwrap());
        assert_eq!(
            history.window.next_seq,
            payload["window"]["nextSeq"].as_u64().unwrap()
        );
        assert_eq!(
            history.entries.len(),
            payload["entries"].as_array().unwrap().len()
        );
        for (entry, raw) in history
            .entries
            .iter()
            .zip(payload["entries"].as_array().unwrap())
        {
            assert_eq!(entry.seq_start, raw["seqStart"].as_u64().unwrap());
            assert_eq!(entry.seq_end, raw["seqEnd"].as_u64().unwrap());
            assert_eq!(
                entry.source_seq_ranges.len(),
                raw["sourceSeqRanges"].as_array().unwrap().len()
            );
            for (range, raw_range) in entry
                .source_seq_ranges
                .iter()
                .zip(raw["sourceSeqRanges"].as_array().unwrap())
            {
                assert_eq!(range.start_seq, raw_range["startSeq"].as_u64().unwrap());
                assert_eq!(range.end_seq, raw_range["endSeq"].as_u64().unwrap());
            }
        }
        assert!(history.entries.iter().any(|entry| matches!(&entry.item,
            TimelineItem::Assistant(text) if text.text == "FIXTURE_REPLY_中文")));
        for key in ["error", "startCursor", "endCursor", "window", "entries"] {
            let mut invalid = payload.clone();
            invalid.as_object_mut().unwrap().remove(key);
            assert!(decode::<History>(invalid).is_err(), "{key} is required");
        }
        let mut invalid = payload;
        invalid["window"]["nextSeq"] = json!(-1);
        assert!(decode::<History>(invalid).is_err());
        assert!(decode::<Cursor>(json!({"epoch":"e","seq":0.5})).is_err());
        assert!(decode::<Cursor>(json!({"epoch":"e","seq":9_007_199_254_740_992_u64})).is_err());
        assert_eq!(
            serde_json::from_str::<Cursor>(r#"{"epoch":"e","seq":1e0}"#)
                .unwrap()
                .seq,
            1
        );
    }

    #[test]
    fn delivery_acceptance_and_observed_cancellation_are_different_from_rpc_correlation() {
        let ConversationMessage::Sent(sent) = response("chat-send") else {
            panic!("not sent")
        };
        assert!(sent.accepted().is_ok());
        let mut queued_payload = message("chat-followup", "server")["payload"].clone();
        // A normal user's send interrupts. queued is agent-originated delivery, a schema case here.
        queued_payload["deliveryStatus"] = json!("queued");
        let queued: Sent = decode(queued_payload).unwrap();
        assert!(matches!(
            queued.accepted().unwrap(),
            Some(DeliveryStatus::Queued)
        ));
        let ConversationMessage::Sent(rejected) = response("chat-rejected") else {
            panic!("not rejected")
        };
        assert!(rejected.accepted().is_err());
        let ConversationMessage::Cancelled(missing) = response("chat-cancel-missing") else {
            panic!("not cancelled")
        };
        assert!(matches!(missing.state(), CancelState::Failed(_)));
        let mut payload = message("chat-cancel", "server")["payload"].clone();
        for status in ["running", "initializing"] {
            payload["agent"]["status"] = json!(status);
            let cancelled: Cancelled = decode(payload.clone()).unwrap();
            assert!(matches!(cancelled.state(), CancelState::Pending));
        }
        payload["agent"]["status"] = json!("idle");
        payload["agent"]["activeTurn"] = Value::Null;
        let cancelled: Cancelled = decode(payload.clone()).unwrap();
        assert!(matches!(
            cancelled.state(),
            CancelState::Stopped(AgentStatus::Idle)
        ));
        payload["agent"]["activeTurn"] = json!({"turnId":"not-finished","startedAt":null});
        assert!(matches!(
            decode::<Cancelled>(payload.clone()).unwrap().state(),
            CancelState::Pending
        ));
        payload["agent"] = Value::Null;
        assert!(matches!(
            decode::<Cancelled>(payload.clone()).unwrap().state(),
            CancelState::MissingAgent
        ));
        payload["error"] = json!("failed cancellation");
        assert!(matches!(
            decode::<Cancelled>(payload).unwrap().state(),
            CancelState::Failed(_)
        ));
    }

    #[test]
    fn all_permission_kinds_actions_and_resolutions_keep_permission_identity() {
        let event = timeline()
            .into_iter()
            .find(|entry| {
                entry["frame"]["message"]["payload"]["event"]["type"] == "permission_requested"
            })
            .unwrap();
        let original = event["frame"]["message"]["payload"]["event"]["request"].clone();
        for kind in ["tool", "plan", "question", "mode", "other"] {
            let mut request = original.clone();
            request["kind"] = json!(kind);
            request["provider"] = json!("custom-provider");
            request["actions"] = json!([{"id":"implement","label":"Accept","behavior":"allow","variant":"primary","intent":"implement_resume"}]);
            let parsed: PermissionRequest = decode(request.clone()).unwrap();
            assert_eq!(parsed.provider, "custom-provider");
            assert_eq!(serde_json::to_value(parsed.kind).unwrap(), json!(kind));
            assert_eq!(parsed.actions.unwrap()[0].id, "implement");
            let resolved = parse_message("agent_permission_resolved", json!({"agentId":"agent","requestId":parsed.id,
                "resolution":{"behavior":"allow","selectedActionId":"implement","updatedInput":{"answers":["yes"]},"updatedPermissions":[{"scope":"session"}]}})).unwrap().unwrap();
            assert_eq!(resolved.request_id(), None);
            let ConversationMessage::PermissionResolved(resolved) = resolved else {
                panic!("not permission")
            };
            assert!(matches!(
                resolved.resolution,
                PermissionResponse::Allow {
                    selected_action_id: Some(_),
                    ..
                }
            ));
            request["actions"][0]["behavior"] = json!("future");
            assert!(decode::<PermissionRequest>(request).is_err());
        }
        for response in [
            json!({"behavior":"future"}),
            json!({"behavior":"allow","updatedInput":null}),
            json!({"behavior":"deny","interrupt":null}),
        ] {
            assert!(decode::<PermissionResponse>(response).is_err());
        }
    }

    #[test]
    fn catalog_detail_create_and_agent_updates_restore_the_same_permission_snapshot() {
        let mut raw = message("chat-create", "server")["payload"]["agent"].clone();
        let permission = timeline()
            .into_iter()
            .find(|entry| {
                entry["frame"]["message"]["payload"]["event"]["type"] == "permission_requested"
            })
            .unwrap()["frame"]["message"]["payload"]["event"]["request"]
            .clone();
        raw["pendingPermissions"] = json!([permission]);
        raw["activeTurn"] = json!({"turnId":"active","startedAt":null});
        let super::super::catalog::CatalogResponse::Agent(detail) =
            super::super::catalog::parse_response(
                "fetch_agent_response",
                json!({"requestId":"detail","agent":raw,"error":null}),
            )
            .unwrap()
            .unwrap()
        else {
            panic!("not detail")
        };
        let snapshot = Snapshot::from_agent(detail.agent.unwrap()).unwrap();
        assert_eq!(snapshot.pending_permissions[0].id, "fixture-permission");
        assert!(matches!(
            snapshot.active_turn,
            Field::Value(ActiveTurn {
                started_at: None,
                ..
            })
        ));
        assert!(snapshot.agent.extra["availableModes"].is_array());
        assert!(snapshot.agent.extra["capabilities"].is_object());
        let ConversationMessage::Created(created) = parse_message(
            "agent.create.response",
            json!({"requestId":"create","agent":raw,"error":null}),
        )
        .unwrap()
        .unwrap() else {
            panic!("not created")
        };
        assert_eq!(
            created.agent.unwrap().pending_permissions[0].id,
            "fixture-permission"
        );
        let ConversationMessage::AgentUpdate(AgentUpdate::Upsert { agent, .. }) = parse_message(
            "agent_update",
            json!({"kind":"upsert","agent":raw,"seq":1,"generation":"g"}),
        )
        .unwrap()
        .unwrap() else {
            panic!("not upsert")
        };
        assert_eq!(agent.pending_permissions[0].id, "fixture-permission");
        raw["pendingPermissions"] = Value::Null;
        assert!(Snapshot::from_agent(decode(raw.clone()).unwrap()).is_err());
        raw["pendingPermissions"] = json!([]);
        raw.as_object_mut().unwrap().remove("activeTurn");
        assert!(matches!(
            Snapshot::from_agent(decode(raw.clone()).unwrap())
                .unwrap()
                .active_turn,
            Field::Missing
        ));
        raw["activeTurn"] = Value::Null;
        assert!(matches!(
            Snapshot::from_agent(decode(raw.clone()).unwrap())
                .unwrap()
                .active_turn,
            Field::Null
        ));
        raw["activeTurn"] = json!({"turnId":"active"});
        assert!(Snapshot::from_agent(decode(raw).unwrap()).is_err());
    }

    #[test]
    fn optional_nullable_fields_and_request_limits_follow_the_wire_source() {
        let mut config = config();
        config.title = Field::Missing;
        assert!(
            wire(create_agent("id", &config, None, None, None).unwrap())["message"]["config"]
                .get("title")
                .is_none()
        );
        config.title = Field::Null;
        assert_eq!(
            wire(create_agent("id", &config, None, None, None).unwrap())["message"]["config"]["title"],
            Value::Null
        );
        config.title = Field::Value("  title  ".into());
        assert_eq!(
            wire(create_agent("id", &config, None, None, None).unwrap())["message"]["config"]["title"],
            "title"
        );
        config.title = Field::Value("\u{feff}title\u{feff}".into());
        assert_eq!(
            wire(create_agent("id", &config, None, None, None).unwrap())["message"]["config"]["title"],
            "title"
        );
        config.title = Field::Value("\u{85}title\u{85}".into());
        assert_eq!(
            wire(create_agent("id", &config, None, None, None).unwrap())["message"]["config"]["title"],
            "\u{85}title\u{85}"
        );
        config.title = Field::Value(" ".into());
        assert!(create_agent("id", &config, None, None, None).is_err());
        config.title = Field::Value("𐀀".repeat(101));
        assert!(create_agent("id", &config, None, None, None).is_err());
        assert!(subscribe_creation("id", CreationKind::Agent, &"𐀀".repeat(257), None).is_err());
        assert!(subscribe_creation("id", CreationKind::Agent, "", None).is_err());
        let mut cancelled = message("chat-cancel", "server")["payload"].clone();
        cancelled.as_object_mut().unwrap().remove("error");
        assert!(matches!(
            decode::<Cancelled>(cancelled.clone()).unwrap().error,
            Field::Missing
        ));
        cancelled["error"] = Value::Null;
        assert!(matches!(
            decode::<Cancelled>(cancelled).unwrap().error,
            Field::Null
        ));
        assert!(decode::<SessionSubscription>(json!({"requestId":"id"})).is_ok());
        assert!(
            decode::<SessionSubscription>(json!({"requestId":"id","subscriptionId":null})).is_err()
        );
        let mut created = message("chat-create", "server")["payload"].clone();
        created["creation"] = Value::Null;
        let error = decode::<Created>(created).err().unwrap();
        assert!(!format!("{error:?}").contains("Conversation fixture"));
    }

    #[test]
    fn critical_tool_and_text_fields_are_checked_while_unknown_details_are_retained() {
        let mut tool = json!({"type":"tool_call","callId":"c","name":"shell","status":"running",
            "detail":{"type":"shell","command":"echo private","future":{"key":true}},"error":null});
        let TimelineItem::Tool(parsed) = decode::<TimelineItem>(tool.clone()).unwrap() else {
            panic!("not tool")
        };
        assert_eq!(parsed.detail.fields["future"]["key"], true);
        tool["detail"]["command"] = Value::Null;
        assert!(decode::<TimelineItem>(tool.clone()).is_err());
        tool["detail"]["command"] = json!("echo");
        tool["status"] = json!("failed");
        assert!(decode::<TimelineItem>(tool.clone()).is_err());
        tool["error"] = json!({"code":"failure"});
        assert!(decode::<TimelineItem>(tool.clone()).is_ok());
        tool.as_object_mut().unwrap().remove("error");
        assert!(decode::<TimelineItem>(tool).is_err());
        for item in [
            json!({"type":"assistant_message"}),
            json!({"type":"user_message","text":null}),
            json!({"type":"reasoning","text":42}),
        ] {
            assert!(decode::<TimelineItem>(item).is_err());
        }
        assert!(
            decode::<TimelineItem>(json!({"type":"user_message","text":"","messageId":null}))
                .is_err()
        );
        // messageId is defined on messages, not reasoning; unknown extension values remain raw.
        let TimelineItem::Reasoning(reasoning) =
            decode::<TimelineItem>(json!({"type":"reasoning","text":"thought","messageId":null}))
                .unwrap()
        else {
            panic!("not reasoning")
        };
        assert!(reasoning.message_id.is_none());
        assert_eq!(reasoning.extra["messageId"], Value::Null);
    }
}
