//! Rebuilds responses the storage bridge had to store in several rows.

use std::{borrow::Cow, collections::BTreeMap};

use crate::event::{Event, EventKind, LifecycleEvent, MessageEvent};

const MAX_PENDING_RUNS: usize = 64;
const MAX_PENDING_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug)]
struct PendingResponse {
    response_id: String,
    total: usize,
    next: usize,
    text: String,
    order: u64,
}

/// Rebuilds storage-only response slices, requiring one response identity and
/// every index from zero through `total - 1`. Different runs may interleave.
///
/// A run's non-part event closes an incomplete sequence. Replay retains at most
/// 64 unfinished runs and 64 MiB of decoded text: oldest unfinished runs are
/// evicted for a new run; a response exceeding the byte budget is discarded.
/// Discarded/incomplete responses never become completed messages.
#[derive(Debug)]
pub struct MessagePartAssembler {
    open: BTreeMap<String, PendingResponse>,
    buffered_bytes: usize,
    next_order: u64,
    max_runs: usize,
    max_bytes: usize,
}

impl Default for MessagePartAssembler {
    fn default() -> Self {
        Self {
            open: BTreeMap::new(),
            buffered_bytes: 0,
            next_order: 0,
            max_runs: MAX_PENDING_RUNS,
            max_bytes: MAX_PENDING_BYTES,
        }
    }
}

impl MessagePartAssembler {
    pub fn new() -> Self {
        Self::default()
    }

    fn discard(&mut self, run: &str) -> Option<PendingResponse> {
        let pending = self.open.remove(run)?;
        self.buffered_bytes -= pending.text.len();
        Some(pending)
    }

    /// Returns ordinary events unchanged, nothing for incomplete/invalid
    /// sequences, and a reconstructed completion after its final slice.
    pub fn accept<'a>(&mut self, event: &'a Event) -> Option<Cow<'a, Event>> {
        let EventKind::Message(MessageEvent::MessagePart {
            run_id,
            response_id,
            text,
            index,
            total,
        }) = &event.kind
        else {
            let run = event.primary_run_id().or(match &event.kind {
                EventKind::Lifecycle(
                    LifecycleEvent::Started { session_id, .. }
                    | LifecycleEvent::Completed { session_id }
                    | LifecycleEvent::Failed { session_id, .. },
                ) => Some(session_id.as_str()),
                _ => None,
            });
            if let Some(run) = run {
                self.discard(run);
            }
            return Some(Cow::Borrowed(event));
        };
        if response_id.is_empty() || *total == 0 || index >= total {
            self.discard(run_id);
            return None;
        }
        if *index == 0 {
            self.discard(run_id);
            if self.max_runs == 0 {
                return None;
            }
            if self.open.len() >= self.max_runs {
                let oldest = self
                    .open
                    .iter()
                    .min_by_key(|(_, pending)| pending.order)?
                    .0
                    .clone();
                self.discard(&oldest);
            }
            self.open.insert(
                run_id.clone(),
                PendingResponse {
                    response_id: response_id.clone(),
                    total: *total,
                    next: 0,
                    text: String::new(),
                    order: self.next_order,
                },
            );
            self.next_order = self.next_order.wrapping_add(1);
        }
        let pending = self.open.get_mut(run_id)?;
        if pending.response_id != *response_id
            || pending.total != *total
            || pending.next != *index
            || self.buffered_bytes.saturating_add(text.len()) > self.max_bytes
        {
            self.discard(run_id);
            return None;
        }
        pending.text.push_str(text);
        self.buffered_bytes += text.len();
        pending.next += 1;
        if pending.next != pending.total {
            return None;
        }
        let text = self.discard(run_id)?.text;
        Some(Cow::Owned(Event {
            meta: event.meta.clone(),
            kind: EventKind::Message(MessageEvent::MessageCompleted {
                run_id: run_id.clone(),
                text,
            }),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AgentRunPhase;

    fn part(run: &str, response: &str, text: &str, index: usize, total: usize) -> Event {
        Event::new(MessageEvent::MessagePart {
            run_id: run.into(),
            response_id: response.into(),
            text: text.into(),
            index,
            total,
        })
    }

    fn completed_text(event: &Event) -> &str {
        let EventKind::Message(MessageEvent::MessageCompleted { text, .. }) = &event.kind else {
            panic!("expected completion")
        };
        text
    }

    #[test]
    fn interleaved_runs_rebuild_their_own_completion_and_preserve_metadata() {
        let mut assembler = MessagePartAssembler::new();
        assert!(assembler.accept(&part("a", "a1", "one ", 0, 2)).is_none());
        assert!(assembler.accept(&part("b", "b1", "x", 0, 2)).is_none());
        let last_a = part("a", "a1", "two", 1, 2);
        let done = assembler.accept(&last_a).unwrap();
        assert_eq!(done.meta, last_a.meta);
        assert_eq!(completed_text(&done), "one two");
        let last_b = part("b", "b1", "y", 1, 2);
        assert_eq!(completed_text(&assembler.accept(&last_b).unwrap()), "xy");
        assert_eq!(assembler.buffered_bytes, 0);
    }

    #[test]
    fn missing_tail_then_missing_head_of_another_response_never_form_a_completion() {
        let mut assembler = MessagePartAssembler::new();
        assert!(
            assembler
                .accept(&part("a", "response-a", "A-prefix", 0, 2))
                .is_none()
        );
        assert!(
            assembler
                .accept(&part("a", "response-b", "B-tail", 1, 2))
                .is_none()
        );
        assert!(
            assembler
                .accept(&part("a", "response-c", "C-prefix", 0, 2))
                .is_none()
        );
        let last = part("a", "response-c", "C-tail", 1, 2);
        assert_eq!(
            completed_text(&assembler.accept(&last).unwrap()),
            "C-prefixC-tail"
        );
    }

    #[test]
    fn missing_middle_inconsistent_total_and_invalid_indices_are_not_completed() {
        for (index, total) in [(2, 3), (1, 2), (3, 3), (1, 0)] {
            let mut assembler = MessagePartAssembler::new();
            assert!(assembler.accept(&part("a", "id", "start", 0, 3)).is_none());
            assert!(
                assembler
                    .accept(&part("a", "id", "invalid", index, total))
                    .is_none()
            );
            assert_eq!(assembler.buffered_bytes, 0);
        }
    }

    #[test]
    fn terminal_and_response_boundaries_discard_unfinished_text() {
        let boundaries = [
            Event::new(MessageEvent::MessageCompleted {
                run_id: "a".into(),
                text: "whole".into(),
            }),
            Event::new(MessageEvent::StreamRetried {
                run_id: Some("a".into()),
            }),
            Event::new(LifecycleEvent::AgentRunStateChanged {
                run_id: "a".into(),
                from: AgentRunPhase::Running,
                to: AgentRunPhase::Stopped,
                reason: None,
            }),
        ];
        for boundary in boundaries {
            let mut assembler = MessagePartAssembler::new();
            assert!(assembler.accept(&part("a", "id", "stale", 0, 2)).is_none());
            assert_eq!(assembler.accept(&boundary).as_deref(), Some(&boundary));
            assert!(assembler.open.is_empty());
            assert_eq!(assembler.buffered_bytes, 0);
            assert!(assembler.accept(&part("a", "id", "tail", 1, 2)).is_none());
        }
    }

    #[test]
    fn unfinished_run_and_byte_budgets_do_not_accumulate_or_poison_later_responses() {
        let mut assembler = MessagePartAssembler {
            max_runs: 2,
            max_bytes: 6,
            ..Default::default()
        };
        for run in ["a", "b", "c"] {
            assert!(assembler.accept(&part(run, run, "abc", 0, 2)).is_none());
            assert!(assembler.open.len() <= 2);
            assert!(assembler.buffered_bytes <= 6);
        }
        assert!(assembler.accept(&part("a", "a", "end", 1, 2)).is_none());
        // A further fragment exceeds the aggregate byte budget, dropping c.
        assert!(assembler.accept(&part("c", "c", "end", 1, 2)).is_none());
        let valid = part("b", "new", "ok", 0, 1);
        assert_eq!(completed_text(&assembler.accept(&valid).unwrap()), "ok");
        assert_eq!(assembler.buffered_bytes, 0);
    }
}
