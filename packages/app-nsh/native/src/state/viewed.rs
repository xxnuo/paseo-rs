//! Root-owned viewed conversations, scoped to a daemon identity rather than its address.

use super::{conversation::ConversationState, timeline::Timeline};
use std::{cell::RefCell, collections::HashMap, rc::Rc};

#[derive(Clone)]
pub(crate) struct ViewedConversation {
    pub(crate) timeline: Rc<RefCell<Timeline>>,
    pub(crate) transactions: Rc<RefCell<ConversationState>>,
}

#[derive(Default)]
pub(crate) struct ViewedConversations {
    entries: HashMap<(String, String), ViewedConversation>,
}

impl ViewedConversations {
    pub(crate) fn get(&mut self, server_id: &str, agent_id: &str) -> ViewedConversation {
        self.entries
            .entry((server_id.to_owned(), agent_id.to_owned()))
            .or_insert_with(|| ViewedConversation {
                timeline: Rc::new(RefCell::new(Timeline::new(agent_id.to_owned()))),
                transactions: Rc::new(RefCell::new(ConversationState::default())),
            })
            .clone()
    }

    pub(crate) fn clear(&mut self) {
        self.entries.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retained_conversations_are_scoped_to_both_daemon_and_agent() {
        let mut cache = ViewedConversations::default();
        let original = cache.get("server-a", "agent-a");
        let reopened = cache.get("server-a", "agent-a");
        assert!(Rc::ptr_eq(&original.timeline, &reopened.timeline));
        assert!(Rc::ptr_eq(&original.transactions, &reopened.transactions));
        assert!(!Rc::ptr_eq(
            &original.timeline,
            &cache.get("server-b", "agent-a").timeline
        ));
        assert!(!Rc::ptr_eq(
            &original.timeline,
            &cache.get("server-a", "agent-b").timeline
        ));
        cache.clear();
        assert!(!Rc::ptr_eq(
            &original.timeline,
            &cache.get("server-a", "agent-a").timeline
        ));
    }
}
