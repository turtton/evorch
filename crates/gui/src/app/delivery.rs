//! Durable receipts, rather than inbox counts or event arrival, decide delivery.
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use super::WorkbenchState;
use super::history::{ComposerDraft, DeliveryStatus, Resolution};
use crate::model::tasks::AgentRunSource;

pub(super) const RECEIPT_REPAIR_INTERVAL: Duration = Duration::from_secs(3);

#[derive(Default)]
pub(super) struct ReceiptReconciliation {
    pub dirty: bool,
    pub notified: HashSet<String>,
    pub last_repair: Option<Instant>,
    /// The periodic pass is due: also look up messages whose recorded origin
    /// could not be read.
    pub origin_check: bool,
    /// The user was already told that a record could not be read.
    pub origin_notice: bool,
}

/// What asking the sink for the runtime's records came to.
pub(super) enum OriginLookup {
    /// The sink records nothing (a fixture); the composer's copy stands.
    Unsupported,
    Recorded(Vec<storage::HostInputOrigin>),
    Failed(String),
}

impl<S: AgentRunSource> WorkbenchState<S> {
    pub(super) fn persist_history(&self) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(path) = &self.sidebar_path {
            self.write_history_sidebar(path)?;
        }
        self.draft_dirty_at.set(None);
        Ok(())
    }

    fn lookup_origins(&self, ids: &[String]) -> OriginLookup {
        match self.sink.host_input_origins(ids) {
            None => OriginLookup::Unsupported,
            Some(Ok(origins)) => OriginLookup::Recorded(origins),
            Some(Err(error)) => OriginLookup::Failed(error.to_string()),
        }
    }

    /// Tells the user once, and logs every time, that the runtime's record of
    /// an accepted message could not be read. The message was accepted and is
    /// not sent again; it shows the composer's copy until the record is read.
    fn report_unverified_origin(&mut self, detail: &str) {
        tracing::warn!(
            detail,
            "could not read what the runtime recorded for a message"
        );
        if !std::mem::replace(&mut self.receipt_reconciliation.origin_notice, true) {
            self.push_notice(format!(
                "Could not confirm which skills the runtime attached to a sent message \
                 ({detail}). It was accepted and is not sent again; the skill chips will \
                 be corrected once the record can be read."
            ));
        }
    }

    /// Takes the words and skill names the runtime recorded for a submission it
    /// accepted as the message's own, saving them, and returns them. A sink that
    /// records nothing leaves the composer's copy. A lookup that fails, or a
    /// record that cannot be read, also leaves it, but marks the message
    /// unverified, tells the user and is looked up again later.
    pub(super) fn adopt_runtime_origin(&mut self, id: &str) -> Option<(String, Vec<String>)> {
        let position = self.history.iter().position(|message| message.id == id)?;
        match self.lookup_origins(&[id.to_owned()]) {
            OriginLookup::Unsupported => None,
            OriginLookup::Failed(error) => {
                self.history[position].origin_unverified = true;
                self.persist_after_origin_change();
                self.report_unverified_origin(&error);
                None
            }
            OriginLookup::Recorded(origins) => {
                let origin = origins.into_iter().find(|origin| origin.prompt_id == id)?;
                let unreadable = origin.unreadable.clone();
                let changed = self.apply_origin_at(position, origin);
                if changed {
                    self.persist_after_origin_change();
                }
                if let Some(reason) = unreadable {
                    self.report_unverified_origin(&reason);
                    return None;
                }
                let message = &self.history[position];
                Some((message.text.clone(), message.skills.clone()))
            }
        }
    }

    fn persist_after_origin_change(&mut self) {
        if let Err(error) = self.persist_history() {
            self.push_notice(format!("Could not save the recorded message: {error}"));
        }
    }

    /// Looks up the messages whose record could not be read before and puts
    /// what the runtime recorded into the history, the bubble and the queue.
    /// Nothing is sent again. Returns whether anything changed.
    pub(super) fn repair_unverified_origins(&mut self) -> bool {
        let ids: Vec<String> = self
            .history
            .iter()
            .filter(|message| message.origin_unverified && !message.id.is_empty())
            .map(|message| message.id.clone())
            .collect();
        if ids.is_empty() {
            self.receipt_reconciliation.origin_notice = false;
            return false;
        }
        let origins = match self.lookup_origins(&ids) {
            OriginLookup::Unsupported => {
                // A sink that records nothing will never answer.
                for message in &mut self.history {
                    message.origin_unverified = false;
                }
                return true;
            }
            OriginLookup::Failed(error) => {
                self.report_unverified_origin(&error);
                return false;
            }
            OriginLookup::Recorded(origins) => origins,
        };
        let index: HashMap<String, usize> = self
            .history
            .iter()
            .enumerate()
            .map(|(position, message)| (message.id.clone(), position))
            .collect();
        let mut changed = false;
        let mut still_unreadable = None;
        for origin in origins {
            let Some(&position) = index.get(&origin.prompt_id) else {
                continue;
            };
            if let Some(reason) = &origin.unreadable {
                still_unreadable = Some(reason.clone());
            }
            if self.apply_origin_at(position, origin) {
                changed = true;
                let message = &self.history[position];
                self.transcripts.update_host_input(
                    &message.thread_id,
                    &message.id,
                    &message.text,
                    &message.skills,
                );
            }
        }
        if let Some(reason) = still_unreadable {
            self.report_unverified_origin(&reason);
        } else if !self.history.iter().any(|message| message.origin_unverified) {
            self.receipt_reconciliation.origin_notice = false;
        }
        changed
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
                self.queued_images.remove(&message.id);
                changed = true;
            }
        }
        changed
    }

    /// Only changed rows are projected; prompts and the history are never cloned
    /// for comparison. Startup builds the transcript separately after recovery.
    pub(super) fn apply_receipts(
        &mut self,
        receipts: &[storage::HostInputReceipt],
        startup: bool,
    ) -> bool {
        use storage::HostInputStatus;
        let receipts: HashMap<_, _> = receipts
            .iter()
            .map(|receipt| (receipt.prompt_id.as_str(), receipt))
            .collect();
        let before = self.history.len();
        self.history.retain(|message| {
            !receipts
                .get(message.id.as_str())
                .is_some_and(|receipt| receipt.status == HostInputStatus::Withdrawn)
        });
        let mut changed = self.history.len() != before;
        for message in &mut self.history {
            if message.resolution.is_some() || message.id.is_empty() {
                continue;
            }
            let was_delivered = message.status == DeliveryStatus::Delivered;
            let status = if let Some(receipt) = receipts.get(message.id.as_str()) {
                if message.image_count != receipt.image_count as usize {
                    message.image_count = receipt.image_count as usize;
                    changed = true;
                }
                match receipt.status {
                    // A failed withdrawal proves adoption before the snapshot
                    // receipt commits. Accepted cannot revoke that observation.
                    HostInputStatus::Accepted if was_delivered => DeliveryStatus::Delivered,
                    HostInputStatus::Accepted => {
                        let terminal = self.phases.get(&receipt.run_id).is_some_and(|phase| {
                            matches!(
                                phase,
                                workspace_ui::ThreadRunPhase::Done
                                    | workspace_ui::ThreadRunPhase::Error
                                    | workspace_ui::ThreadRunPhase::Stopped
                            )
                        });
                        let live = receipt
                            .run_id
                            .parse()
                            .ok()
                            .is_some_and(|run| self.tasks.host_input_run_is_live(run));
                        if terminal && !live {
                            DeliveryStatus::Unknown
                        } else {
                            DeliveryStatus::Queued
                        }
                    }
                    HostInputStatus::Delivered => DeliveryStatus::Delivered,
                    HostInputStatus::Dropped => DeliveryStatus::Dropped,
                    HostInputStatus::Unknown => DeliveryStatus::Unknown,
                    HostInputStatus::Withdrawn => continue,
                }
            } else if startup && message.status == DeliveryStatus::Queued {
                DeliveryStatus::Rejected {
                    reason: "Interrupted before sending".into(),
                }
            } else {
                continue;
            };
            if message.status != status {
                message.status = status;
                changed = true;
                // Also withdraws the bubble shown optimistically at send time.
                if message.status != DeliveryStatus::Delivered {
                    self.transcripts
                        .remove_host_input(&message.thread_id, &message.id);
                } else if !startup {
                    self.transcripts.push_host_input(
                        &message.thread_id,
                        &message.id,
                        message.text.clone(),
                        message.skills.clone(),
                    );
                }
            }
        }
        changed
    }

    pub(super) fn note_receipt_event(&mut self, event: &event_bus::Event) {
        use event_bus::{EventKind, LifecycleEvent};
        if let EventKind::Ownership(event_bus::OwnershipEvent {
            owner_id,
            action: event_bus::OwnershipAction::Claimed | event_bus::OwnershipAction::Handoff,
            ..
        }) = &event.kind
            && self
                .ownership
                .as_ref()
                .is_some_and(|host| host.owner_id() == owner_id)
        {
            self.receipt_reconciliation.last_repair = None;
        }
        if matches!(event.kind, EventKind::Lifecycle(_) | EventKind::Provider(_)) {
            self.receipt_reconciliation.dirty = true;
        }
        if let EventKind::Lifecycle(
            LifecycleEvent::HostInputDelivered { prompt_id, .. }
            | LifecycleEvent::HostInputDropped { prompt_id, .. }
            | LifecycleEvent::HostInputUnknown { prompt_id, .. },
        ) = &event.kind
        {
            self.receipt_reconciliation
                .notified
                .insert(prompt_id.clone());
        }
    }

    /// At most one lookup per event batch/frame. The periodic broad pass repairs
    /// lost notifications, including Delivered -> Unknown snapshot invalidation.
    pub(super) fn read_receipts_if_due(
        &mut self,
        now: Instant,
    ) -> Option<Vec<storage::HostInputReceipt>> {
        let state = &mut self.receipt_reconciliation;
        let repair = state
            .last_repair
            .is_none_or(|last| now.saturating_duration_since(last) >= RECEIPT_REPAIR_INTERVAL);
        if !state.dirty && !repair {
            return None;
        }
        state.dirty = false;
        if repair {
            state.last_repair = Some(now);
            state.origin_check = true;
        }
        let ids: Vec<_> = self
            .history
            .iter()
            .filter(|message| {
                message.resolution.is_none()
                    && !message.id.is_empty()
                    && (matches!(
                        message.status,
                        DeliveryStatus::Queued | DeliveryStatus::Unknown
                    ) || (message.status == DeliveryStatus::Delivered
                        && (repair || state.notified.contains(&message.id))))
            })
            .map(|message| message.id.clone())
            .collect();
        state.notified.clear();
        if ids.is_empty() {
            return None;
        }
        match self.sink.host_input_receipts(&ids)? {
            Ok(mut receipts) => {
                let requested: HashSet<_> = ids.iter().map(String::as_str).collect();
                receipts.retain(|receipt| requested.contains(receipt.prompt_id.as_str()));
                Some(receipts)
            }
            Err(error) => {
                tracing::warn!(%error, "receipt lookup failed");
                None // The periodic pass retries even without another notification.
            }
        }
    }

    pub(super) fn finish_receipt_reconciliation(&mut self, changed: bool) {
        let repaired = std::mem::take(&mut self.receipt_reconciliation.origin_check)
            && self.repair_unverified_origins();
        if changed || repaired {
            self.rebuild_delivery_rows();
            self.save_sidebar();
        }
    }

    #[cfg(test)]
    pub(super) fn reconcile_receipts_at(&mut self, now: Instant) -> bool {
        if let Some(receipts) = self.read_receipts_if_due(now) {
            let changed = self.apply_receipts(&receipts, false);
            self.finish_receipt_reconciliation(changed);
        }
        self.sink.has_host_input_receipts()
    }

    /// Explicit recovery/claim boundary, also used by focused receipt fixtures.
    #[cfg(test)]
    pub(super) fn reconcile_receipts(&mut self) -> bool {
        self.receipt_reconciliation.last_repair = None;
        self.reconcile_receipts_at(Instant::now())
    }

    pub(super) fn rebuild_delivery_rows(&mut self) {
        let pending: HashSet<_> = self
            .history
            .iter()
            .filter(|message| message.status == DeliveryStatus::Queued || message.awaits_user())
            .map(|message| message.id.as_str())
            .collect();
        self.queued_images
            .retain(|id, _| pending.contains(id.as_str()));
        self.follow_up_queues.clear();
        for message in &self.history {
            if message.resolution.is_none()
                && ((message.status == DeliveryStatus::Queued && !message.starts_run)
                    || message.awaits_user())
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
            .sum::<usize>()
            .saturating_sub(self.composer.attachments.len());
    }

    /// Move to a durable composer draft before touching the visible editor.
    /// A failed save rolls back both the original and its draft.
    pub(super) fn move_to_composer(&mut self, id: &str) -> bool {
        self.move_to_composer_inner(id, false)
    }

    pub(super) fn move_withdrawn_to_composer(&mut self, id: &str) -> bool {
        self.move_to_composer_inner(id, true)
    }

    fn move_to_composer_inner(&mut self, id: &str, withdrawn: bool) -> bool {
        let Some(index) = self
            .history
            .iter()
            .position(|message| message.id == id && (withdrawn || message.awaits_user()))
        else {
            return false;
        };
        let history = self.history.clone();
        let drafts = self.composer_drafts.clone();
        let sidebar = self.sidebar.clone();
        let images = if withdrawn {
            self.queued_images.get(id).cloned().unwrap_or_default()
        } else {
            Vec::new()
        };
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
        if withdrawn {
            self.history.remove(index);
        }
        if let Err(error) = self.persist_history() {
            self.push_notice(format!("Could not save the draft: {error}"));
            if !withdrawn {
                self.history = history;
                self.composer_drafts = drafts;
                self.sidebar = sidebar;
                return false;
            }
            // Withdrawal is already durable and cannot be undone. Preserve the
            // recovered editor contents in memory even when saving its draft fails.
        }
        if withdrawn {
            self.queued_images.remove(id);
        }
        if active {
            self.composer.attachments.extend(images);
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
            self.push_notice(format!("Could not save the rejection state: {error}"));
            return false;
        }
        // A refusal after the bubble went up takes the bubble back.
        self.transcripts.remove_host_input(&thread, id);
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
