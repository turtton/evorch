use super::*;
use event_bus::{EventKind, MessageEvent};
#[test]
fn summary_preview_is_bounded_coalesced_and_utf8_safe() {
    let bus = Arc::new(EventBus::new(2048));
    let mut receiver = bus.subscribe();
    let mut operation = Operation::new(bus, "run".into());
    let mut stream = operation.path(CompactionMode::ModelSummary);
    for _ in 0..100_000 {
        stream.text("日本語");
    }
    stream.flush_pending();
    let mut total = 0;
    let mut count = 0;
    for event in receiver.drain_pending_snapshot() {
        let json = serde_json::to_string(&event).unwrap();
        let wire: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(wire["kind"]["payload"]["kind"], event.kind.subkind_name());
        assert_eq!(serde_json::from_str::<Event>(&json).unwrap(), event);
        assert_eq!(event.primary_run_id(), Some("run"));
        let EventKind::Compaction(CompactionEvent::Progress {
            delta, revision, ..
        }) = event.kind
        else {
            panic!("not progress");
        };
        assert!(delta.len() <= COMPACTION_DELTA_MAX_BYTES);
        count += 1;
        assert_eq!(revision, count);
        total += delta.len();
    }
    assert!(total <= 64 * 1024);
    assert!(count <= 18, "tiny deltas must be coalesced");
}
#[test]
fn only_text_is_observed_and_retry_discards_unflushed_text() {
    let bus = Arc::new(EventBus::new(32));
    let mut receiver = bus.subscribe();
    let mut operation = Operation::new(bus, "run".into());
    let mut stream = operation.path(CompactionMode::ModelSummary);
    stream.event(&Event::new(MessageEvent::ReasoningDelta {
        run_id: Some("run".into()),
        delta: "hidden reasoning".into(),
    }));
    stream.text("discarded");
    stream.retry();
    stream.text("accepted");
    stream.flush_pending();
    drop(stream);
    operation.path(CompactionMode::Structural);
    let mut attempts = Vec::new();
    let mut text = String::new();
    for event in receiver.drain_pending_snapshot() {
        let EventKind::Compaction(CompactionEvent::Progress { delta, attempt, .. }) = event.kind
        else {
            panic!("not progress");
        };
        text.push_str(&delta);
        attempts.push(attempt);
    }
    assert_eq!(text, "accepted");
    assert_eq!(attempts, [1, 2, 2, 3]);
}
#[test]
fn dropped_operation_aborts_but_commit_does_not_publish_a_second_success() {
    let bus = Arc::new(EventBus::new(32));
    let mut receiver = bus.subscribe();
    let mut operation = Operation::new(bus.clone(), "run".into());
    operation.started = true;
    let id = operation.id.clone();
    drop(operation);
    assert!(
        matches!(receiver.drain_pending_snapshot().pop().unwrap().kind, EventKind::Compaction(CompactionEvent::Aborted { operation_id, .. }) if operation_id == id)
    );
    let mut operation = Operation::new(bus, "run".into());
    operation.started = true;
    operation.committed();
    drop(operation);
    assert!(
        receiver.drain_pending_snapshot().is_empty(),
        "Compacted remains the sole success event"
    );
}
#[test]
fn terminal_cleanup_survives_revocation_but_text_is_fenced_and_events_roundtrip() {
    let bus = Arc::new(EventBus::new(32));
    let mut receiver = bus.subscribe();
    bus.register_mutation_fence("run".into(), Arc::new(|| false));
    let mut operation = Operation::new(bus.clone(), "run".into());
    operation.started = true;
    operation
        .path(CompactionMode::ModelSummary)
        .text("rejected");
    drop(operation);
    let event = receiver.drain_pending_snapshot().pop().unwrap();
    assert_eq!(event.kind.primary_run_id(), Some("run"));
    assert!(matches!(
        event.kind,
        EventKind::Compaction(CompactionEvent::Aborted { .. })
    ));
    let json = serde_json::to_string(&event).unwrap();
    assert_eq!(serde_json::from_str::<Event>(&json).unwrap(), event);
    assert!(receiver.drain_pending_snapshot().is_empty());
}

#[tokio::test]
async fn observing_progress_does_not_change_summary_requests_or_results() {
    use crate::{AgentInvocationContext, AgentModel, RuntimeError};
    use agents::Role;
    use providers::{ChatResponse, ContentBlock, Message, ToolSpec};
    #[derive(Default)]
    struct Capture(std::sync::Mutex<Vec<Vec<Message>>>);
    #[async_trait::async_trait]
    impl AgentModel for Capture {
        fn selected_model(&self, _: Role, _: Option<&str>) -> String {
            "summary".into()
        }
        async fn complete(
            &self,
            _: &AgentInvocationContext,
            _: Role,
            messages: &[Message],
            _: &[ToolSpec],
        ) -> Result<ChatResponse, RuntimeError> {
            self.0.lock().unwrap().push(messages.to_vec());
            Ok(ChatResponse {
                message: Message {
                    role: providers::Role::Assistant,
                    content: vec![ContentBlock::Text {
                        text: "accepted summary".into(),
                    }],
                },
                usage: Default::default(),
                usage_reported: true,
                finish_reason: providers::FinishReason::Stop,
            })
        }
        async fn complete_streaming(
            &self,
            invocation: &AgentInvocationContext,
            role: Role,
            messages: &[Message],
            tools: &[ToolSpec],
            bus: &EventBus,
        ) -> Result<ChatResponse, RuntimeError> {
            bus.emit(Event::new(MessageEvent::MessageDelta {
                run_id: Some(invocation.run_id.clone()),
                delta: "failed partial".into(),
            }));
            bus.emit(Event::new(MessageEvent::StreamRetried {
                run_id: Some(invocation.run_id.clone()),
            }));
            bus.emit(Event::new(MessageEvent::MessageDelta {
                run_id: Some(invocation.run_id.clone()),
                delta: "accepted summary".into(),
            }));
            self.complete(invocation, role, messages, tools).await
        }
    }
    let capture = Arc::new(Capture::default());
    let summarizer = super::super::summary::ModelSummarizer {
        turn: None,
        model: capture.clone(),
        role: Role::Worker,
        run_id: "run".into(),
        category: None,
        model_preference: None,
        idle_timeout: std::time::Duration::from_secs(60),
        timeout: std::time::Duration::from_secs(600),
    };
    let input = super::super::summary::SummaryInput {
        goal: Some("goal"),
        compacted: &[],
    };
    let bus = Arc::new(EventBus::new(32));
    let mut events = bus.subscribe();
    let mut operation = Operation::new(bus, "run".into());
    let mut progress = operation.path(CompactionMode::ModelSummary);
    let observed = summarizer
        .summarize_with_progress(&input, false, Some(&mut progress))
        .await
        .0
        .unwrap();
    let unobserved = summarizer.summarize_with_usage(&input).await.0.unwrap();
    assert_eq!(observed, unobserved);
    let requests = capture.0.lock().unwrap();
    assert_eq!(requests[0], requests[1]);
    let text: String = events
        .drain_pending_snapshot()
        .into_iter()
        .filter_map(|event| match event.kind {
            EventKind::Compaction(CompactionEvent::Progress { delta, .. }) => Some(delta),
            _ => None,
        })
        .collect();
    assert_eq!(text, "accepted summary");
}

#[test]
fn timed_flushes_cannot_flood_the_main_bus_with_tiny_fragments() {
    let bus = Arc::new(EventBus::new(2048));
    let mut events = bus.subscribe();
    let mut operation = Operation::new(bus, "run".into());
    let mut stream = operation.path(CompactionMode::ModelSummary);
    for _ in 0..10_000 {
        stream.text("x");
        stream.flush_pending();
    }
    assert_eq!(events.drain_pending_snapshot().len(), 256);
    stream.retry();
    for _ in 0..254 {
        stream.text("x");
        stream.flush_pending();
    }
    // One bulk fragment at the last revision must not bypass the event cap
    // inside the byte-chunking loop (or strand a non-draining while loop).
    stream.text(&"日本語".repeat(6_000));
    stream.flush_pending();
    let tail = events.drain_pending_snapshot();
    assert_eq!(tail.len(), 256);
    assert!(tail.iter().all(|event| matches!(&event.kind, EventKind::Compaction(CompactionEvent::Progress { attempt: 2, revision: 1..=256, delta, .. }) if delta.len() <= COMPACTION_DELTA_MAX_BYTES)));
}
