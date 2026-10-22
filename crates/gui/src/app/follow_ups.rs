//! Follow-ups queued while a run is busy stay above the composer until the
//! runtime reports they entered the agent's context.
//!
//! The runtime only reports how many host messages are still pending, and it
//! consumes them in submission order, so the oldest waiting entries are the
//! delivered ones whenever the count drops.
use super::WorkbenchState;
use crate::model::composer::QueuedFollowUp;
use crate::model::tasks::AgentRunSource;
use crate::model::transcript::TranscriptEntry;

impl<S: AgentRunSource> WorkbenchState<S> {
    /// Whether a chat submitted now would wait in a live run's inbox.
    pub(super) fn follow_up_would_queue(&self, thread: &str) -> bool {
        self.sink
            .follow_up_status(thread)
            .is_some_and(|status| !status.closed)
    }

    pub(super) fn queue_follow_up(&mut self, thread: &str, text: String, prompt: String) {
        self.follow_up_queues
            .entry(thread.into())
            .or_default()
            .push(QueuedFollowUp {
                text,
                prompt,
                undelivered: false,
            });
    }

    /// Move delivered follow-ups into their transcripts, mark ones a finished
    /// run dropped, and mirror the active thread's queue into the composer.
    pub(super) fn reconcile_follow_ups(&mut self) {
        let threads: Vec<String> = self.follow_up_queues.keys().cloned().collect();
        for thread in threads {
            let status = self.sink.follow_up_status(&thread);
            let Some(queue) = self.follow_up_queues.get_mut(&thread) else {
                continue;
            };
            let waiting = queue.iter().filter(|item| !item.undelivered).count();
            // A closed inbox still reports what it consumed before closing.
            let mut delivered = status.map_or(0, |status| waiting.saturating_sub(status.pending));
            let dropped = status.is_none_or(|status| status.closed);
            let mut kept = Vec::with_capacity(queue.len());
            for mut item in std::mem::take(queue) {
                if delivered > 0 && !item.undelivered {
                    delivered -= 1;
                    self.transcripts.push_to_thread(
                        &thread,
                        TranscriptEntry::UserMessage { text: item.prompt },
                    );
                } else {
                    item.undelivered |= dropped;
                    kept.push(item);
                }
            }
            if kept.is_empty() {
                self.follow_up_queues.remove(&thread);
            } else {
                self.follow_up_queues.insert(thread, kept);
            }
        }
        self.composer.queued = self
            .sidebar
            .active_thread
            .as_ref()
            .and_then(|id| self.follow_up_queues.get(&id.to_string()))
            .cloned()
            .unwrap_or_default();
    }

    /// Put an undelivered follow-up back into the composer to edit and resend.
    pub(super) fn restore_queued_follow_up(&mut self, index: usize) {
        let Some(item) = self.take_undelivered(index) else {
            return;
        };
        if self.composer.input.trim().is_empty() {
            self.composer.input = item.text;
        } else {
            self.composer.input = format!("{}\n\n{}", self.composer.input, item.text);
        }
        self.composer.pending_cursor = Some(self.composer.input.chars().count());
        self.composer.focus_requested = true;
        self.persist_composer_draft();
    }

    pub(super) fn dismiss_queued_follow_up(&mut self, index: usize) {
        self.take_undelivered(index);
    }

    /// Only dropped entries leave on request; waiting ones belong to the run.
    fn take_undelivered(&mut self, index: usize) -> Option<QueuedFollowUp> {
        let thread = self.sidebar.active_thread.as_ref()?.to_string();
        let queue = self.follow_up_queues.get_mut(&thread)?;
        if !queue.get(index)?.undelivered {
            return None;
        }
        let item = queue.remove(index);
        if queue.is_empty() {
            self.follow_up_queues.remove(&thread);
        }
        self.composer.queued = self
            .follow_up_queues
            .get(&thread)
            .cloned()
            .unwrap_or_default();
        Some(item)
    }
}
