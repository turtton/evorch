//! Host inputs this incarnation appended, and which of them a committed
//! snapshot holds. Delivery is the snapshot commit, never the append.
use storage::{HostInputStatus, HostInputTransition};

use super::LoopState;

#[derive(Debug, Default)]
pub(crate) struct HostInputLedger {
    /// Prompt IDs in the last committed snapshot's `host_inputs_json`, kept
    /// after a compaction summarizes their messages.
    recorded: Vec<String>,
    /// Appended to the raw history at this index but not yet committed.
    appended: Vec<(String, usize)>,
}

/// The host inputs one snapshot write records.
#[derive(Debug, Default)]
pub(crate) struct SnapshotHostInputs {
    /// Every prompt ID the snapshot holds, for `host_inputs_json`.
    pub(crate) ids: Vec<String>,
    /// IDs this write delivers for the first time.
    pub(crate) delivered: Vec<String>,
}

impl HostInputLedger {
    pub(crate) fn restored(recorded: Vec<String>) -> Self {
        Self {
            recorded,
            appended: Vec::new(),
        }
    }

    pub(crate) fn contains(&self, prompt_id: &str) -> bool {
        self.recorded.iter().any(|id| id == prompt_id)
            || self.appended.iter().any(|(id, _)| id == prompt_id)
    }

    /// Record an append at raw history `index`; a known ID is ignored.
    pub(crate) fn append(&mut self, prompt_id: String, index: usize) {
        if !self.contains(&prompt_id) {
            self.appended.push((prompt_id, index));
        }
    }

    /// What a snapshot of the raw history up to `end` holds. The raw history is
    /// append-only, so an index stays valid across compactions.
    pub(crate) fn snapshot(&self, end: usize) -> SnapshotHostInputs {
        let delivered: Vec<String> = self
            .appended
            .iter()
            .filter(|(_, index)| *index < end)
            .map(|(id, _)| id.clone())
            .collect();
        let ids = self.recorded.iter().chain(&delivered).cloned().collect();
        SnapshotHostInputs { ids, delivered }
    }

    /// The snapshot holding `delivered` committed; later snapshots keep them.
    pub(crate) fn committed(&mut self, delivered: &[String]) {
        self.appended.retain(|(id, _)| {
            let committed = delivered.contains(id);
            if committed {
                self.recorded.push(id.clone());
            }
            !committed
        });
    }

    /// Appended inputs no committed snapshot holds: the model may have read
    /// them, but nothing durable proves it.
    pub(crate) fn unrecorded(&self) -> impl Iterator<Item = &str> {
        self.appended.iter().map(|(id, _)| id.as_str())
    }
}

impl LoopState {
    /// The one place host and internal input enters the context: adopt its
    /// identity, append it with its images, and note a tracked host input.
    pub(crate) fn take_user_input(&mut self, input: crate::runtime::user_inbox::UserInput) {
        let tracked = crate::runtime::user_inbox::tracked_prompt_id(&input).map(String::from);
        let (text, images, trusted_user, prompt_id, _) = input;
        self.accept_input_identity(trusted_user, prompt_id);
        self.append_user_message(&text, images, tracked);
    }

    /// Append a user message without changing the request turn.
    pub(crate) fn append_user_message(
        &mut self,
        text: &str,
        images: Vec<crate::DelegateImage>,
        tracked: Option<String>,
    ) {
        self.context.push_user(text);
        if let Some(message) = self.context.messages.last_mut() {
            message.content.extend(images.into_iter().map(|image| {
                providers::ContentBlock::Image {
                    media_type: image.media_type,
                    data: image.data,
                }
            }));
        }
        if let Some(prompt_id) = tracked {
            if self.initial_prompt_pending.as_deref() == Some(prompt_id.as_str()) {
                self.initial_prompt_pending = None;
            }
            let index = self.context.messages.len() - 1;
            self.host_inputs().append(prompt_id, index);
        }
    }

    pub(crate) fn host_inputs(&self) -> std::sync::MutexGuard<'_, HostInputLedger> {
        self.host_input_ledger
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Take every host input still waiting in this incarnation's inbox after it
    /// was sealed: none of them reached the context. The pending count is left
    /// as it is; a closed inbox reports it to the host as not consumed.
    pub(crate) fn drain_undelivered_inbox(&mut self) -> Vec<String> {
        let mut messages = std::mem::take(&mut self.pending_user_messages);
        while let Ok(message) = self.channels.inbox_rx.try_recv() {
            messages.push(message);
        }
        let mut dropped: Vec<String> = messages
            .iter()
            .filter_map(|message| {
                crate::runtime::user_inbox::tracked_prompt_id(message).map(String::from)
            })
            .collect();
        // A prompt the run ended before appending, such as after a failed setup.
        dropped.extend(self.initial_prompt_pending.take());
        dropped
    }

    /// Receipt changes for a run ending with `dropped` never read: those are
    /// dropped and unrecorded appends are unknown. Inputs a snapshot of `end`
    /// holds are delivered by that snapshot instead.
    pub(crate) fn terminal_host_inputs(
        &self,
        dropped: &[String],
        end: Option<usize>,
        unknown_reason: &str,
    ) -> Vec<HostInputTransition> {
        let ledger = self.host_inputs();
        let held = end
            .map(|end| ledger.snapshot(end).delivered)
            .unwrap_or_default();
        dropped
            .iter()
            .map(|id| {
                HostInputTransition::new(id.clone(), HostInputStatus::Dropped, Some("run_ended"))
            })
            .chain(
                ledger
                    .unrecorded()
                    .filter(|id| !held.iter().any(|held| held == id))
                    .map(|id| {
                        HostInputTransition::new(id, HostInputStatus::Unknown, Some(unknown_reason))
                    }),
            )
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshots_deliver_appends_inside_their_end_once_and_keep_recorded_ids() {
        let mut ledger = HostInputLedger::restored(vec!["old".into()]);
        ledger.append("a".into(), 3);
        ledger.append("b".into(), 7);
        ledger.append("a".into(), 9);
        ledger.append("old".into(), 1);

        let first = ledger.snapshot(5);
        assert_eq!(first.ids, ["old", "a"]);
        assert_eq!(first.delivered, ["a"]);
        // A failed write commits nothing; the next snapshot retries the same set.
        assert_eq!(ledger.snapshot(5).delivered, ["a"]);
        ledger.committed(&first.delivered);
        let second = ledger.snapshot(8);
        assert_eq!(second.ids, ["old", "a", "b"]);
        assert_eq!(second.delivered, ["b"]);
        assert_eq!(ledger.unrecorded().collect::<Vec<_>>(), ["b"]);
        ledger.committed(&second.delivered);
        assert_eq!(ledger.snapshot(8).ids, ["old", "a", "b"]);
        assert!(ledger.snapshot(8).delivered.is_empty());
    }
}
