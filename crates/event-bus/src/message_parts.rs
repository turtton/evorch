//! Rebuilds responses the storage bridge had to store in several rows.

use std::{borrow::Cow, collections::BTreeMap};

use crate::event::{Event, EventKind, MessageEvent};

/// Turns stored [`MessageEvent::MessagePart`] sequences back into the
/// `MessageCompleted` event the run originally emitted.
///
/// Feed every stored event in order. Slices of different runs may interleave.
/// A response whose remaining slices were never written (the process stopped
/// mid-write) yields nothing: only a complete response is ever reported.
#[derive(Debug, Default)]
pub struct MessagePartAssembler {
    open: BTreeMap<String, (usize, String)>,
}

impl MessagePartAssembler {
    pub fn new() -> Self {
        Self::default()
    }

    /// The event readers should handle in place of `event`: `event` itself
    /// unless it is a slice, nothing while a response is incomplete, and the
    /// rebuilt `MessageCompleted` (with the final slice's metadata) once the
    /// last slice arrives.
    pub fn accept<'a>(&mut self, event: &'a Event) -> Option<Cow<'a, Event>> {
        let EventKind::Message(MessageEvent::MessagePart {
            run_id,
            text,
            index,
            last,
        }) = &event.kind
        else {
            if let EventKind::Message(MessageEvent::MessageCompleted { run_id, .. }) = &event.kind {
                self.open.remove(run_id);
            }
            return Some(Cow::Borrowed(event));
        };
        if *index == 0 {
            self.open.insert(run_id.clone(), (0, String::new()));
        }
        let (next, buffer) = self.open.get_mut(run_id)?;
        if next != index {
            self.open.remove(run_id);
            return None;
        }
        buffer.push_str(text);
        *next += 1;
        if !*last {
            return None;
        }
        let (_, text) = self.open.remove(run_id)?;
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

    fn part(run: &str, text: &str, index: usize, last: bool) -> Event {
        Event::new(MessageEvent::MessagePart {
            run_id: run.into(),
            text: text.into(),
            index,
            last,
        })
    }

    #[test]
    fn interleaved_runs_rebuild_their_own_completion_and_others_pass_through() {
        let mut assembler = MessagePartAssembler::new();
        let other = Event::new(MessageEvent::StreamRetried { run_id: None });
        assert_eq!(assembler.accept(&other).as_deref(), Some(&other));
        assert!(assembler.accept(&part("a", "one ", 0, false)).is_none());
        assert!(assembler.accept(&part("b", "x", 0, false)).is_none());
        let last_a = part("a", "two", 1, true);
        let done = assembler.accept(&last_a).unwrap().into_owned();
        assert_eq!(done.meta, last_a.meta);
        assert_eq!(
            done.kind,
            EventKind::Message(MessageEvent::MessageCompleted {
                run_id: "a".into(),
                text: "one two".into()
            })
        );
        let done_b = assembler
            .accept(&part("b", "y", 1, true))
            .unwrap()
            .into_owned();
        assert!(
            matches!(done_b.kind, EventKind::Message(MessageEvent::MessageCompleted { text, .. }) if text == "xy")
        );
    }

    #[test]
    fn missing_slices_never_publish_or_contaminate_a_later_response() {
        let mut assembler = MessagePartAssembler::new();
        assert!(assembler.accept(&part("a", "stale", 0, false)).is_none());
        assert!(
            assembler
                .accept(&part("a", "missing middle", 2, true))
                .is_none()
        );
        assert!(assembler.accept(&part("a", "no start", 1, true)).is_none());
        assert!(
            assembler
                .accept(&part("a", "incomplete", 0, false))
                .is_none()
        );
        let next = part("a", "new", 0, true);
        let done = assembler.accept(&next).unwrap();
        assert!(matches!(&done.kind,
            EventKind::Message(MessageEvent::MessageCompleted { text, .. }) if text == "new"));
    }
}
