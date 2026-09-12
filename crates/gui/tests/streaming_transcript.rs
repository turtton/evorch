use event_bus::{AgentRunPhase, Event, LifecycleEvent, MessageEvent};
use gui::model::transcript::{TranscriptEntry, TranscriptModel};
use gui::model::transcript_registry::TranscriptRegistry;

fn message(delta: &str) -> Event {
    Event::new(MessageEvent::MessageDelta {
        delta: delta.into(),
        run_id: Some("stream".into()),
    })
}

fn reasoning(delta: &str) -> Event {
    Event::new(MessageEvent::ReasoningDelta {
        delta: delta.into(),
        run_id: Some("stream".into()),
    })
}

#[test]
fn interleaved_text_reasoning_deltas_render_in_order() {
    // Given: arrival order differs from canonical reasoning-first grouping.
    let mut registry = TranscriptRegistry::new();
    let events = [
        reasoning(""),
        message("First"),
        message(" "),
        reasoning(""),
        reasoning("think"),
        message(""),
        reasoning(" more"),
        message("second"),
        reasoning(""),
        message("."),
    ];

    // When: real registry routing consumes each live delta.
    for event in events {
        registry.apply(&event);
    }

    // Then: both destinations preserve exactly the nonempty arrival blocks.
    let expected = [
        TranscriptEntry::Message {
            text: "First ".into(),
        },
        TranscriptEntry::Reasoning {
            text: "think more".into(),
        },
        TranscriptEntry::Message {
            text: "second.".into(),
        },
    ];
    assert_eq!(registry.thread().visible_entries(), expected);
    assert_eq!(
        registry.run("stream").expect("run").visible_entries(),
        expected
    );
}

#[test]
fn empty_delta_is_noop_before_kind_switch() {
    // Given: one reasoning block occupies the entire bounded transcript.
    let mut transcript = TranscriptModel::with_capacity(1);
    transcript.apply(&reasoning("keep"));

    // When: an empty message arrives between reasoning fragments.
    transcript.apply(&message(""));
    transcript.apply(&reasoning(" thinking"));

    // Then: the empty kind switch neither splits nor evicts the block.
    assert_eq!(
        transcript.entries(),
        [TranscriptEntry::Reasoning {
            text: "keep thinking".into(),
        }]
    );
}

#[test]
fn delta_stream_final_state_matches_snapshot() {
    // Given: an independently specified canonical visible message snapshot.
    let canonical_message = "Hello, 世界!\n";
    let mut snapshot = TranscriptModel::new();
    snapshot.push_message(canonical_message);
    let mut registry = TranscriptRegistry::new();

    // When: the complete stream arrives without replaying the final response.
    for event in [
        message(""),
        message("Hello"),
        message(", "),
        reasoning(""),
        message("世界"),
        message("!"),
        message("\n"),
        reasoning(""),
    ] {
        registry.apply(&event);
    }

    // Then: streamed visible content and block boundaries equal the snapshot.
    assert_eq!(
        registry.thread().visible_entries(),
        snapshot.visible_entries()
    );
    assert_eq!(
        registry.run("stream").expect("run").visible_entries(),
        snapshot.visible_entries()
    );
}

#[test]
fn empty_deltas_leave_a_fresh_transcript_empty() {
    // Given: no visible content yet.
    let mut transcript = TranscriptModel::new();

    // When: either kind emits an empty delta.
    for event in [message(""), reasoning("")] {
        transcript.apply(&event);
    }

    // Then: no placeholder blocks appear.
    assert!(transcript.entries().is_empty());
}

#[test]
fn failed_or_cancelled_stream_keeps_partial_display() {
    // Given: each terminal error interrupts partial reasoning and text.
    for (reason, terminal) in [
        (
            "cancelled",
            TranscriptEntry::Notice {
                text: "Run cancelled".into(),
            },
        ),
        (
            "connection lost",
            TranscriptEntry::Error {
                text: "Run failed: connection lost".into(),
            },
        ),
    ] {
        let mut registry = TranscriptRegistry::new();
        registry.apply(&reasoning("partial thought"));
        registry.apply(&message("partial answer"));

        // When: the run terminates without a committed final response.
        registry.apply(&Event::new(LifecycleEvent::AgentRunStateChanged {
            run_id: "stream".into(),
            from: AgentRunPhase::Running,
            to: AgentRunPhase::Error,
            reason: Some(reason.into()),
        }));

        // Then: partial arrival-ordered display survives with one terminal entry.
        let expected = [
            TranscriptEntry::Reasoning {
                text: "partial thought".into(),
            },
            TranscriptEntry::Message {
                text: "partial answer".into(),
            },
            terminal,
        ];
        assert_eq!(registry.thread().entries(), expected);
        assert_eq!(registry.run("stream").expect("run").entries(), expected);
    }
}
