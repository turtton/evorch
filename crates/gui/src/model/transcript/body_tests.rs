//! Text that stays in the store: how streamed pieces, completions and gaps
//! combine in a transcript without any of it being read.

use event_bus::{Event, MessageEvent};

use super::{Fill, TranscriptEntry, TranscriptModel};
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
    model.push_unreadable(8, Some("run"), FailureKind::Unreadable, "damaged", None);
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
    model.push_unreadable(8, Some("run"), FailureKind::Unreadable, "damaged", None);
    assert_eq!(model.entries().len(), 3);
}

fn streamed(model: &mut TranscriptModel, id: i64, event: &Event) {
    model.set_source(Some((id, id)));
    model.apply(event);
    model.set_source(None);
}

fn shown(model: &TranscriptModel) -> Vec<String> {
    model
        .entries()
        .iter()
        .map(|entry| match entry {
            TranscriptEntry::Message { text, .. } => format!("message {}", text.inline_or_empty()),
            TranscriptEntry::Notice { text } => format!("notice {text}"),
            TranscriptEntry::Unreadable { event_id, .. } => format!("gap {event_id}"),
            _ => "other".into(),
        })
        .collect()
}

#[test]
fn a_gap_in_the_middle_of_a_stream_is_replaced_by_the_whole_stream_and_nothing_is_shown_twice() {
    // Shown: the slices before and after the unreadable one, as separate entries.
    let mut model = TranscriptModel::new();
    model.push_message("one");
    streamed(&mut model, 7, &delta("before "));
    model.push_unreadable(8, Some("run"), FailureKind::Unreadable, "damaged", None);
    streamed(&mut model, 9, &delta("after"));
    model.push_message("last");
    assert_eq!(
        shown(&model),
        [
            "message one",
            "message before ",
            "gap 8",
            "message after",
            "message last"
        ]
    );
    let ids: Vec<_> = model.visible_entry_ids().collect();
    // Replayed with the slice readable, the stream is one entry.
    let mut replayed = TranscriptModel::new();
    for (id, text) in [(7, "before "), (8, "the middle "), (9, "after")] {
        streamed(&mut replayed, id, &delta(text));
    }
    assert_eq!(shown(&replayed), ["message before the middle after"]);

    assert_eq!(
        model.fill_unreadable_from(8, &replayed, i64::MAX, true),
        Fill::Filled
    );

    assert_eq!(
        shown(&model),
        [
            "message one",
            "message before the middle after",
            "message last"
        ],
        "the whole stream stands where its first slice was, and neither half is left over"
    );
    let after: Vec<_> = model.visible_entry_ids().collect();
    assert_eq!(after[0], ids[0], "what is before keeps its id");
    assert_eq!(
        after[1], ids[1],
        "the stream keeps the id of its first slice"
    );
    assert_eq!(after[2], ids[4], "what follows keeps its id");
    for id in &after {
        assert!(model.entry_revision(*id).is_some(), "id {id}");
    }
}

#[test]
fn a_completion_that_replaces_what_was_streamed_takes_its_place_not_a_second_one() {
    let mut model = TranscriptModel::new();
    streamed(&mut model, 3, &delta("partial "));
    model.push_unreadable(4, Some("run"), FailureKind::Unreadable, "damaged", None);
    model.push_message("later notice");
    assert_eq!(
        shown(&model),
        ["message partial ", "gap 4", "message later notice"]
    );
    let mut replayed = TranscriptModel::new();
    streamed(&mut replayed, 3, &delta("partial "));
    streamed(&mut replayed, 4, &completed("the full answer"));
    assert_eq!(shown(&replayed), ["message the full answer"]);

    assert_eq!(
        model.fill_unreadable_from(4, &replayed, i64::MAX, true),
        Fill::Filled
    );

    assert_eq!(
        shown(&model),
        ["message the full answer", "message later notice"]
    );
}

#[test]
fn an_entry_made_from_other_events_is_not_taken_for_the_gaps_stream() {
    // A separate message of the same run, made by an event outside the gap's span.
    let mut model = TranscriptModel::new();
    streamed(&mut model, 1, &completed("an earlier answer"));
    model.push_unreadable(5, Some("run"), FailureKind::Unreadable, "damaged", None);
    streamed(&mut model, 9, &completed("a later answer"));
    let mut replayed = TranscriptModel::new();
    streamed(&mut replayed, 1, &completed("an earlier answer"));
    streamed(&mut replayed, 5, &completed("the middle answer"));
    streamed(&mut replayed, 9, &completed("a later answer"));

    assert_eq!(
        model.fill_unreadable_from(5, &replayed, i64::MAX, true),
        Fill::Filled
    );

    assert_eq!(
        shown(&model),
        [
            "message an earlier answer",
            "message the middle answer",
            "message a later answer"
        ]
    );
}

#[test]
fn a_gap_whose_event_made_nothing_closes_without_a_trace() {
    let mut model = TranscriptModel::new();
    model.push_message("one");
    model.push_unreadable(6, None, FailureKind::Unreadable, "damaged", None);
    assert_eq!(
        model.fill_unreadable_from(6, &TranscriptModel::new(), i64::MAX, true),
        Fill::Filled
    );
    assert_eq!(
        model.fill_unreadable_from(6, &TranscriptModel::new(), i64::MAX, true),
        Fill::Absent
    );
    assert_eq!(shown(&model), ["message one"]);
}

#[test]
fn rows_older_than_what_they_would_replace_change_nothing() {
    let mut model = TranscriptModel::new();
    streamed(&mut model, 2, &delta("A "));
    model.push_unreadable(3, Some("run"), FailureKind::Unreadable, "damaged", None);
    streamed(&mut model, 4, &delta("C "));
    // A later delta (event 6) extended what the rows (read through event 5) replace.
    streamed(&mut model, 6, &delta("D"));
    let before = shown(&model);
    let mut replayed = TranscriptModel::new();
    for (id, text) in [(2, "A "), (3, "B "), (4, "C ")] {
        streamed(&mut replayed, id, &delta(text));
    }

    assert_eq!(
        model.fill_unreadable_from(3, &replayed, 5, true),
        Fill::Stale
    );
    assert_eq!(shown(&model), before, "not a thing was replaced");
    // The same rows read up to the later event are not stale.
    streamed(&mut replayed, 6, &delta("D"));
    assert_eq!(
        model.fill_unreadable_from(3, &replayed, 6, false),
        Fill::Filled,
        "a dry run says so without touching anything"
    );
    assert_eq!(shown(&model), before);
    assert_eq!(
        model.fill_unreadable_from(3, &replayed, 6, true),
        Fill::Filled
    );
    assert_eq!(shown(&model), ["message A B C D"]);
}
