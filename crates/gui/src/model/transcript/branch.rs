//! Fork and rewind boundaries inside a transcript.

use workspace_ui::{ForkPoint, LineageKind};

use super::{TranscriptEntry, TranscriptModel};

impl TranscriptModel {
    /// Entry by absolute id, if it has not been evicted.
    pub fn entry_by_id(&self, entry_id: usize) -> Option<&TranscriptEntry> {
        self.entries.get(entry_id.checked_sub(self.first_entry_id)?)
    }

    /// The boundary recorded for a completed turn, if `entry_id` is one.
    pub fn fork_point(&self, entry_id: usize) -> Option<ForkPoint> {
        match self.entry_by_id(entry_id)? {
            TranscriptEntry::TurnEnd {
                run_id,
                context_len,
            } => Some(ForkPoint {
                run_id: run_id.clone(),
                context_len: *context_len,
            }),
            _ => None,
        }
    }

    /// The newest branch marker: this conversation's own departure point.
    pub fn last_branch_entry_id(&self) -> Option<usize> {
        self.entries
            .iter()
            .rposition(|entry| matches!(entry, TranscriptEntry::Branch { .. }))
            .map(|index| self.first_entry_id + index)
    }

    /// The latest completed turn in this transcript.
    pub fn last_fork_point(&self) -> Option<ForkPoint> {
        let index = self
            .entries
            .iter()
            .rposition(|entry| matches!(entry, TranscriptEntry::TurnEnd { .. }))?;
        self.fork_point(self.first_entry_id + index)
    }

    /// Completed turns before `entry_id`, for "turn N" labels.
    pub fn turn_number(&self, entry_id: usize) -> usize {
        let end = entry_id
            .saturating_sub(self.first_entry_id)
            .min(self.entries.len());
        self.entries[..end]
            .iter()
            .filter(|entry| matches!(entry, TranscriptEntry::TurnEnd { .. }))
            .count()
    }

    /// For the first user message of a turn, the boundary to rewind to before it:
    /// `Some(None)` is the start of the conversation. Later messages of the same
    /// turn and other entries return `None`.
    pub fn edit_point(&self, entry_id: usize) -> Option<Option<ForkPoint>> {
        let index = entry_id.checked_sub(self.first_entry_id)?;
        if !matches!(
            self.entries.get(index)?,
            TranscriptEntry::UserMessage { .. }
        ) {
            return None;
        }
        let previous = self.entries[..index]
            .iter()
            .enumerate()
            .rev()
            .find(|(_, entry)| {
                matches!(
                    entry,
                    TranscriptEntry::TurnEnd { .. }
                        | TranscriptEntry::Branch { .. }
                        | TranscriptEntry::UserMessage { .. }
                )
            });
        match previous {
            Some((_, TranscriptEntry::UserMessage { .. })) => None,
            Some((previous, TranscriptEntry::TurnEnd { .. })) => {
                Some(Some(self.fork_point(self.first_entry_id + previous)?))
            }
            Some((previous, _)) => {
                // A branch marker: rewind to the inherited boundary just before it.
                let turn = self.entries[..previous]
                    .iter()
                    .rposition(|entry| matches!(entry, TranscriptEntry::TurnEnd { .. }));
                Some(turn.and_then(|turn| self.fork_point(self.first_entry_id + turn)))
            }
            None if self.first_entry_id == 0 => Some(None),
            None => None,
        }
    }

    /// Text of a user message by absolute id.
    /// The typed text, without skill bodies inlined at send time.
    pub fn user_text(&self, entry_id: usize) -> Option<&str> {
        match self.entry_by_id(entry_id)? {
            TranscriptEntry::UserMessage { text } => {
                Some(crate::model::composer::split_skill_attachments(text).0)
            }
            _ => None,
        }
    }

    /// A new transcript holding this history up to `point` and a branch marker.
    /// `None` when the boundary is not (or no longer) in this transcript.
    pub(crate) fn branch_at(
        &self,
        point: Option<&ForkPoint>,
        kind: LineageKind,
        source_thread_id: &str,
    ) -> Option<Self> {
        let end = match point {
            None => 0,
            Some(point) => {
                self.entries.iter().rposition(|entry| {
                    matches!(entry, TranscriptEntry::TurnEnd { run_id, context_len }
                        if run_id == &point.run_id && *context_len == point.context_len)
                })? + 1
            }
        };
        let mut branch = Self::with_capacity(self.capacity);
        branch.agent_names.clone_from(&self.agent_names);
        for entry in &self.entries[..end] {
            branch.push(entry.clone());
        }
        branch.push(TranscriptEntry::Branch {
            kind,
            source_thread_id: source_thread_id.into(),
        });
        Some(branch)
    }
}
