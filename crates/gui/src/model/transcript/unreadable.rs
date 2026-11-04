//! Stored events the history could not read, shown where they belong.

use super::{EntryMeta, TranscriptEntry, TranscriptModel};
use crate::body::{BodyRef, FailureKind};

/// Whether two entries are the same stream of one run: the pieces of what a
/// replay shows as one entry (the slices of a reasoning, the deltas of a
/// message, a call's start and result) are, whatever their text.
fn same_stream(shown: &TranscriptEntry, replayed: &TranscriptEntry) -> bool {
    match (shown, replayed) {
        (TranscriptEntry::Unreadable { .. }, _) | (_, TranscriptEntry::Unreadable { .. }) => false,
        (
            TranscriptEntry::Message { run_id, .. },
            TranscriptEntry::Message {
                run_id: replayed_run,
                ..
            },
        )
        | (
            TranscriptEntry::Reasoning { run_id, .. },
            TranscriptEntry::Reasoning {
                run_id: replayed_run,
                ..
            },
        ) => run_id == replayed_run,
        (
            TranscriptEntry::Tool {
                run_id, call_id, ..
            },
            TranscriptEntry::Tool {
                run_id: replayed_run,
                call_id: replayed_call,
                ..
            },
        ) => run_id == replayed_run && call_id == replayed_call,
        (
            TranscriptEntry::Compaction { checkpoint_id, .. },
            TranscriptEntry::Compaction {
                checkpoint_id: replayed_checkpoint,
                ..
            },
        ) => checkpoint_id == replayed_checkpoint,
        (shown, replayed) => std::mem::discriminant(shown) == std::mem::discriminant(replayed),
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
        let source = self.source.replace((event_id, event_id));
        self.push(TranscriptEntry::Unreadable {
            event_id,
            run_id: run_id.map(str::to_owned),
            kind,
            reason: reason.to_owned(),
            reference,
        });
        self.source = source;
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

    /// Fills the gap of `event_id` from `replayed`, a model built afresh from
    /// the rows of the same run. What the event is part of is read from the
    /// stored events each entry was made from, not from its text: the entries
    /// of the replay that the event touched take the place of the entries here
    /// that were made from the same events (the gap, the pieces of a response
    /// that the missing slice completes, a call shown without its result), so
    /// a response never shows twice. Entries made from other events, host
    /// inputs, and what arrived since the rows were read are left where they
    /// are, and the entries that stay keep their identity. Returns whether the
    /// gap was here.
    pub(crate) fn fill_unreadable_from(
        &mut self,
        event_id: i64,
        replayed: &TranscriptModel,
    ) -> bool {
        let Some(gap) = self.unreadable_index(event_id) else {
            return false;
        };
        let candidates: Vec<(TranscriptEntry, (i64, i64))> = replayed
            .entries
            .iter()
            .zip(&replayed.entry_meta)
            .filter(|(entry, _)| !matches!(entry, TranscriptEntry::Unreadable { .. }))
            .filter_map(|(entry, meta)| meta.src.map(|src| (entry.clone(), src)))
            .collect();
        let overlap = |left: (i64, i64), right: (i64, i64)| left.0 <= right.1 && right.0 <= left.1;
        // The entries of the replay the event touched, and every entry on either
        // side that was made from the same events (a response whose first
        // slice and completion both fall in the span, say), until no more join.
        let mut taken: Vec<bool> = candidates
            .iter()
            .map(|(_, src)| src.0 <= event_id && event_id <= src.1)
            .collect();
        let mut slots: Vec<usize> = Vec::new();
        loop {
            let mut changed = false;
            for (index, (entry, meta)) in self.entries.iter().zip(&self.entry_meta).enumerate() {
                let Some(src) = meta.src else { continue };
                if index == gap || slots.contains(&index) {
                    continue;
                }
                let related = candidates
                    .iter()
                    .zip(&taken)
                    .any(|((replayed, span), taken)| {
                        *taken && overlap(src, *span) && same_stream(entry, replayed)
                    });
                if related {
                    slots.push(index);
                    changed = true;
                }
            }
            for index in 0..candidates.len() {
                if taken[index] {
                    continue;
                }
                let (replayed, span) = &candidates[index];
                let related = slots.iter().any(|slot| {
                    self.entry_meta[*slot]
                        .src
                        .is_some_and(|src| overlap(src, *span))
                        && same_stream(&self.entries[*slot], replayed)
                });
                if related {
                    taken[index] = true;
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        let fresh: Vec<(TranscriptEntry, (i64, i64))> = candidates
            .into_iter()
            .zip(taken)
            .filter(|(_, taken)| *taken)
            .map(|(candidate, _)| candidate)
            .collect();
        slots.push(gap);
        slots.sort_unstable();
        slots.dedup();
        let placed = slots.len().min(fresh.len());
        let mut fresh = fresh.into_iter();
        for (slot, (entry, src)) in slots.iter().zip(fresh.by_ref()) {
            self.entries[*slot] = entry;
            self.entry_meta[*slot].revision += 1;
            self.entry_meta[*slot].src = Some(src);
        }
        // More entries than places: the rest follow the last place.
        let after_last = slots.get(placed.wrapping_sub(1)).map_or(gap, |last| *last) + 1;
        for (at, (entry, src)) in (after_last..).zip(fresh) {
            self.entries.insert(at, entry);
            self.entry_meta.insert(
                at,
                EntryMeta {
                    id: self.next_entry_id,
                    revision: 0,
                    src: Some(src),
                },
            );
            self.next_entry_id += 1;
            if self.view_start >= at {
                self.view_start += 1;
            }
        }
        // Fewer: the places left over close.
        for slot in slots[placed..].iter().rev() {
            self.remove_entry(*slot);
        }
        true
    }
}
