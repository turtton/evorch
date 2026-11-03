//! Stored events the history could not read, shown where they belong.

use super::{EntryMeta, TranscriptEntry, TranscriptModel};
use crate::body::FailureKind;

impl TranscriptModel {
    /// Marks a place the history could not read. One per event: a second
    /// report of the same event (the other replay pass) changes nothing.
    pub(crate) fn push_unreadable(
        &mut self,
        event_id: i64,
        run_id: Option<&str>,
        kind: FailureKind,
        reason: &str,
    ) {
        if self.unreadable_index(event_id).is_some() {
            return;
        }
        self.push(TranscriptEntry::Unreadable {
            event_id,
            run_id: run_id.map(str::to_owned),
            kind,
            reason: reason.to_owned(),
        });
    }

    fn unreadable_index(&self, event_id: i64) -> Option<usize> {
        self.entries.iter().position(
            |entry| matches!(entry, TranscriptEntry::Unreadable { event_id: id, .. } if *id == event_id),
        )
    }

    pub(crate) fn has_unreadable(&self, event_id: i64) -> bool {
        self.unreadable_index(event_id).is_some()
    }

    /// Records why a read of the event failed again.
    pub(crate) fn update_unreadable(&mut self, event_id: i64, kind: FailureKind, reason: &str) {
        let Some(index) = self.unreadable_index(event_id) else {
            return;
        };
        if let Some(TranscriptEntry::Unreadable {
            kind: shown_kind,
            reason: shown,
            ..
        }) = self.entry_mut(index)
        {
            *shown_kind = kind;
            reason.clone_into(shown);
        }
    }

    /// Puts `replacements` where the unreadable event was: the first in its
    /// place, the rest right after it. With none, the gap simply closes.
    /// Returns whether the event was shown as unreadable here.
    pub(crate) fn replace_unreadable(
        &mut self,
        event_id: i64,
        replacements: Vec<TranscriptEntry>,
    ) -> bool {
        let Some(index) = self.unreadable_index(event_id) else {
            return false;
        };
        let mut replacements = replacements.into_iter();
        let Some(first) = replacements.next() else {
            self.remove_entry(index);
            return true;
        };
        self.entries[index] = first;
        self.entry_meta[index].revision += 1;
        for (at, entry) in (index + 1..).zip(replacements) {
            self.entries.insert(at, entry);
            self.entry_meta.insert(
                at,
                EntryMeta {
                    id: self.next_entry_id,
                    revision: 0,
                },
            );
            self.next_entry_id += 1;
            if self.view_start >= at {
                self.view_start += 1;
            }
        }
        true
    }
}
