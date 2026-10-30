//! Fork and rewind boundaries inside a transcript.

use workspace_ui::{ForkPoint, LineageKind};

use super::{TranscriptEntry, TranscriptModel};

impl TranscriptModel {
    /// Entry by absolute id, if it has not been evicted.
    pub fn entry_by_id(&self, entry_id: usize) -> Option<&TranscriptEntry> {
        self.entries.get(self.entry_index(entry_id)?)
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
            .map(|index| self.entry_meta[index].id)
    }

    /// The latest completed turn in this transcript.
    pub fn last_fork_point(&self) -> Option<ForkPoint> {
        let index = self
            .entries
            .iter()
            .rposition(|entry| matches!(entry, TranscriptEntry::TurnEnd { .. }))?;
        self.fork_point(self.entry_meta[index].id)
    }

    /// Completed turns before `entry_id`, for "turn N" labels.
    pub fn turn_number(&self, entry_id: usize) -> usize {
        let end = self.entry_meta.partition_point(|meta| meta.id < entry_id);
        self.entries[..end]
            .iter()
            .filter(|entry| matches!(entry, TranscriptEntry::TurnEnd { .. }))
            .count()
    }

    /// For the first user message of a turn, the boundary to rewind to before it:
    /// `Some(None)` is the start of the conversation. Later messages of the same
    /// turn and other entries return `None`.
    pub fn edit_point(&self, entry_id: usize) -> Option<Option<ForkPoint>> {
        let index = self.entry_index(entry_id)?;
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
                Some(Some(self.fork_point(self.entry_meta[previous].id)?))
            }
            Some((previous, _)) => {
                // A branch marker: rewind to the inherited boundary just before it.
                let turn = self.entries[..previous]
                    .iter()
                    .rposition(|entry| matches!(entry, TranscriptEntry::TurnEnd { .. }));
                Some(turn.and_then(|turn| self.fork_point(self.entry_meta[turn].id)))
            }
            None if !self.history_evicted => Some(None),
            None => None,
        }
    }

    /// The user message starting the turn after the completed turn `entry_id`,
    /// when editing it rewinds to that same boundary; it then offers that rewind.
    pub fn edit_after_turn(&self, entry_id: usize) -> Option<usize> {
        let point = self.fork_point(entry_id)?;
        let index = self.entry_index(entry_id)?;
        let next = self.entries[index + 1..].iter().position(|entry| {
            matches!(
                entry,
                TranscriptEntry::UserMessage { .. } | TranscriptEntry::TurnEnd { .. }
            )
        })?;
        let next_id = self.entry_meta[index + 1 + next].id;
        (self.edit_point(next_id) == Some(Some(point))).then_some(next_id)
    }

    /// Whether anything but branch markers follows `entry_id`: a rewind to it
    /// would drop something.
    pub fn continues_after(&self, entry_id: usize) -> bool {
        self.entry_index(entry_id).is_some_and(|index| {
            self.entries
                .iter()
                .skip(index + 1)
                .any(|entry| !matches!(entry, TranscriptEntry::Branch { .. }))
        })
    }

    /// The first user message, when editing it restarts the conversation.
    pub fn start_edit_message(&self) -> Option<usize> {
        let index = self.entries.iter().position(|entry| {
            matches!(
                entry,
                TranscriptEntry::UserMessage { .. } | TranscriptEntry::TurnEnd { .. }
            )
        })?;
        let id = self.entry_meta[index].id;
        (self.edit_point(id) == Some(None)).then_some(id)
    }

    /// Text of a user message by absolute id.
    pub fn user_text(&self, entry_id: usize) -> Option<&str> {
        match self.entry_by_id(entry_id)? {
            TranscriptEntry::UserMessage { text, .. } => Some(text),
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
