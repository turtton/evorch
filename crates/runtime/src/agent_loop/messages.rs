use event_bus::{AgentMessage, AgentMessageKind};

use super::LoopState;

const AGENT_MESSAGE_PREFIX: &str = "agent-message";

impl LoopState {
    pub(super) fn flush_aside(&mut self) -> bool {
        if self.task.mailbox.is_empty() {
            return false;
        }
        let messages = self.task.mailbox.drain_where(|_| true);
        if messages.is_empty() {
            return false;
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
        for message in messages {
            self.context.push_user(&format_agent_message(&message));
        }
        self.publish_message_count();
    }
}

fn format_agent_message(message: &AgentMessage) -> String {
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
