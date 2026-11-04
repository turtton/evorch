//! Stored events the history could not read, shown where they belong.

use super::{EntryMeta, TranscriptEntry, TranscriptModel};
use crate::body::{BodyRef, FailureKind};

/// How an entry of a fresh replay relates to one already shown.
enum Slot {
    /// The same entry.
    Same,
    /// The same call, with a result the shown one lacks.
    Completed,
    /// Another entry.
    Other,
}

fn slot(shown: &TranscriptEntry, replayed: &TranscriptEntry) -> Slot {
    if shown == replayed {
        return Slot::Same;
    }
    match (shown, replayed) {
        (
            TranscriptEntry::Unreadable { event_id, .. },
            TranscriptEntry::Unreadable {
                event_id: replayed_id,
                ..
            },
        ) if event_id == replayed_id => Slot::Same,
        (
            TranscriptEntry::Tool {
                call_id,
                run_id,
                output: None,
                ..
            },
            TranscriptEntry::Tool {
                call_id: replayed_call,
                run_id: replayed_run,
                output: Some(_),
                ..
            },
        ) if call_id == replayed_call && run_id == replayed_run => Slot::Completed,
        _ => Slot::Other,
    }
}

impl TranscriptModel {
    /// Marks a place the history could not read. One per event: a second
    /// report of the same event (the other replay pass) changes nothing.
    pub(crate) fn push_unreadable(
        &mut self,
        event_id: i64,
        run_id: Option<&str>,
        kind: FailureKind,
        reason: &str,
        reference: Option<BodyRef>,
    ) {
        if self.unreadable_index(event_id).is_some() {
            return;
        }
        self.push(TranscriptEntry::Unreadable {
            event_id,
            run_id: run_id.map(str::to_owned),
            kind,
            reason: reason.to_owned(),
            reference,
        });
    }

    fn unreadable_index(&self, event_id: i64) -> Option<usize> {
        self.entries.iter().position(
            |entry| matches!(entry, TranscriptEntry::Unreadable { event_id: id, .. } if *id == event_id),
        )
    }

    /// Records why a read of the event failed again.
    pub(crate) fn update_unreadable(
        &mut self,
        event_id: i64,
        kind: Option<FailureKind>,
        reason: &str,
        reference: Option<BodyRef>,
    ) {
        let Some(index) = self.unreadable_index(event_id) else {
            return;
        };
        if let Some(TranscriptEntry::Unreadable {
            kind: shown_kind,
            reason: shown,
            reference: shown_reference,
            ..
        }) = self.entry_mut(index)
        {
            if let Some(kind) = kind {
                *shown_kind = kind;
                *shown_reference = reference;
            }
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

    /// Fills the gap of `event_id` from `replayed`, a model built afresh from
    /// the rows of the same run. The entries the two models share around the
    /// gap are matched in order, so everything shown keeps its place and its
    /// identity; what lies between them in the replay is what the gap held. A
    /// call that the replay shows with its result takes that result in place.
    /// Entries only this model has (host inputs, what arrived since the rows
    /// were read) are left where they are. Returns whether the gap was here.
    pub(crate) fn fill_unreadable_from(
        &mut self,
        event_id: i64,
        replayed: &TranscriptModel,
    ) -> bool {
        let Some(gap) = self.unreadable_index(event_id) else {
            return false;
        };
        let fresh = replayed.entries();
        let shown = &self.entries;
        let mut completed: Vec<(usize, TranscriptEntry)> = Vec::new();
        let mut start = 0;
        for (index, entry) in shown[..gap].iter().enumerate() {
            let Some(next) = fresh.get(start) else { break };
            match slot(entry, next) {
                Slot::Same => start += 1,
                Slot::Completed => {
                    completed.push((index, next.clone()));
                    start += 1;
                }
                Slot::Other => {}
            }
        }
        let mut end = fresh.len();
        for (index, entry) in shown.iter().enumerate().skip(gap + 1).rev() {
            if end <= start {
                break;
            }
            match slot(entry, &fresh[end - 1]) {
                Slot::Same => end -= 1,
                Slot::Completed => {
                    completed.push((index, fresh[end - 1].clone()));
                    end -= 1;
                }
                Slot::Other => {}
            }
        }
        let between = fresh[start..end.max(start)].to_vec();
        for (index, entry) in completed {
            if let Some(shown) = self.entry_mut(index) {
                *shown = entry;
            }
        }
        self.replace_unreadable(event_id, between)
    }
}
