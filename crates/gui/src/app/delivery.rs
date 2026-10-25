//! Each composer message records how far it got towards the agent, so a
//! refused one is neither drawn nor replayed as sent.
use super::WorkbenchState;
use super::history::{DeliveryStatus, Resolution};
use crate::model::tasks::AgentRunSource;

impl<S: AgentRunSource> WorkbenchState<S> {
    /// Mark waiting messages as part of the conversation; returns whether any changed.
    pub(super) fn mark_delivered(&mut self, submission_ids: &[String]) -> bool {
        if submission_ids.is_empty() {
            return false;
        }
        let mut changed = false;
        for message in &mut self.history {
            if message.status == DeliveryStatus::Queued
                && !message.id.is_empty()
                && submission_ids.contains(&message.id)
            {
                message.status = DeliveryStatus::Delivered;
                changed = true;
            }
        }
        changed
    }

    /// Record that the runtime refused a submission still waiting for its
    /// verdict, and return whether one was waiting.
    ///
    /// The agent never saw it, so it gets no bubble. Its text goes back into
    /// its thread's composer, unless the user has drafted something else
    /// there: that draft is kept, and the message waits above the composer as
    /// a not-delivered row to edit back in or dismiss.
    pub fn reject_submission(&mut self, submission_id: &str, reason: impl Into<String>) -> bool {
        let Some(index) = self.history.iter().position(|message| {
            !submission_id.is_empty()
                && message.id == submission_id
                && message.status == DeliveryStatus::Queued
        }) else {
            return false;
        };
        let message = &mut self.history[index];
        message.status = DeliveryStatus::Rejected {
            reason: reason.into(),
        };
        let thread = message.thread_id.clone();
        let text = message.text.clone();
        // A refused follow-up no longer waits in the run's inbox.
        if let Some(rows) = self.follow_up_queues.get_mut(&thread) {
            rows.retain(|row| row.submission_id != submission_id);
            if rows.is_empty() {
                self.follow_up_queues.remove(&thread);
            }
        }
        if self.restore_rejected_text(&thread, &text) {
            self.history[index].resolution = Some(Resolution::Restored);
        } else {
            let row = self.history[index].undelivered_row();
            self.follow_up_queues.entry(thread).or_default().push(row);
        }
        self.mirror_active_queue();
        self.save_sidebar();
        true
    }

    /// Put rejected text back into its thread's draft unless a different
    /// draft is there. A refusal reported while sending finds the composer
    /// still holding that text and its attachments.
    fn restore_rejected_text(&mut self, thread: &str, text: &str) -> bool {
        let active = self
            .sidebar
            .active_thread
            .as_ref()
            .is_some_and(|id| id.to_string() == thread);
        let draft = if active {
            &mut self.composer.input
        } else {
            match self
                .sidebar
                .threads
                .iter_mut()
                .find(|record| record.id.to_string() == thread)
            {
                Some(record) => &mut record.draft_input,
                None => return false,
            }
        };
        let current = draft.trim();
        if current == text {
            return true;
        }
        if !current.is_empty() {
            return false;
        }
        text.clone_into(draft);
        if active {
            self.composer.pending_cursor = Some(text.chars().count());
            self.composer.focus_requested = true;
            self.persist_composer_draft();
        }
        true
    }

    /// The user restored or dismissed a message that never reached the agent.
    pub(super) fn resolve_undelivered(&mut self, submission_id: &str, resolution: Resolution) {
        let Some(message) = self.history.iter_mut().find(|message| {
            !submission_id.is_empty() && message.id == submission_id && message.awaits_user()
        }) else {
            return;
        };
        message.resolution = Some(resolution);
        self.save_sidebar();
    }
}
