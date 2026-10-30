//! Follow-ups queued while a run is busy stay above the composer until the
//! runtime reports they entered the agent's context.
//!
//! Durable receipts are authoritative. The count fallback is only for
//! lightweight fixture sinks that have no receipt store.
use super::WorkbenchState;
use super::history::Resolution;
use crate::model::composer::QueuedFollowUp;
use crate::model::tasks::AgentRunSource;

impl<S: AgentRunSource> WorkbenchState<S> {
    /// Whether a chat submitted now would wait in a live run's inbox.
    pub(super) fn follow_up_would_queue(&self, thread: &str) -> bool {
        self.sink
            .follow_up_status(thread)
            .is_some_and(|status| !status.closed)
    }

    pub(super) fn queue_follow_up(
        &mut self,
        thread: &str,
        submission_id: String,
        text: String,
        skills: Vec<String>,
    ) {
        self.follow_up_queues
            .entry(thread.into())
            .or_default()
            .push(QueuedFollowUp {
                submission_id,
                text,
                skills,
                undelivered: false,
                rejection: None,
            });
    }

    /// Move delivered follow-ups into their transcripts, mark ones a finished
    /// run dropped, and mirror the active thread's queue into the composer.
    pub(super) fn reconcile_follow_ups(&mut self) {
        self.refresh_draft_banner();
        // The event-drain boundary has already reconciled this frame. Rendering
        // must not perform a second lookup, even if the frame takes seconds.
        if self.sink.has_host_input_receipts() {
            self.mirror_active_queue();
            return;
        }
        let threads: Vec<String> = self.follow_up_queues.keys().cloned().collect();
        let mut delivered_ids = Vec::new();
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
                    self.transcripts.push_host_input(
                        &thread,
                        &item.submission_id,
                        item.text,
                        item.skills,
                    );
                    delivered_ids.push(item.submission_id);
                } else {
                    item.undelivered |= dropped;
                    if dropped
                        && let Some(message) = self.history.iter_mut().find(|message| {
                            message.id == item.submission_id
                                && message.status == super::history::DeliveryStatus::Queued
                        })
                    {
                        message.status = super::history::DeliveryStatus::Dropped;
                    }
                    kept.push(item);
                }
            }
            if kept.is_empty() {
                self.follow_up_queues.remove(&thread);
            } else {
                self.follow_up_queues.insert(thread, kept);
            }
        }
        if self.mark_delivered(&delivered_ids) {
            self.save_sidebar();
        }
        self.mirror_active_queue();
    }

    pub(super) fn mirror_active_queue(&mut self) {
        self.composer.queued = self
            .sidebar
            .active_thread
            .as_ref()
            .and_then(|id| self.follow_up_queues.get(&id.to_string()))
            .cloned()
            .unwrap_or_default();
    }

    /// Runtime withdrawal is authoritative. Never restore/delete a message that
    /// has already entered the context, even if this frame still showed Queued.
    pub(super) fn withdraw_queued_follow_up(&mut self, index: usize, edit: bool) {
        let Some(id) = self
            .composer
            .queued
            .get(index)
            .filter(|item| !item.undelivered)
            .map(|item| item.submission_id.clone())
        else {
            return;
        };
        let Some(thread) = self
            .history
            .iter()
            .find(|message| message.id == id)
            .map(|message| message.thread_id.clone())
        else {
            return;
        };
        match self.sink.withdraw_follow_up(&thread, &id) {
            Ok(()) => {
                if edit {
                    self.move_withdrawn_to_composer(&id);
                } else {
                    self.history.retain(|message| message.id != id);
                    self.queued_images.remove(&id);
                    if let Err(error) = self.persist_history() {
                        self.push_notice(format!("Could not save deletion: {error}"));
                    }
                }
                self.rebuild_delivery_rows();
            }
            Err(runtime::RuntimeError::FollowUpWithdrawal(
                runtime::FollowUpWithdrawalError::AlreadyConsumed,
            )) => {
                if let Some(message) = self.history.iter().find(|message| message.id == id) {
                    self.transcripts.push_host_input(
                        &thread,
                        &id,
                        message.text.clone(),
                        message.skills.clone(),
                    );
                }
                if self.mark_delivered(&[id]) {
                    self.save_sidebar();
                }
                self.rebuild_delivery_rows();
                self.push_notice("Message already delivered; it cannot be edited or deleted.");
            }
            Err(error) => self.push_notice(format!("Could not withdraw message: {error}")),
        }
    }

    /// Put an undelivered follow-up back into the composer to edit and resend.
    pub(super) fn restore_queued_follow_up(&mut self, index: usize) {
        let Some(id) = self
            .composer
            .queued
            .get(index)
            .filter(|item| item.undelivered)
            .map(|item| item.submission_id.clone())
        else {
            return;
        };
        self.move_to_composer(&id);
    }

    pub(super) fn dismiss_queued_follow_up(&mut self, index: usize) {
        let Some(id) = self
            .composer
            .queued
            .get(index)
            .filter(|item| item.undelivered)
            .map(|item| item.submission_id.clone())
        else {
            return;
        };
        let Some(message) = self
            .history
            .iter_mut()
            .find(|message| message.id == id && message.awaits_user())
        else {
            return;
        };
        message.resolution = Some(Resolution::Dismissed);
        if let Err(error) = self.persist_history() {
            self.history
                .iter_mut()
                .find(|message| message.id == id)
                .unwrap()
                .resolution = None;
            self.push_notice(format!("Could not save state: {error}"));
            return;
        }
        self.rebuild_delivery_rows();
    }
}
