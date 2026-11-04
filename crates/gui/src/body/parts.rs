//! Rebuilds a response stored in several rows, without reading any of it.
//!
//! The rules are those of [`event_bus::MessagePartAssembler`] (one response
//! identity, every index from zero, a non-part event of the run closes an
//! unfinished sequence, at most 64 unfinished runs and 64 MiB), counted in the
//! declared length of each slice. A rebuilt response keeps its slices as the
//! pieces of one [`BodyText`]; nothing is joined before it is shown.

use std::collections::BTreeMap;

use event_bus::{Event, EventKind, LifecycleEvent, MessageEvent};

use super::backend::DeferredEvent;
use super::text::BodyText;

const MAX_PENDING_RUNS: usize = 64;
const MAX_PENDING_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug)]
struct Pending {
    /// The id of the slice that began the response.
    first_id: i64,
    response_id: String,
    total: usize,
    next: usize,
    body: BodyText,
    bytes: usize,
    order: u64,
}

/// What [`DeferredPartAssembler::accept`] made of a row.
#[derive(Debug, PartialEq)]
pub enum Accepted<'a> {
    /// An ordinary event, unchanged.
    Event(&'a DeferredEvent),
    /// The last slice arrived: the response, as a completion.
    Completed(Box<DeferredEvent>),
    /// A slice that completes nothing yet, or one that was discarded.
    Nothing,
}

#[derive(Debug)]
pub struct DeferredPartAssembler {
    open: BTreeMap<String, Pending>,
    buffered_bytes: usize,
    next_order: u64,
    max_runs: usize,
    max_bytes: usize,
}

impl Default for DeferredPartAssembler {
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

impl DeferredPartAssembler {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn with_limits(max_runs: usize, max_bytes: usize) -> Self {
        Self {
            max_runs,
            max_bytes,
            ..Self::default()
        }
    }

    fn discard(&mut self, run: &str) -> Option<Pending> {
        let pending = self.open.remove(run)?;
        self.buffered_bytes -= pending.bytes;
        Some(pending)
    }

    /// An event that could not be read breaks the run's sequence too: what
    /// follows can no longer be the rest of it.
    pub fn interrupt(&mut self, run: Option<&str>) {
        if let Some(run) = run {
            self.discard(run);
        }
    }

    /// The unfinished sequences, oldest first (the order they began in, not
    /// their runs' names), for the caller to hand on or resolve. Handing them
    /// on in this order keeps the next assembler's eviction of its oldest
    /// response the same one this assembler would have made.
    pub fn take_open(&mut self) -> Vec<(String, String, usize, usize, BodyText)> {
        let open = std::mem::take(&mut self.open);
        self.buffered_bytes = 0;
        let mut open: Vec<_> = open.into_iter().collect();
        open.sort_by_key(|(_, pending)| pending.order);
        open.into_iter()
            .map(|(run, p)| (run, p.response_id, p.total, p.next, p.body))
            .collect()
    }

    pub fn accept<'a>(&mut self, row: &'a DeferredEvent) -> Accepted<'a> {
        let event = &row.event;
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
            return Accepted::Event(row);
        };
        if response_id.is_empty() || *total == 0 || index >= total {
            self.discard(run_id);
            return Accepted::Nothing;
        }
        if *index == 0 {
            self.discard(run_id);
            if self.max_runs == 0 {
                return Accepted::Nothing;
            }
            if self.open.len() >= self.max_runs {
                let Some(oldest) = self
                    .open
                    .iter()
                    .min_by_key(|(_, pending)| pending.order)
                    .map(|(run, _)| run.clone())
                else {
                    return Accepted::Nothing;
                };
                self.discard(&oldest);
            }
            self.open.insert(
                run_id.clone(),
                Pending {
                    first_id: row.id,
                    response_id: response_id.clone(),
                    total: *total,
                    next: 0,
                    body: BodyText::new(),
                    bytes: 0,
                    order: self.next_order,
                },
            );
            self.next_order = self.next_order.wrapping_add(1);
        }
        let slice = row.body.clone().unwrap_or_else(|| text.as_str().into());
        let slice_len = slice.len();
        let Some(pending) = self.open.get_mut(run_id) else {
            return Accepted::Nothing;
        };
        if pending.response_id != *response_id
            || pending.total != *total
            || pending.next != *index
            || self.buffered_bytes.saturating_add(slice_len) > self.max_bytes
        {
            self.discard(run_id);
            return Accepted::Nothing;
        }
        pending.body.push_body(slice);
        pending.bytes += slice_len;
        self.buffered_bytes += slice_len;
        pending.next += 1;
        if pending.next != pending.total {
            return Accepted::Nothing;
        }
        let Some(done) = self.discard(run_id) else {
            return Accepted::Nothing;
        };
        Accepted::Completed(Box::new(completion(
            row.id,
            done.first_id,
            event,
            run_id.clone(),
            done.body,
        )))
    }
}

/// A completion for `body`: when none of it is in the store the text is
/// inline in the event, as a reader that knows nothing of bodies expects.
fn completion(
    id: i64,
    first_id: i64,
    source: &Event,
    run_id: String,
    body: BodyText,
) -> DeferredEvent {
    let (text, body) = match body.inline() {
        Some(text) => (text.to_owned(), None),
        None => (String::new(), Some(body)),
    };
    DeferredEvent {
        id,
        event: Event {
            meta: source.meta.clone(),
            kind: EventKind::Message(MessageEvent::MessageCompleted { run_id, text }),
        },
        body,
        first_id: Some(first_id),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::body::{BodyRef, DeferredBody};

    fn part(
        run: &str,
        response: &str,
        body: BodyText,
        index: usize,
        total: usize,
    ) -> DeferredEvent {
        DeferredEvent {
            id: index as i64,
            event: Event::new(MessageEvent::MessagePart {
                run_id: run.into(),
                response_id: response.into(),
                text: body.inline().unwrap_or("").to_owned(),
                index,
                total,
            }),
            body: (!body.segments().is_empty() && body.inline().is_none()).then_some(body),
            first_id: None,
        }
    }

    fn deferred(byte: u8, len: u64) -> BodyText {
        BodyText::deferred(DeferredBody::new(
            BodyRef {
                hash: [byte; 32],
                byte_len: len,
            },
            Some("a"),
            Some(1),
        ))
    }

    fn completed(accepted: Accepted<'_>) -> DeferredEvent {
        match accepted {
            Accepted::Completed(done) => *done,
            other => panic!("expected a completion, got {other:?}"),
        }
    }

    #[test]
    fn slices_in_the_store_and_inline_form_one_body_in_order() {
        let mut assembler = DeferredPartAssembler::new();
        let first = part("a", "r", deferred(1, 5000), 0, 3);
        assert_eq!(assembler.accept(&first), Accepted::Nothing);
        let second = part("a", "r", "mid ".into(), 1, 3);
        assert_eq!(assembler.accept(&second), Accepted::Nothing);
        let last = part("a", "r", deferred(2, 7000), 2, 3);
        let done = completed(assembler.accept(&last));
        let body = done.body.expect("still in the store");
        assert_eq!(body.byte_len(), 5000 + 4 + 7000);
        assert_eq!(body.segments().len(), 3);
        assert_eq!(assembler.buffered_bytes, 0);
    }

    #[test]
    fn an_all_inline_response_comes_back_as_plain_text() {
        let mut assembler = DeferredPartAssembler::new();
        assert_eq!(
            assembler.accept(&part("a", "r", "one ".into(), 0, 2)),
            Accepted::Nothing
        );
        let done = completed(assembler.accept(&part("a", "r", "two".into(), 1, 2)));
        assert!(done.body.is_none());
        let EventKind::Message(MessageEvent::MessageCompleted { text, .. }) = done.event.kind
        else {
            panic!("completion")
        };
        assert_eq!(text, "one two");
    }

    #[test]
    fn a_gap_a_foreign_response_and_the_byte_budget_end_the_sequence() {
        let mut assembler = DeferredPartAssembler::with_limits(2, 6000);
        assert_eq!(
            assembler.accept(&part("a", "r", deferred(1, 4000), 0, 2)),
            Accepted::Nothing
        );
        // An unreadable event in the sequence leaves the next slice orphaned.
        assembler.interrupt(Some("a"));
        assert_eq!(
            assembler.accept(&part("a", "r", "tail".into(), 1, 2)),
            Accepted::Nothing
        );
        assert_eq!(assembler.buffered_bytes, 0);
        // Declared lengths count against the budget although nothing is read.
        assert_eq!(
            assembler.accept(&part("b", "r", deferred(3, 4000), 0, 2)),
            Accepted::Nothing
        );
        assert_eq!(
            assembler.accept(&part("b", "r", deferred(4, 4000), 1, 2)),
            Accepted::Nothing
        );
        assert!(assembler.open.is_empty());
    }

    #[test]
    fn unfinished_responses_are_handed_on_oldest_first_not_in_the_order_of_run_names() {
        let mut assembler = DeferredPartAssembler::new();
        for run in ["c-run", "a-run", "b-run"] {
            assert_eq!(
                assembler.accept(&part(run, "r", "x".into(), 0, 2)),
                Accepted::Nothing
            );
        }
        // A newer response of the oldest run makes it the newest.
        assert_eq!(
            assembler.accept(&part("c-run", "r2", "y".into(), 0, 2)),
            Accepted::Nothing
        );

        let runs: Vec<String> = assembler
            .take_open()
            .into_iter()
            .map(|(run, ..)| run)
            .collect();

        assert_eq!(runs, ["a-run", "b-run", "c-run"]);
    }

    #[test]
    fn a_run_s_other_event_closes_its_unfinished_response() {
        let mut assembler = DeferredPartAssembler::new();
        assert_eq!(
            assembler.accept(&part("a", "r", "x".into(), 0, 2)),
            Accepted::Nothing
        );
        let boundary = DeferredEvent {
            id: 9,
            event: Event::new(MessageEvent::StreamRetried {
                run_id: Some("a".into()),
            }),
            body: None,
            first_id: None,
        };
        assert!(matches!(assembler.accept(&boundary), Accepted::Event(_)));
        assert!(assembler.open.is_empty());
    }
}
