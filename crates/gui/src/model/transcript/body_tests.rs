//! Text that stays in the store: how streamed pieces, completions and gaps
//! combine in a transcript without any of it being read.

use event_bus::{Event, MessageEvent};

use super::{TranscriptEntry, TranscriptModel};
use crate::body::{BodyRef, BodyText, DeferredBody, FailureKind, Segment};

fn stored(byte: u8, len: u64) -> BodyText {
    BodyText::deferred(DeferredBody::new(
        BodyRef {
            hash: [byte; 32],
            byte_len: len,
        },
        Some("run"),
        Some(i64::from(byte)),
    ))
}

fn delta(text: &str) -> Event {
    Event::new(MessageEvent::MessageDelta {
        run_id: Some("run".into()),
        delta: text.into(),
    })
}

fn empty_delta() -> Event {
    delta("")
}

fn completed(text: &str) -> Event {
    Event::new(MessageEvent::MessageCompleted {
        run_id: "run".into(),
        text: text.into(),
    })
}

fn messages(model: &TranscriptModel) -> Vec<&BodyText> {
    model
        .entries()
        .iter()
        .filter_map(|entry| match entry {
            TranscriptEntry::Message { text, .. } => Some(text),
            _ => None,
        })
        .collect()
}

#[test]
fn streamed_pieces_in_the_store_and_inline_form_one_message_in_order() {
    let mut model = TranscriptModel::new();
    model.apply(&delta("Intro "));
    model.apply_with_body(&empty_delta(), Some(stored(1, 5_000)));
    model.apply(&delta(" outro"));

    let [text] = messages(&model)[..] else {
        panic!("one message")
    };
    let segments = text.segments();
    assert!(matches!(&segments[0], Segment::Inline(inline) if inline == "Intro "));
    assert!(matches!(&segments[1], Segment::Deferred(body) if body.key.byte_len == 5_000));
    assert!(matches!(&segments[2], Segment::Inline(inline) if inline == " outro"));
    assert_eq!(text.byte_len(), 6 + 5_000 + 6);
}

#[test]
fn a_completion_in_the_store_replaces_what_was_streamed() {
    let mut model = TranscriptModel::new();
    model.apply(&delta("partial"));
    let event = completed("");
    model.apply_with_body(&event, Some(stored(2, 9_000)));

    let [text] = messages(&model)[..] else {
        panic!("one message")
    };
    assert_eq!(text, &stored(2, 9_000));
    // Applying the very same event again (a second delivery) changes nothing.
    model.apply_with_body(&event, Some(stored(2, 9_000)));
    assert_eq!(messages(&model).len(), 1);
}

#[test]
fn a_body_given_for_one_event_is_not_used_for_the_next() {
    let mut model = TranscriptModel::new();
    model.apply_with_body(&completed(""), Some(stored(3, 4_000)));
    model.apply(&Event::new(MessageEvent::MessageCompleted {
        run_id: "other".into(),
        text: "plain".into(),
    }));

    let all = messages(&model);
    assert_eq!(all.len(), 2);
    assert!(all[0].has_deferred());
    assert_eq!(all[1], &"plain");
}

#[test]
fn a_gap_ends_the_message_being_streamed_so_the_stream_cannot_continue_across_it() {
    let mut model = TranscriptModel::new();
    model.apply(&delta("before "));
    model.push_unreadable(8, Some("run"), FailureKind::Unreadable, "damaged");
    model.apply(&delta("after"));

    let kinds: Vec<_> = model
        .entries()
        .iter()
        .map(|entry| match entry {
            TranscriptEntry::Message { text, .. } => text.inline_or_empty().to_owned(),
            TranscriptEntry::Unreadable { event_id, .. } => format!("gap {event_id}"),
            _ => "other".into(),
        })
        .collect();
    assert_eq!(kinds, ["before ", "gap 8", "after"]);
    // Reporting the same event again adds no second gap.
    model.push_unreadable(8, Some("run"), FailureKind::Unreadable, "damaged");
    assert_eq!(model.entries().len(), 3);
}

#[test]
fn entries_that_replace_a_gap_keep_stable_ids_and_the_rest_of_the_transcript_still_updates() {
    let mut model = TranscriptModel::new();
    model.push_message("one");
    model.push_unreadable(5, Some("run"), FailureKind::Unreadable, "damaged");
    model.push_message("three");
    let ids: Vec<_> = model.visible_entry_ids().collect();

    assert!(model.replace_unreadable(
        5,
        vec![
            TranscriptEntry::Notice { text: "a".into() },
            TranscriptEntry::Notice { text: "b".into() },
        ],
    ));

    let after: Vec<_> = model.visible_entry_ids().collect();
    assert_eq!(after.len(), 4);
    assert_eq!(after[0], ids[0]);
    assert_eq!(after[3], ids[2], "what followed keeps its id");
    // Every id still finds its entry and its revision.
    for id in &after {
        assert!(model.entry_revision(*id).is_some(), "id {id}");
    }
    // A result for an entry after the gap still reaches that entry.
    model.apply(&delta("more"));
    assert_eq!(model.entries().len(), 5);
    // Closing a gap with nothing leaves no trace.
    model.push_unreadable(6, None, FailureKind::Unreadable, "damaged");
    assert!(model.replace_unreadable(6, Vec::new()));
    assert!(!model.replace_unreadable(6, Vec::new()));
    assert_eq!(model.entries().len(), 5);
}
