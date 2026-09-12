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

#[test]
fn retries_exhausted_keeps_partial_display_with_one_terminal_error() {
    // Given: 再試行を使い切ったプロバイダエラーと部分表示がある。
    let reason = providers::ProviderError::RetriesExhausted {
        attempts: 3,
        last: Box::new(providers::ProviderError::Transport {
            message: "connection lost".into(),
        }),
    }
    .to_string();
    let mut registry = TranscriptRegistry::new();
    registry.apply(&reasoning("partial thought"));
    registry.apply(&message("partial answer"));

    // When: 実際のDisplayによる理由を持つError終端を受信する。
    registry.apply(&Event::new(LifecycleEvent::AgentRunStateChanged {
        run_id: "stream".into(),
        from: AgentRunPhase::Running,
        to: AgentRunPhase::Error,
        reason: Some(reason),
    }));

    // Then: 部分表示を保持し、終端エラーは1件だけで両transcriptが一致する。
    let entries = registry.thread().entries();
    assert!(matches!(entries, [
        TranscriptEntry::Reasoning { text: thought },
        TranscriptEntry::Message { text: answer },
        TranscriptEntry::Error { text },
    ] if thought == "partial thought" && answer == "partial answer"
        && text.contains("Run failed") && text.contains("3 attempts")));
    assert_eq!(registry.run("stream").expect("run").entries(), entries);
}

#[test]
fn transport_attempt_failure_keeps_partial_display_with_retrying_notice() {
    // Given: ストリームの部分表示を持つrunがある。
    let mut registry = TranscriptRegistry::new();
    registry.apply(&message("partial"));

    // When: 終端ではなく試行単位のTransport失敗を受信する。
    registry.apply(&Event::new(event_bus::ProviderEvent::RequestFailed {
        request_id: "request-1".into(),
        provider: "openai".into(),
        profile: None,
        protocol: "openai-chat-completions".into(),
        model: "test".into(),
        streaming: true,
        duration_ms: 42,
        failure: event_bus::ProviderFailureKind::Transport,
        run_id: Some("stream".into()),
    }));

    // Then: retrying Noticeが部分表示に続き、両transcriptが一致する。
    let entries = registry.thread().entries();
    assert!(matches!(entries, [
        TranscriptEntry::Message { text: partial },
        TranscriptEntry::Notice { text },
    ] if partial == "partial" && text.contains("transport error")
        && text.contains(", retrying")));
    assert_eq!(registry.run("stream").expect("run").entries(), entries);
}
