//! Delivers notices that another thread changed a file this run knows.

use event_bus::{
    AgentMessage, AgentMessageEvent, AgentMessageKind, DeliveryDisposition, Event, EventKind,
};

use super::LoopState;
use super::messages::format_agent_message;

impl LoopState {
    /// Starts tracking this run's files once its thread and active root are known.
    pub(super) fn track_file_touches(&mut self, active_root: Option<&std::path::Path>) {
        let (Some(thread_id), Some(root), Some(runtime)) =
            (self.thread_id.clone(), active_root, self.runtime())
        else {
            return;
        };
        self.file_touch =
            runtime
                .shared
                .file_touch
                .subscribe(self.task.run_id.to_string(), thread_id, root);
    }

    /// Appends one notice for the changes pending for this run. Called only from the
    /// loop head, so a notice never causes a request by itself and never replaces a
    /// published result. It is appended like a shell job notice: the turn is untouched.
    pub(super) fn flush_file_touches(&mut self) {
        let Some(text) = self
            .file_touch
            .as_ref()
            .and_then(|tracking| tracking.take_notice())
        else {
            return;
        };
        let Some(Ok(message_id)) = self
            .runtime()
            .map(|runtime| runtime.allocate_file_touch_message_id())
        else {
            return;
        };
        let message = AgentMessage {
            message_id,
            sender_run_id: "runtime".to_owned(),
            recipient_run_id: self.task.run_id.to_string(),
            kind: AgentMessageKind::Send,
            content: text,
            reply_to: None,
        };
        self.context.push_user(&format_agent_message(&message));
        self.publish_message_count();
        self.shared.bus.emit(Event::new(EventKind::AgentMessage(
            AgentMessageEvent::Delivered {
                message,
                disposition: DeliveryDisposition::Aside,
            },
        )));
    }
}
