//! Durable receipts, rather than inbox counts or event arrival, decide delivery.
use super::WorkbenchState;
use super::history::{ComposerDraft, DeliveryStatus, Resolution};
use crate::model::tasks::AgentRunSource;
use crate::model::transcript::TranscriptEntry;

impl<S: AgentRunSource> WorkbenchState<S> {
    pub(super) fn persist_history(&self) -> Result<(), Box<dyn std::error::Error>> {
        match &self.sidebar_path {
            Some(path) => self.write_history_sidebar(path),
            None => Ok(()), // in-memory fixtures
        }
    }

    pub(super) fn mark_delivered(&mut self, ids: &[String]) -> bool {
        let mut changed = false;
        for message in &mut self.history {
            if message.status == DeliveryStatus::Queued
                && message.resolution.is_none()
                && !message.id.is_empty()
                && ids.contains(&message.id)
            {
                message.status = DeliveryStatus::Delivered;
                changed = true;
            }
        }
        changed
    }

    pub(super) fn apply_receipts(&mut self, receipts: &[storage::HostInputReceipt], startup: bool) {
        use storage::HostInputStatus;
        for message in &mut self.history {
            if message.resolution.is_some() || message.id.is_empty() {
                continue;
            }
            if let Some(receipt) = receipts
                .iter()
                .find(|receipt| receipt.prompt_id == message.id)
            {
                message.image_count = receipt.image_count as usize;
                message.status = match receipt.status {
                    HostInputStatus::Accepted => {
                        let terminal = self.phases.get(&receipt.run_id).is_some_and(|phase| {
                            matches!(
                                phase,
                                workspace_ui::ThreadRunPhase::Done
                                    | workspace_ui::ThreadRunPhase::Error
                                    | workspace_ui::ThreadRunPhase::Stopped
                            )
                        });
                        if terminal {
                            DeliveryStatus::Unknown
                        } else {
                            DeliveryStatus::Queued
                        }
                    }
                    HostInputStatus::Delivered => DeliveryStatus::Delivered,
                    HostInputStatus::Dropped => DeliveryStatus::Dropped,
                    HostInputStatus::Unknown => DeliveryStatus::Unknown,
                };
            } else if startup && message.status == DeliveryStatus::Queued {
                message.status = DeliveryStatus::Rejected {
                    reason: "送信前に中断".into(),
                };
            }
        }
    }

    /// A notification is just a reason to re-read the receipts. Missing/lagged
    /// notifications are repaired by the same reconciliation on each frame.
    pub(super) fn reconcile_receipts(&mut self) -> bool {
        let ids: Vec<_> = self
            .history
            .iter()
            .filter(|message| {
                message.resolution.is_none()
                    && matches!(
                        message.status,
                        DeliveryStatus::Queued | DeliveryStatus::Unknown
                    )
            })
            .map(|message| message.id.clone())
            .collect();
        if ids.is_empty() {
            return self.sink.host_input_receipts(&[]).is_some();
        }
        let Some(result) = self.sink.host_input_receipts(&ids) else {
            return false;
        };
        let receipts = match result {
            Ok(receipts) => receipts,
            Err(error) => {
                tracing::warn!(%error, "receipt lookup failed");
                return true;
            }
        };
        let before = self.history.clone();
        self.apply_receipts(&receipts, false);
        for (old, new) in before.iter().zip(&self.history) {
            if old.status != DeliveryStatus::Delivered && new.status == DeliveryStatus::Delivered {
                self.transcripts.push_to_thread(
                    &new.thread_id,
                    TranscriptEntry::UserMessage {
                        text: new.prompt.clone(),
                    },
                );
            }
        }
        if before
            .iter()
            .zip(&self.history)
            .any(|(old, new)| old.status != new.status)
        {
            self.rebuild_delivery_rows();
            self.save_sidebar();
        }
        true
    }

    pub(super) fn rebuild_delivery_rows(&mut self) {
        self.follow_up_queues.clear();
        for message in &self.history {
            if message.resolution.is_none()
                && (message.status == DeliveryStatus::Queued || message.awaits_user())
            {
                let mut row = message.undelivered_row();
                row.undelivered = message.status != DeliveryStatus::Queued;
                self.follow_up_queues
                    .entry(message.thread_id.clone())
                    .or_default()
                    .push(row);
            }
        }
        self.mirror_active_queue();
    }

    pub(super) fn refresh_draft_banner(&mut self) {
        let thread = self.sidebar.active_thread.as_ref().map(ToString::to_string);
        self.composer.images_needed = self
            .composer_drafts
            .iter()
            .filter(|draft| Some(&draft.thread_id) == thread.as_ref())
            .map(|draft| draft.images_needed)
            .sum();
    }

    /// Move to a durable composer draft before touching the visible editor.
    /// A failed save rolls back both the original and its draft.
    pub(super) fn move_to_composer(&mut self, id: &str) -> bool {
        let Some(index) = self
            .history
            .iter()
            .position(|message| message.id == id && message.awaits_user())
        else {
            return false;
        };
        let history = self.history.clone();
        let drafts = self.composer_drafts.clone();
        let sidebar = self.sidebar.clone();
        let message = &mut self.history[index];
        let draft_id = uuid::Uuid::new_v4().to_string();
        message.resolution = Some(Resolution::MovedToComposer {
            draft_id: draft_id.clone(),
        });
        let thread = message.thread_id.clone();
        let text = message.text.clone();
        self.composer_drafts.push(ComposerDraft {
            draft_id,
            thread_id: thread.clone(),
            text: text.clone(),
            origin: id.into(),
            images_needed: message.image_count,
        });
        let active = self
            .sidebar
            .active_thread
            .as_ref()
            .is_some_and(|id| id.to_string() == thread);
        let current = if active {
            self.composer.input.clone()
        } else {
            self.sidebar
                .threads
                .iter()
                .find(|record| record.id.to_string() == thread)
                .map(|record| record.draft_input.clone())
                .unwrap_or_default()
        };
        let restored = if current == text {
            current
        } else if current.trim().is_empty() {
            text
        } else {
            format!("{current}\n\n{text}")
        };
        if let Some(record) = self
            .sidebar
            .threads
            .iter_mut()
            .find(|record| record.id.to_string() == thread)
        {
            record.draft_input.clone_from(&restored);
        }
        if let Err(error) = self.persist_history() {
            self.history = history;
            self.composer_drafts = drafts;
            self.sidebar = sidebar;
            self.push_notice(format!("下書きを保存できませんでした: {error}"));
            return false;
        }
        if active {
            self.composer.input = restored;
            self.composer.pending_cursor = Some(self.composer.input.chars().count());
            self.composer.focus_requested = true;
        }
        self.rebuild_delivery_rows();
        self.refresh_draft_banner();
        true
    }

    pub fn reject_submission(&mut self, id: &str, reason: impl Into<String>) -> bool {
        let Some(message) = self.history.iter_mut().find(|message| {
            !id.is_empty()
                && message.id == id
                && message.status == DeliveryStatus::Queued
                && message.resolution.is_none()
        }) else {
            return false;
        };
        message.status = DeliveryStatus::Rejected {
            reason: reason.into(),
        };
        let thread = message.thread_id.clone();
        let text = message.text.clone();
        // Persist the refusal before resolving it. A save failure leaves the
        // queued identity intact for receipt reconciliation after recovery.
        if let Err(error) = self.persist_history() {
            self.history
                .iter_mut()
                .find(|message| message.id == id)
                .unwrap()
                .status = DeliveryStatus::Queued;
            self.push_notice(format!("拒否状態を保存できませんでした: {error}"));
            return false;
        }
        let active = self
            .sidebar
            .active_thread
            .as_ref()
            .is_some_and(|id| id.to_string() == thread);
        let current = if active {
            self.composer.input.as_str()
        } else {
            self.sidebar
                .threads
                .iter()
                .find(|record| record.id.to_string() == thread)
                .map_or("", |record| record.draft_input.as_str())
        };
        if current.trim().is_empty() || current == text {
            self.move_to_composer(id);
        }
        self.rebuild_delivery_rows();
        true
    }
}
