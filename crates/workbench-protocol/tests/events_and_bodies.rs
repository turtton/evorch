//! Event ordering around the snapshot boundary, and body paging.

mod common;

use common::*;
use workbench_protocol::events::*;
use workbench_protocol::ids::BodyId;
use workbench_protocol::validation::ValidationError;
use workbench_protocol::views::*;
use workbench_protocol::wire::*;

#[test]
fn events_at_or_before_the_snapshot_boundary_are_duplicates() {
    let mut tracker = CursorTracker::new(cursor(7, 40));
    assert_eq!(tracker.observe(cursor(7, 39)), CursorDecision::Duplicate);
    assert_eq!(tracker.observe(cursor(7, 40)), CursorDecision::Duplicate);
    assert_eq!(tracker.observe(cursor(7, 41)), CursorDecision::Deliver);
    assert_eq!(tracker.last(), cursor(7, 41));
}

#[test]
fn repeated_and_reordered_events_are_not_delivered_twice() {
    let mut tracker = CursorTracker::new(cursor(7, 0));
    assert_eq!(tracker.observe(cursor(7, 5)), CursorDecision::Deliver);
    assert_eq!(tracker.observe(cursor(7, 5)), CursorDecision::Duplicate);
    assert_eq!(tracker.observe(cursor(7, 3)), CursorDecision::Duplicate);
    // Scoped subscriptions see gaps in the core-wide sequence; a gap is not loss.
    assert_eq!(tracker.observe(cursor(7, 900)), CursorDecision::Deliver);
}

#[test]
fn another_epoch_never_delivers_and_does_not_move_the_cursor() {
    let mut tracker = CursorTracker::new(cursor(7, 10));
    assert_eq!(tracker.observe(cursor(8, 1)), CursorDecision::EpochMismatch);
    assert_eq!(
        tracker.observe(cursor(6, 99)),
        CursorDecision::EpochMismatch
    );
    assert_eq!(tracker.last(), cursor(7, 10));
}

#[test]
fn only_resync_and_close_end_a_subscription() {
    let ending: Vec<bool> = ui_events().iter().map(UiEvent::ends_subscription).collect();
    let expected: Vec<bool> = ui_events()
        .iter()
        .map(|event| {
            let wire = serde_json::to_value(event).unwrap();
            matches!(
                wire["payload"]["type"].as_str(),
                Some("resync_required" | "subscription_closed")
            )
        })
        .collect();
    assert_eq!(ending, expected);
}

const TEXT: &str = "aé日🙂z"; // 1 + 2 + 3 + 4 + 1 = 11 bytes

fn body() -> BodyId {
    BodyId::new("body-1").unwrap()
}

fn slice(offset: u64, max: u32) -> Result<BodyChunk, ValidationError> {
    BodyChunk::slice(body(), TEXT, ByteOffset(offset), max)
}

#[test]
fn chunks_back_off_to_character_boundaries_and_reassemble() {
    let mut out = String::new();
    let mut offset = 0;
    let mut chunks = 0;
    loop {
        let chunk = slice(offset, 4).unwrap();
        chunk.validate(4).unwrap();
        assert!(chunk.text.len() <= 4);
        assert_eq!(chunk.total_bytes, ByteOffset(11));
        out.push_str(&chunk.text);
        chunks += 1;
        match chunk.next_offset {
            Some(next) => {
                assert_eq!(next.get(), offset + chunk.text.len() as u64);
                offset = next.get();
            }
            None => break,
        }
    }
    assert_eq!(out, TEXT);
    assert_eq!(chunks, 4); // "aé" | "日" | "🙂" | "z"
}

#[test]
fn chunk_boundaries_at_exact_size_end_and_empty_body() {
    let whole = slice(0, 11).unwrap();
    assert_eq!(whole.text, TEXT);
    assert_eq!(whole.next_offset, None);
    whole.validate(11).unwrap();

    let last = slice(10, 4).unwrap();
    assert_eq!(last.text, "z");
    assert_eq!(last.next_offset, None);

    let at_end = slice(11, 4).unwrap();
    assert_eq!(at_end.text, "");
    assert_eq!(at_end.next_offset, None);

    let empty = BodyChunk::slice(body(), "", ByteOffset(0), 4).unwrap();
    assert_eq!(empty.total_bytes, ByteOffset(0));
    assert_eq!(empty.next_offset, None);
}

#[test]
fn bad_offsets_and_tiny_chunks_are_refused() {
    assert!(slice(12, 4).is_err(), "past the end");
    assert!(slice(2, 4).is_err(), "inside 'é'");
    assert!(slice(u64::MAX, 4).is_err());
    assert!(slice(0, 3).is_err(), "cannot hold a 4-byte scalar");
}

#[test]
fn chunk_validation_catches_inconsistent_chunks() {
    let good = slice(0, 4).unwrap();
    let oversize = BodyChunk {
        text: "x".repeat(5),
        ..good.clone()
    };
    assert!(oversize.validate(4).is_err());
    let past_end = BodyChunk {
        total_bytes: ByteOffset(1),
        ..good.clone()
    };
    assert!(past_end.validate(4).is_err());
    let wrong_next = BodyChunk {
        next_offset: Some(ByteOffset(99)),
        ..good.clone()
    };
    assert!(wrong_next.validate(4).is_err());
    let missing_next = BodyChunk {
        next_offset: None,
        ..good
    };
    assert!(missing_next.validate(4).is_err());
}

#[test]
fn pages_larger_than_the_protocol_allows_are_refused() {
    let page = |n: usize| Page {
        items: vec![0u8; n],
        next_cursor: None,
        snapshot_revision: Revision(1),
    };
    page(200).validate().unwrap();
    assert!(page(201).validate().is_err());
}

#[test]
fn inline_messages_are_bounded_and_may_carry_terminal_output() {
    let inline = |text: String| message(MessageContent::Inline { text });
    inline("ok \u{1b}[31mred\u{1b}[0m".into())
        .validate()
        .unwrap();
    assert!(inline("x".repeat(16 * 1024 + 1)).validate().is_err());
    message(MessageContent::Body(body_ref()))
        .validate()
        .unwrap();
}

#[test]
fn question_views_validate_their_provenance() {
    question(QuestionTarget::User).validate().unwrap();
    let mut incoherent = question(QuestionTarget::User);
    incoherent.raised_by.agent_run_id = None;
    assert!(incoherent.validate().is_err());
    let mut many = question(QuestionTarget::User);
    many.options = vec!["o".into(); 33];
    assert!(many.validate().is_err());
}

#[test]
fn a_non_final_chunk_must_carry_text_and_offsets_must_not_overflow() {
    let empty_but_not_last = BodyChunk {
        body_id: body(),
        offset: ByteOffset(4),
        text: String::new(),
        total_bytes: ByteOffset(11),
        next_offset: Some(ByteOffset(4)),
    };
    assert!(empty_but_not_last.validate(64).is_err());

    // The sum overflows u64; it must be an error, not a wrapped or saturated pass.
    let overflowing = BodyChunk {
        body_id: body(),
        offset: ByteOffset(u64::MAX),
        text: "x".into(),
        total_bytes: ByteOffset(u64::MAX),
        next_offset: None,
    };
    assert!(overflowing.validate(64).is_err());
}
