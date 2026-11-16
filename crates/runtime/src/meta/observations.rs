//! Receipts travel with the exact tool result and are committed only after it
//! enters the caller's context. Peeking, serialization and cancellation are not
//! observations; in particular a phase-only result must not carry output receipts.
use crate::{
    agent_loop::LoopState,
    mailbox::{CompletionNotice, QueuedMessage},
};
use std::sync::Arc;

#[derive(Default)]
pub(crate) struct Observations {
    pub(crate) completions: Vec<Arc<CompletionNotice>>,
    pub(crate) questions: Vec<String>,
    pub(crate) messages: Vec<QueuedMessage>,
    pub(crate) shell: Vec<tools::ShellJobIdentity>,
    pub(crate) events: Vec<String>,
}
impl Observations {
    pub(crate) fn presented(self, state: &mut LoopState) {
        let Some(runtime) = state.runtime() else {
            return;
        };
        state
            .reported_questions
            .extend(self.questions.iter().cloned());
        runtime.observe_questions(self.questions);
        for notice in self.completions {
            runtime.observe_completion(&notice);
        }
        let ids = self
            .messages
            .iter()
            .map(|m| m.message.message_id.as_str())
            .collect::<std::collections::HashSet<_>>();
        if let Ok(mailbox) = runtime.run_mailbox(state.caller_run_id()) {
            mailbox.consume_ids(&ids);
        }
        for queued in self.messages {
            runtime.observe_queued_message(&queued);
        }
        if !self.shell.is_empty() {
            state.shell_result_batch_open = state.has_open_tool_batch();
        }
        for identity in self.shell {
            state
                .shell_checkpoint_results
                .insert(identity.id().into(), identity);
        }
        // Keep identity evidence on failure: retry ACK, never re-present the text.
        let _ = state.acknowledge_restart_shell_results();
        for event in self.events {
            state.budget.observe_wait_event(&event);
        }
    }
}
