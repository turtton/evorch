use event_bus::{AgentMessage, AgentMessageKind};

use super::LoopState;

const AGENT_MESSAGE_PREFIX: &str = "agent-message";

impl LoopState {
    pub(crate) fn begin_internal_input(&mut self) {
        self.turn = crate::request_turn::internal(self.task.ancestry);
    }

    pub(crate) fn accept_input_identity(&mut self, trusted_user: bool, prompt_id: Option<String>) {
        if trusted_user && let Some(prompt_id) = prompt_id {
            self.turn =
                crate::request_turn::human(prompt_id, self.task.ancestry, false, Some(&self.turn));
        } else {
            self.begin_internal_input();
        }
    }

    pub(crate) fn has_pending_user_messages(&self) -> bool {
        !self.pending_user_messages.is_empty()
    }

    pub(crate) fn queue_user_message(&mut self, message: crate::runtime::user_inbox::UserInput) {
        self.pending_user_messages.push(message);
    }

    pub(super) fn flush_user_messages(&mut self) -> bool {
        let (cut, _) = self.channels.user_inbox.seal_for_next_request();
        self.flush_user_messages_through(cut)
    }

    pub(super) fn flush_scheduled_user_messages(&mut self) {
        let (cut, requested) = self.channels.user_inbox.seal_for_next_request();
        if self.has_pending_user_messages() || requested {
            self.flush_user_messages_through(cut);
        }
    }

    fn flush_user_messages_through(&mut self, cut: u64) -> bool {
        let mut messages = std::mem::take(&mut self.pending_user_messages);
        while let Ok(message) = self.channels.inbox_rx.try_recv() {
            messages.push(message);
        }
        let mut later = Vec::new();
        messages.retain(|message| {
            if message.2 && message.4 > cut {
                later.push(message.clone());
                false
            } else {
                true
            }
        });
        self.pending_user_messages = later;
        // A trusted goal wake is a scheduling signal, not a new human request.
        // When already running, the normal Stop boundary will perform the check.
        messages.retain(|(text, _, trusted_user, _, _)| {
            if !trusted_user && text == crate::thread_goals::CHECKS_WAKE {
                self.goal_wake_pending = true;
                false
            } else {
                true
            }
        });
        let received = !messages.is_empty();
        let through = messages
            .iter()
            .filter(|message| message.2)
            .map(|message| message.4)
            .max()
            .unwrap_or(0);
        for message in messages {
            self.take_user_input(message);
        }
        self.channels.user_inbox.consumed_through(through);
        if received {
            self.publish_message_count();
            self.resumed = true;
        }
        received
    }

    pub(super) fn flush_aside(&mut self) -> bool {
        let received = self.flush_user_messages();
        if self.task.mailbox.is_empty() {
            return received;
        }
        let messages = self.task.mailbox.drain_where(|_| true);
        if messages.is_empty() {
            return received;
        }
        self.inject_messages(messages);
        // Aside は turn 境界で新しい入力となり、次の clean Stop で完了する。
        self.resumed = true;
        true
    }

    pub(super) fn inject_parent_messages(&mut self) {
        let Some(parent) = self.task.parent else {
            return;
        };
        let parent = parent.to_string();
        let messages = self
            .task
            .mailbox
            .drain_where(|message| message.sender_run_id == parent);
        self.inject_messages(messages);
    }

    pub(super) fn inject_messages(&mut self, messages: Vec<AgentMessage>) {
        if messages.is_empty() {
            return;
        }
        self.begin_internal_input();
        for message in messages {
            self.context.push_user(&format_agent_message(&message));
        }
        self.publish_message_count();
    }
}

pub(super) fn format_agent_message(message: &AgentMessage) -> String {
    let kind = match message.kind {
        AgentMessageKind::Send => "send",
        AgentMessageKind::Reply => "reply",
        AgentMessageKind::Steering => "steering",
    };
    format!(
        "[{AGENT_MESSAGE_PREFIX} id={} from={} kind={kind}]\n{}",
        message.message_id, message.sender_run_id, message.content
    )
}

#[cfg(test)]
mod shell_handoff_tests {
    use std::sync::Arc;
    use tools::executor::ShellInputControl;

    #[tokio::test]
    async fn next_request_cut_appends_inputs_once_and_keeps_newer_reservation() {
        let (_cancel, cancelled) = tokio::sync::watch::channel(crate::RunInterrupt::None);
        let (_runtime, mut state) = super::super::delegate_cleanup_tests::fixture(cancelled);
        let (sender, receiver) = tokio::sync::mpsc::channel(8);
        state.channels.inbox_rx = receiver;
        let inbox = Arc::new(crate::runtime::user_inbox::UserInbox::default());
        state.channels.user_inbox = inbox.clone();
        let first = inbox.begin_wait();
        let parallel = inbox.begin_wait();
        inbox
            .enqueue(|seq| {
                sender.try_send(("before cut".into(), vec![], true, Some("one".into()), seq))
            })
            .unwrap();
        assert!(*first.borrow() && *parallel.borrow());
        drop(first);
        drop(parallel); // Reservation survives receiver teardown.
        let (cut, requested) = inbox.seal_for_next_request();
        assert!(requested);
        let _new_wait = inbox.begin_wait();
        inbox
            .enqueue(|seq| {
                sender.try_send(("after cut".into(), vec![], true, Some("two".into()), seq))
            })
            .unwrap();
        assert!(state.flush_user_messages_through(cut));
        assert_eq!(inbox.status().pending, 1);
        assert!(inbox.status().next_turn_requested);
        assert_eq!(state.pending_user_messages.len(), 1);
        assert!(state.host_inputs().contains("one"));
        assert!(!state.host_inputs().contains("two"));
        state.flush_scheduled_user_messages();
        assert_eq!(inbox.status().pending, 0);
        assert!(!inbox.status().next_turn_requested);
        assert!(state.host_inputs().contains("two"));
        assert!(!state.flush_user_messages());
        let history = serde_json::to_string(&state.context.messages).unwrap();
        assert_eq!(history.matches("before cut").count(), 1);
        assert_eq!(history.matches("after cut").count(), 1);
    }
}
