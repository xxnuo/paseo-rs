//! Local chat transactions. Transport receipts never stand in for provider/turn confirmation.

use crate::protocol::{
    catalog::Field,
    conversation::{
        ActiveTurn, CancelState, Cancelled, DeliveryStatus, KnownStreamEvent, PermissionBehavior,
        PermissionKind, PermissionRequest, PermissionResponse, Sent, Stream, StreamEvent,
        TimelineItem,
    },
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    rc::Rc,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SendStatus {
    Sending,
    Accepted,
    Queued,
    UnknownOutcome,
    Rejected,
    Observed,
}

// No Debug: drafts, messages, errors, permission input and responses can contain private data.
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PendingMessage {
    pub(crate) agent_id: String,
    pub(crate) client_message_id: String,
    pub(crate) text: String,
    pub(crate) status: SendStatus,
    pub(crate) attempt: u64,
    pub(crate) error: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PermissionStatus {
    Pending,
    Submitting,
    Failed,
}

#[derive(Clone)]
pub(crate) struct PendingPermission {
    pub(crate) agent_id: String,
    pub(crate) request: Rc<PermissionRequest>,
    pub(crate) status: PermissionStatus,
    pub(crate) response: Option<Rc<PermissionResponse>>,
    pub(crate) attempt: u64,
    pub(crate) error: Option<String>,
    revision: u64,
}

#[derive(Clone)]
pub(crate) struct Turn {
    pub(crate) turn_id: Option<String>,
    pub(crate) started_at: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CancelStatus {
    Waiting,
    UnknownOutcome,
    Confirmed,
    Failed,
}

#[derive(Clone)]
pub(crate) struct Cancellation {
    pub(crate) target_turn_id: Option<String>,
    pub(crate) attempt: u64,
    pub(crate) status: CancelStatus,
    pub(crate) error: Option<String>,
}

#[derive(Default)]
pub(crate) struct ConversationState {
    messages: Vec<PendingMessage>,
    permissions: Vec<PendingPermission>,
    resolved_permissions: HashSet<(String, String)>,
    permission_revision: u64,
    turns: HashMap<String, Turn>,
    cancellations: HashMap<String, Cancellation>,
}

impl ConversationState {
    pub(crate) fn pending_messages(&self) -> &[PendingMessage] {
        &self.messages
    }

    pub(crate) fn permissions(&self) -> &[PendingPermission] {
        &self.permissions
    }

    pub(crate) fn turn(&self, agent_id: &str) -> Option<&Turn> {
        self.turns.get(agent_id)
    }

    pub(crate) fn cancellation(&self, agent_id: &str) -> Option<&Cancellation> {
        self.cancellations.get(agent_id)
    }

    pub(crate) fn pending_records(&self) -> Vec<PendingMessage> {
        self.messages
            .iter()
            .filter(|message| message.status != SendStatus::Observed)
            .cloned()
            .collect()
    }

    pub(crate) fn restore_pending(&mut self, records: Vec<PendingMessage>) {
        for mut record in records {
            if record.status == SendStatus::Observed
                || self
                    .message(&record.agent_id, &record.client_message_id)
                    .is_some()
            {
                continue;
            }
            if record.status == SendStatus::Sending {
                record.status = SendStatus::UnknownOutcome;
            }
            self.messages.push(record);
        }
    }

    pub(crate) fn begin_send(
        &mut self,
        agent_id: &str,
        client_message_id: &str,
        text: &str,
    ) -> Option<u64> {
        if client_message_id.is_empty() || self.message(agent_id, client_message_id).is_some() {
            return None;
        }
        self.messages.push(PendingMessage {
            agent_id: agent_id.into(),
            client_message_id: client_message_id.into(),
            text: text.into(),
            status: SendStatus::Sending,
            attempt: 1,
            error: None,
        });
        Some(1)
    }

    // Unknown delivery is retried with its original identity; a rejected/new prompt gets a new ID.
    pub(crate) fn retry_send(&mut self, agent_id: &str, client_message_id: &str) -> Option<u64> {
        let message = self.message(agent_id, client_message_id)?;
        if message.status != SendStatus::UnknownOutcome {
            return None;
        }
        message.attempt = message.attempt.checked_add(1)?;
        message.status = SendStatus::Sending;
        message.error = None;
        Some(message.attempt)
    }

    pub(crate) fn sent(
        &mut self,
        agent_id: &str,
        client_message_id: &str,
        attempt: u64,
        reply: &Sent,
    ) {
        if reply.agent_id != agent_id {
            return;
        }
        let Some(message) = self.message(agent_id, client_message_id) else {
            return;
        };
        if attempt == 0 || attempt > message.attempt || message.status == SendStatus::Observed {
            return;
        }
        match reply.accepted() {
            Ok(delivery) => {
                if message.status != SendStatus::Queued {
                    message.status = if matches!(delivery, Some(DeliveryStatus::Queued)) {
                        SendStatus::Queued
                    } else {
                        SendStatus::Accepted
                    };
                }
                message.error = None;
            }
            Err(error)
                if attempt == message.attempt
                    && !matches!(message.status, SendStatus::Accepted | SendStatus::Queued) =>
            {
                message.status = SendStatus::Rejected;
                message.error = Some(error.into());
            }
            Err(_) => {}
        }
    }

    pub(crate) fn send_unknown(
        &mut self,
        agent_id: &str,
        client_message_id: &str,
        attempt: u64,
        error: &str,
    ) {
        if let Some(message) = self.message(agent_id, client_message_id)
            && message.attempt == attempt
            && message.status == SendStatus::Sending
        {
            message.status = SendStatus::UnknownOutcome;
            message.error = Some(error.into());
        }
    }

    pub(crate) fn observe_user(&mut self, agent_id: &str, item: &TimelineItem) {
        if let TimelineItem::User(text) = item
            && let Some(id) = &text.client_message_id
            && let Some(message) = self.message(agent_id, id)
        {
            message.status = SendStatus::Observed;
            message.error = None;
        }
    }

    fn message(&mut self, agent_id: &str, client_message_id: &str) -> Option<&mut PendingMessage> {
        self.messages.iter_mut().find(|message| {
            message.agent_id == agent_id && message.client_message_id == client_message_id
        })
    }

    pub(crate) fn snapshot_turn(&mut self, agent_id: &str, active_turn: &Field<ActiveTurn>) {
        match active_turn {
            Field::Value(turn) => self.open_turn(
                agent_id,
                Some(turn.turn_id.clone()),
                turn.started_at.clone(),
            ),
            Field::Null => {
                self.turns.remove(agent_id);
            }
            Field::Missing => {}
        }
    }

    fn open_turn(&mut self, agent_id: &str, turn_id: Option<String>, started_at: Option<String>) {
        let was_idle = !self.turns.contains_key(agent_id);
        if was_idle
            && let Some(cancel) = self.cancellations.get_mut(agent_id)
            && cancel.target_turn_id.is_none()
            && matches!(
                cancel.status,
                CancelStatus::Waiting | CancelStatus::UnknownOutcome
            )
        {
            cancel.target_turn_id = turn_id.clone();
        }
        let started_at = self
            .turns
            .get(agent_id)
            .filter(|turn| turn.turn_id == turn_id)
            .and_then(|turn| turn.started_at.clone())
            .or(started_at);
        self.turns.insert(
            agent_id.into(),
            Turn {
                turn_id,
                started_at,
            },
        );
    }

    fn close_turn(&mut self, agent_id: &str, turn_id: Option<&str>) {
        if self
            .turns
            .get(agent_id)
            .is_some_and(|turn| turn.turn_id.as_deref() == turn_id)
        {
            self.turns.remove(agent_id);
        }
        if let Some(cancel) = self.cancellations.get_mut(agent_id)
            && cancel.target_turn_id.as_deref() == turn_id
        {
            cancel.status = CancelStatus::Confirmed;
            cancel.error = None;
        }
    }

    pub(crate) fn begin_cancel(&mut self, agent_id: &str) -> Option<u64> {
        let target_turn_id = self
            .turns
            .get(agent_id)
            .and_then(|turn| turn.turn_id.clone());
        if self.cancellations.get(agent_id).is_some_and(|cancel| {
            cancel.status == CancelStatus::Waiting && cancel.target_turn_id == target_turn_id
        }) {
            return None;
        }
        let attempt = self
            .cancellations
            .get(agent_id)
            .map_or(Some(1), |cancel| cancel.attempt.checked_add(1))?;
        self.cancellations.insert(
            agent_id.into(),
            Cancellation {
                target_turn_id,
                attempt,
                status: CancelStatus::Waiting,
                error: None,
            },
        );
        Some(attempt)
    }

    pub(crate) fn cancelled(&mut self, agent_id: &str, attempt: u64, reply: &Cancelled) {
        if reply.agent_id != agent_id {
            return;
        }
        let Some(cancel) = self.cancellations.get_mut(agent_id) else {
            return;
        };
        if cancel.attempt != attempt || cancel.status == CancelStatus::Confirmed {
            return;
        }
        match reply.state() {
            CancelState::Stopped(_) => {
                cancel.status = CancelStatus::Confirmed;
                cancel.error = None;
                if self
                    .turns
                    .get(agent_id)
                    .is_some_and(|turn| turn.turn_id == cancel.target_turn_id)
                {
                    self.turns.remove(agent_id);
                }
            }
            CancelState::Failed(error) => {
                cancel.status = CancelStatus::Failed;
                cancel.error = Some(error.into());
            }
            CancelState::MissingAgent => {
                cancel.status = CancelStatus::Failed;
                cancel.error = Some("Agent unavailable".into());
            }
            CancelState::Pending => {}
        }
    }

    pub(crate) fn cancel_unknown(&mut self, agent_id: &str, attempt: u64, error: &str) {
        if let Some(cancel) = self.cancellations.get_mut(agent_id)
            && cancel.attempt == attempt
            && cancel.status == CancelStatus::Waiting
        {
            cancel.status = CancelStatus::UnknownOutcome;
            cancel.error = Some(error.into());
        }
    }

    pub(crate) fn permission_revision(&self) -> u64 {
        self.permission_revision
    }

    pub(crate) fn request_permission(&mut self, agent_id: &str, request: Rc<PermissionRequest>) {
        if self
            .resolved_permissions
            .contains(&(agent_id.into(), request.id.clone()))
        {
            return;
        }
        self.permission_revision += 1;
        if let Some(permission) = self.permissions.iter_mut().find(|permission| {
            permission.agent_id == agent_id && permission.request.id == request.id
        }) {
            permission.request = request;
            permission.revision = self.permission_revision;
        } else {
            self.permissions.push(PendingPermission {
                agent_id: agent_id.into(),
                request,
                status: PermissionStatus::Pending,
                response: None,
                attempt: 0,
                error: None,
                revision: self.permission_revision,
            });
        }
    }

    pub(crate) fn resolve_permission(&mut self, agent_id: &str, request_id: &str) {
        self.permission_revision += 1;
        self.resolved_permissions
            .insert((agent_id.into(), request_id.into()));
        self.permissions.retain(|permission| {
            permission.agent_id != agent_id || permission.request.id != request_id
        });
    }

    // Capture started_revision before fetching: a snapshot cannot erase later live requests.
    pub(crate) fn sync_permissions(
        &mut self,
        agent_id: &str,
        started_revision: u64,
        requests: impl IntoIterator<Item = Rc<PermissionRequest>>,
    ) {
        let requests: Vec<_> = requests.into_iter().collect();
        let removed: Vec<_> = self
            .permissions
            .iter()
            .filter(|permission| {
                permission.agent_id == agent_id
                    && permission.revision <= started_revision
                    && permission.status != PermissionStatus::Submitting
                    && !requests
                        .iter()
                        .any(|request| request.id == permission.request.id)
            })
            .map(|permission| permission.request.id.clone())
            .collect();
        for id in removed {
            self.resolve_permission(agent_id, &id);
        }
        for request in requests {
            if self.permissions.iter().any(|permission| {
                permission.agent_id == agent_id
                    && permission.request.id == request.id
                    && permission.revision > started_revision
            }) {
                continue;
            }
            self.request_permission(agent_id, request);
        }
    }

    pub(crate) fn begin_permission_response(
        &mut self,
        agent_id: &str,
        request_id: &str,
        response: PermissionResponse,
    ) -> Option<u64> {
        let permission = self.permissions.iter_mut().find(|permission| {
            permission.agent_id == agent_id && permission.request.id == request_id
        })?;
        if permission.status == PermissionStatus::Submitting
            || !valid_response(&permission.request, &response)
        {
            return None;
        }
        permission.attempt = permission.attempt.checked_add(1)?;
        self.permission_revision += 1;
        permission.revision = self.permission_revision;
        permission.status = PermissionStatus::Submitting;
        permission.response = Some(Rc::new(response));
        permission.error = None;
        Some(permission.attempt)
    }

    pub(crate) fn permission_response_failed(
        &mut self,
        agent_id: &str,
        request_id: &str,
        attempt: u64,
        error: &str,
    ) {
        if let Some(permission) = self.permissions.iter_mut().find(|permission| {
            permission.agent_id == agent_id && permission.request.id == request_id
        }) && permission.attempt == attempt
            && permission.status == PermissionStatus::Submitting
        {
            self.permission_revision += 1;
            permission.revision = self.permission_revision;
            permission.status = PermissionStatus::Failed;
            permission.error = Some(error.into());
        }
    }

    pub(crate) fn stream(&mut self, stream: &Stream) {
        let StreamEvent::Known(event) = &stream.event else {
            return;
        };
        match event {
            KnownStreamEvent::TurnStarted { turn_id, .. } => self.open_turn(
                &stream.agent_id,
                turn_id.clone(),
                Some(stream.timestamp.clone()),
            ),
            KnownStreamEvent::TurnCompleted { turn_id, .. }
            | KnownStreamEvent::TurnFailed { turn_id, .. }
            | KnownStreamEvent::TurnCanceled { turn_id, .. } => {
                self.close_turn(&stream.agent_id, turn_id.as_deref())
            }
            KnownStreamEvent::Timeline { item, .. } => self.observe_user(&stream.agent_id, item),
            KnownStreamEvent::PermissionRequested { request, .. } => {
                self.request_permission(&stream.agent_id, Rc::new(request.clone()))
            }
            KnownStreamEvent::PermissionResolved { request_id, .. } => {
                self.resolve_permission(&stream.agent_id, request_id)
            }
            _ => {}
        }
    }

    pub(crate) fn disconnected(&mut self, error: &str) {
        for message in &mut self.messages {
            if message.status == SendStatus::Sending {
                message.status = SendStatus::UnknownOutcome;
                message.error = Some(error.into());
            }
        }
        for permission in &mut self.permissions {
            if permission.status == PermissionStatus::Submitting {
                self.permission_revision += 1;
                permission.revision = self.permission_revision;
                permission.status = PermissionStatus::Failed;
                permission.error = Some(error.into());
            }
        }
        for cancel in self.cancellations.values_mut() {
            if cancel.status == CancelStatus::Waiting {
                cancel.status = CancelStatus::UnknownOutcome;
                cancel.error = Some(error.into());
            }
        }
    }
}

fn valid_response(request: &PermissionRequest, response: &PermissionResponse) -> bool {
    let (selected, allow) = match response {
        PermissionResponse::Allow {
            selected_action_id,
            updated_input,
            ..
        } => {
            if request.kind == PermissionKind::Question && updated_input.is_none() {
                return false;
            }
            (selected_action_id.as_deref(), true)
        }
        PermissionResponse::Deny {
            selected_action_id, ..
        } => (selected_action_id.as_deref(), false),
    };
    match (
        request
            .actions
            .as_deref()
            .filter(|actions| !actions.is_empty()),
        selected,
    ) {
        (Some(actions), Some(id)) => actions.iter().any(|action| {
            action.id == id
                && matches!(
                    (&action.behavior, allow),
                    (PermissionBehavior::Allow, true) | (PermissionBehavior::Deny, false)
                )
        }),
        (None, None) => true,
        (Some(_), None) if request.kind == PermissionKind::Question => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn sent(accepted: bool, queued: bool) -> Sent {
        let mut value = json!({"requestId":"wire-id", "agentId":"agent", "accepted":accepted,
            "error":if accepted { Value::Null } else { json!("Rejected") }});
        if queued {
            value["deliveryStatus"] = json!("queued");
        }
        serde_json::from_value(value).unwrap()
    }

    fn user(id: &str) -> TimelineItem {
        serde_json::from_value(
            json!({"type":"user_message", "text":"canonical", "clientMessageId":id}),
        )
        .unwrap()
    }

    fn permission(id: &str) -> Rc<PermissionRequest> {
        Rc::new(
            serde_json::from_value(
                json!({"id":id, "provider":"test", "name":"tool", "kind":"tool",
            "input":{"secret":"preserved"}, "metadata":{"extension":true},
            "actions":[{"id":"implement", "label":"Run", "behavior":"allow"},
                {"id":"stop", "label":"Stop", "behavior":"deny"}]}),
            )
            .unwrap(),
        )
    }

    fn allow(id: Option<&str>) -> PermissionResponse {
        PermissionResponse::Allow {
            selected_action_id: id.map(str::to_owned),
            updated_input: None,
            updated_permissions: None,
        }
    }

    fn stopped() -> Cancelled {
        let frames: Vec<Value> =
            serde_json::from_str(include_str!("../../../protocol-fixtures/conversation.json"))
                .unwrap();
        let mut payload = frames
            .iter()
            .find(|frame| {
                frame["direction"] == "server"
                    && frame["frame"]["message"]["payload"]["requestId"] == "chat-cancel"
            })
            .unwrap()["frame"]["message"]["payload"]
            .clone();
        payload["agentId"] = json!("agent");
        payload["agent"]["status"] = json!("idle");
        payload["agent"]["activeTurn"] = Value::Null;
        serde_json::from_value(payload).unwrap()
    }

    #[test]
    fn independent_submissions_keep_original_identity_text_and_success() {
        let mut state = ConversationState::default();
        assert_eq!(state.begin_send("agent", "one", "first"), Some(1));
        assert_eq!(state.begin_send("agent", "two", "second"), Some(1));
        assert_eq!(state.begin_send("agent", "one", "duplicate"), None);
        state.sent("agent", "one", 1, &sent(true, true));
        state.send_unknown("agent", "one", 1, "Late timeout");
        state.sent("agent", "one", 1, &sent(false, false));
        assert_eq!(state.pending_messages()[0].status, SendStatus::Queued);
        assert_eq!(state.pending_messages()[1].status, SendStatus::Sending);
        assert_eq!(state.pending_messages()[0].text, "first");
        state.observe_user("agent", &user("one"));
        state.sent("agent", "one", 1, &sent(false, false));
        assert_eq!(state.pending_messages()[0].status, SendStatus::Observed);
        assert_eq!(state.pending_records().len(), 1);
    }

    #[test]
    fn canonical_before_rpc_and_transport_failure_cannot_roll_back() {
        let mut state = ConversationState::default();
        state.begin_send("agent", "one", "first");
        state.observe_user("other-agent", &user("one"));
        assert_eq!(state.pending_messages()[0].status, SendStatus::Sending);
        state.observe_user("agent", &user("one"));
        state.sent("agent", "one", 1, &sent(true, false));
        state.disconnected("Offline");
        state.send_unknown("agent", "one", 1, "Timeout");
        assert_eq!(state.pending_messages()[0].status, SendStatus::Observed);
    }

    #[test]
    fn retry_unknown_reuses_id_and_ignores_old_failures_but_accepts_old_success() {
        let mut state = ConversationState::default();
        state.begin_send("agent", "stable-id", "first");
        state.disconnected("Offline");
        assert_eq!(
            state.pending_messages()[0].status,
            SendStatus::UnknownOutcome
        );
        assert_eq!(state.retry_send("agent", "stable-id"), Some(2));
        state.sent("agent", "stable-id", 1, &sent(false, false));
        state.send_unknown("agent", "stable-id", 1, "Old timeout");
        assert_eq!(state.pending_messages()[0].status, SendStatus::Sending);
        state.sent("agent", "stable-id", 1, &sent(true, false));
        state.send_unknown("agent", "stable-id", 2, "New timeout");
        assert_eq!(state.pending_messages()[0].status, SendStatus::Accepted);
        assert_eq!(state.pending_messages()[0].client_message_id, "stable-id");
        assert_eq!(state.retry_send("agent", "stable-id"), None);
    }

    #[test]
    fn explicit_rejection_allows_a_new_message_without_reusing_old_identity() {
        let mut state = ConversationState::default();
        state.begin_send("agent", "one", "first");
        state.sent("agent", "one", 1, &sent(false, false));
        assert_eq!(state.pending_messages()[0].status, SendStatus::Rejected);
        assert_eq!(state.retry_send("agent", "one"), None);
        assert_eq!(state.begin_send("agent", "two", "first"), Some(1));
    }

    #[test]
    fn persisted_sends_are_unknown_after_restart_and_canonical_records_are_omitted() {
        let mut state = ConversationState::default();
        state.begin_send("agent", "one", "first");
        state.begin_send("agent", "two", "second");
        state.observe_user("agent", &user("two"));
        let serialized = serde_json::to_string(&state.pending_records()).unwrap();
        let mut restored = ConversationState::default();
        restored.restore_pending(serde_json::from_str(&serialized).unwrap());
        restored.restore_pending(serde_json::from_str(&serialized).unwrap());
        assert_eq!(restored.pending_messages().len(), 1);
        assert_eq!(
            restored.pending_messages()[0].status,
            SendStatus::UnknownOutcome
        );
        assert_eq!(restored.pending_messages()[0].text, "first");
        assert_eq!(restored.retry_send("agent", "one"), Some(2));
    }

    #[test]
    fn cancellation_waits_for_confirmation_and_is_bound_to_its_turn() {
        let mut state = ConversationState::default();
        state.open_turn("agent", Some("old".into()), None);
        assert_eq!(state.begin_cancel("agent"), Some(1));
        let mut pending = stopped();
        pending.agent.as_mut().unwrap().active_turn = Field::Value(ActiveTurn {
            turn_id: "old".into(),
            started_at: None,
        });
        state.cancelled("agent", 1, &pending);
        assert_eq!(
            state.cancellation("agent").unwrap().status,
            CancelStatus::Waiting
        );
        state.open_turn("agent", Some("new".into()), None);
        state.cancelled("agent", 1, &stopped());
        assert_eq!(
            state
                .cancellation("agent")
                .unwrap()
                .target_turn_id
                .as_deref(),
            Some("old")
        );
        assert_eq!(
            state.cancellation("agent").unwrap().status,
            CancelStatus::Confirmed
        );
        assert_eq!(state.turn("agent").unwrap().turn_id.as_deref(), Some("new"));
        state.close_turn("agent", Some("old"));
        state.close_turn("agent", None);
        assert_eq!(state.turn("agent").unwrap().turn_id.as_deref(), Some("new"));
    }

    #[test]
    fn late_cancel_rpc_and_old_terminal_event_cannot_clear_a_new_cancellation() {
        let mut state = ConversationState::default();
        state.open_turn("agent", Some("old".into()), None);
        state.begin_cancel("agent");
        state.open_turn("agent", Some("new".into()), None);
        assert_eq!(state.begin_cancel("agent"), Some(2));
        state.cancelled("agent", 1, &stopped());
        state.close_turn("agent", Some("old"));
        assert_eq!(
            state.cancellation("agent").unwrap().status,
            CancelStatus::Waiting
        );
        assert!(state.turn("agent").is_some());
        state.close_turn("agent", Some("new"));
        assert_eq!(
            state.cancellation("agent").unwrap().status,
            CancelStatus::Confirmed
        );
        assert!(state.turn("agent").is_none());
    }

    #[test]
    fn cancellation_survives_send_to_turn_handoff_and_disconnect() {
        let mut state = ConversationState::default();
        state.begin_cancel("agent");
        state.cancel_unknown("agent", 1, "Offline");
        state.open_turn("agent", Some("first-turn".into()), Some("started".into()));
        assert_eq!(
            state
                .cancellation("agent")
                .unwrap()
                .target_turn_id
                .as_deref(),
            Some("first-turn")
        );
        assert_eq!(
            state.cancellation("agent").unwrap().status,
            CancelStatus::UnknownOutcome
        );
        state.close_turn("agent", Some("first-turn"));
        state.cancel_unknown("agent", 1, "Late timeout");
        assert_eq!(
            state.cancellation("agent").unwrap().status,
            CancelStatus::Confirmed
        );
    }

    #[test]
    fn both_permission_channels_deduplicate_and_resolved_cannot_be_resurrected() {
        let mut state = ConversationState::default();
        state.request_permission("agent", permission("same-id"));
        state.sync_permissions(
            "agent",
            state.permission_revision(),
            [permission("same-id")],
        );
        state.request_permission("other-agent", permission("same-id"));
        assert_eq!(state.permissions().len(), 2);
        state.resolve_permission("agent", "same-id");
        state.sync_permissions("agent", 0, [permission("same-id")]);
        state.request_permission("agent", permission("same-id"));
        assert_eq!(state.permissions().len(), 1);
        assert_eq!(state.permissions()[0].agent_id, "other-agent");
        assert_eq!(
            state.permissions()[0].request.input.as_ref().unwrap()["secret"],
            "preserved"
        );
    }

    #[test]
    fn stale_snapshot_cannot_erase_new_live_permission_or_finish_a_socket_write() {
        let mut state = ConversationState::default();
        let revision = state.permission_revision();
        state.request_permission("agent", permission("live"));
        state.sync_permissions("agent", revision, []);
        assert_eq!(state.permissions().len(), 1);
        assert_eq!(
            state.begin_permission_response("agent", "live", allow(Some("implement"))),
            Some(1)
        );
        state.sync_permissions("agent", state.permission_revision(), []);
        assert_eq!(state.permissions()[0].status, PermissionStatus::Submitting);
        state.resolve_permission("agent", "live");
        state.permission_response_failed("agent", "live", 1, "Late failure");
        assert!(state.permissions().is_empty());
    }

    #[test]
    fn permission_failure_preserves_request_and_response_for_retry() {
        let mut state = ConversationState::default();
        state.request_permission("agent", permission("p"));
        assert_eq!(
            state.begin_permission_response("agent", "p", allow(None)),
            None
        );
        assert_eq!(
            state.begin_permission_response("agent", "p", allow(Some("stop"))),
            None
        );
        assert_eq!(
            state.begin_permission_response("agent", "p", allow(Some("implement"))),
            Some(1)
        );
        state.permission_response_failed("agent", "p", 1, "Timeout");
        assert_eq!(state.permissions()[0].status, PermissionStatus::Failed);
        assert!(state.permissions()[0].response.is_some());
        assert_eq!(
            state.begin_permission_response("agent", "p", allow(Some("implement"))),
            Some(2)
        );
        state.permission_response_failed("agent", "p", 1, "Old failure");
        assert_eq!(state.permissions()[0].status, PermissionStatus::Submitting);
        state.disconnected("Offline");
        assert_eq!(state.permissions()[0].status, PermissionStatus::Failed);
    }

    #[test]
    fn absent_actions_need_no_invented_id_and_questions_require_explicit_input() {
        let mut state = ConversationState::default();
        let question: PermissionRequest = serde_json::from_value(
            json!({"id":"question", "provider":"test", "name":"ask", "kind":"question"}),
        )
        .unwrap();
        state.request_permission("agent", Rc::new(question));
        assert_eq!(
            state.begin_permission_response("agent", "question", allow(None)),
            None
        );
        let updated_input = json!({"answers":{"Choice":"A"}})
            .as_object()
            .unwrap()
            .clone();
        assert_eq!(
            state.begin_permission_response(
                "agent",
                "question",
                PermissionResponse::Allow {
                    selected_action_id: None,
                    updated_input: Some(updated_input.clone()),
                    updated_permissions: None,
                }
            ),
            Some(1)
        );
        assert!(
            matches!(&**state.permissions()[0].response.as_ref().unwrap(), PermissionResponse::Allow { updated_input:Some(input), .. } if input == &updated_input)
        );
        let tool: PermissionRequest = serde_json::from_value(
            json!({"id":"tool", "provider":"test", "name":"tool", "kind":"tool"}),
        )
        .unwrap();
        state.request_permission("agent", Rc::new(tool));
        assert_eq!(
            state.begin_permission_response("agent", "tool", allow(Some("invented"))),
            None
        );
        assert_eq!(
            state.begin_permission_response("agent", "tool", allow(None)),
            Some(1)
        );
    }

    #[test]
    fn snapshot_started_before_a_failed_submission_keeps_the_retryable_request() {
        let mut state = ConversationState::default();
        state.request_permission("agent", permission("p"));
        let revision = state.permission_revision();
        state.begin_permission_response("agent", "p", allow(Some("implement")));
        state.permission_response_failed("agent", "p", 1, "Timeout");
        state.sync_permissions("agent", revision, []);
        assert_eq!(state.permissions()[0].status, PermissionStatus::Failed);
        state.sync_permissions("agent", state.permission_revision(), []);
        assert!(state.permissions().is_empty());
        state.request_permission("agent", permission("p"));
        assert!(state.permissions().is_empty());
    }

    #[test]
    fn streams_apply_user_turn_and_permission_channels_and_ignore_unknown_events() {
        let mut state = ConversationState::default();
        state.begin_send("agent", "one", "first");
        for event in [
            json!({"type":"turn_started", "provider":"test", "turnId":"turn"}),
            json!({"type":"timeline", "provider":"test", "item":{"type":"user_message", "text":"first", "clientMessageId":"one"}}),
            json!({"type":"permission_requested", "provider":"test", "request":{"id":"p", "provider":"test", "name":"tool", "kind":"tool"}}),
            json!({"type":"future_event", "secret":"ignored"}),
        ] {
            let stream: Stream = serde_json::from_value(
                json!({"agentId":"agent", "timestamp":"now", "event":event}),
            )
            .unwrap();
            state.stream(&stream);
        }
        assert_eq!(state.pending_messages()[0].status, SendStatus::Observed);
        assert_eq!(
            state.turn("agent").unwrap().turn_id.as_deref(),
            Some("turn")
        );
        assert_eq!(state.permissions().len(), 1);
        state.snapshot_turn("agent", &Field::Missing);
        assert!(state.turn("agent").is_some());
        let stream: Stream = serde_json::from_value(json!({"agentId":"agent", "timestamp":"later", "event":{
            "type":"permission_resolved", "provider":"test", "requestId":"p", "resolution":{"behavior":"allow"}
        }})).unwrap();
        state.stream(&stream);
        assert!(state.permissions().is_empty());
        state.snapshot_turn("agent", &Field::Null);
        assert!(state.turn("agent").is_none());
    }
}
