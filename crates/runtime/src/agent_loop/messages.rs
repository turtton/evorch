use event_bus::{AgentMessage, AgentMessageKind};

use super::LoopState;

const AGENT_MESSAGE_PREFIX: &str = "agent-message";

impl LoopState {
    pub(crate) fn has_pending_user_messages(&self) -> bool {
        !self.pending_user_messages.is_empty()
    }

    pub(crate) fn queue_user_message(&mut self, message: (String, Vec<crate::DelegateImage>)) {
        self.pending_user_messages.push(message);
    }

    pub(super) fn flush_user_messages(&mut self) -> bool {
        let mut messages = std::mem::take(&mut self.pending_user_messages);
        while let Ok(message) = self.channels.inbox_rx.try_recv() {
            messages.push(message);
        }
        let received = !messages.is_empty();
        for (text, images) in messages {
            self.context.push_user(&text);
            if let Some(message) = self.context.messages.last_mut() {
                message.content.extend(images.into_iter().map(|image| {
                    providers::ContentBlock::Image {
                        media_type: image.media_type,
                        data: image.data,
                    }
                }));
            }
        }
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
