//! Read-state triggers from app/hooks/use-agent-attention-clear.ts and utils/agent-attention.ts.

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Trigger {
    FocusEntry,
    InputFocus,
    PromptSend,
    AgentBlur,
}

impl Trigger {
    pub(crate) fn queued(self, previous: Option<Self>) -> Self {
        // Automatic focus entry must never replace a real interaction or be promoted to one.
        previous
            .filter(|trigger| *trigger != Self::FocusEntry)
            .unwrap_or(self)
    }
}

#[derive(Default)]
pub(crate) struct Attention {
    required: bool,
    permission: bool,
    deferred: bool,
    viewed: bool,
    known: bool,
}

impl Attention {
    pub(crate) fn update(&mut self, required: bool, permission: bool) {
        self.known = true;
        if !required {
            self.deferred = false;
        } else if !self.required && self.viewed {
            // A newly completed reply remains unread until an interaction or leaving the Agent.
            self.deferred = true;
        }
        self.required = required;
        self.permission = permission;
    }

    pub(crate) fn snapshot(&mut self, required: bool, permission: bool) -> bool {
        if self.known {
            self.update(required, permission);
            return false;
        }
        self.required = required;
        self.permission = permission;
        self.known = true;
        self.viewed && self.trigger(Trigger::FocusEntry)
    }

    pub(crate) fn visibility(&mut self, viewed: bool) -> bool {
        let entered = viewed && !self.viewed;
        self.viewed = viewed;
        entered && self.trigger(Trigger::FocusEntry)
    }

    pub(crate) fn trigger(&mut self, trigger: Trigger) -> bool {
        if !self.required || self.permission || trigger == Trigger::FocusEntry && self.deferred {
            return false;
        }
        self.deferred = false;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn existing_attention_clears_on_entry_but_newly_viewed_reply_waits_for_interaction() {
        let mut state = Attention::default();
        state.update(true, false);
        assert!(state.visibility(true));
        state.update(false, false);
        state.update(true, false);
        assert!(!state.visibility(true));
        assert!(!state.visibility(false));
        assert!(!state.visibility(true));
        assert!(state.trigger(Trigger::InputFocus));
    }

    #[test]
    fn background_reply_clears_when_resuming_and_permission_is_never_marked_read() {
        let mut state = Attention::default();
        state.visibility(true);
        state.visibility(false);
        state.update(true, false);
        assert!(state.visibility(true));
        state.update(true, true);
        for trigger in [
            Trigger::FocusEntry,
            Trigger::InputFocus,
            Trigger::PromptSend,
            Trigger::AgentBlur,
        ] {
            assert!(!state.trigger(trigger));
        }
    }

    #[test]
    fn newly_finished_attention_clears_on_send_or_blur_and_reset_removes_defer() {
        for trigger in [Trigger::PromptSend, Trigger::AgentBlur] {
            let mut state = Attention::default();
            state.visibility(true);
            state.update(true, false);
            assert!(state.trigger(trigger));
            state.update(false, false);
            state.visibility(false);
            state.update(true, false);
            assert!(state.visibility(true));
        }
    }

    #[test]
    fn initial_snapshot_clears_existing_attention_but_does_not_erase_a_prior_live_event() {
        let mut state = Attention::default();
        state.visibility(true);
        assert!(state.snapshot(true, false));
        state.update(false, false);
        state.update(true, false);
        assert!(!state.snapshot(true, false));
        assert!(!state.trigger(Trigger::FocusEntry));
    }

    #[test]
    fn queued_automatic_entry_cannot_clear_a_new_foreground_reply() {
        let mut state = Attention::default();
        state.update(true, false);
        assert!(state.visibility(true));
        state.visibility(false);
        assert!(state.visibility(true));
        let queued = Trigger::FocusEntry.queued(None);
        state.update(false, false);
        state.update(true, false);
        assert!(!state.trigger(queued));
        let interaction = Trigger::InputFocus.queued(Some(queued));
        assert!(Trigger::FocusEntry.queued(Some(interaction)) == Trigger::InputFocus);
        assert!(state.trigger(interaction));
    }
}
