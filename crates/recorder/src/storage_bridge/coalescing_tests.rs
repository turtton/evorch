use super::*;
use event_bus::{LifecycleEvent, ToolEvent};

fn reasoning(run: Option<&str>, text: &str) -> Event {
    Event::new(MessageEvent::ReasoningDelta {
        run_id: run.map(str::to_owned),
        delta: text.into(),
    })
}

fn message_delta(run: &str, text: &str) -> Event {
    Event::new(MessageEvent::MessageDelta {
        run_id: Some(run.into()),
        delta: text.into(),
    })
}

fn tool_started(run: &str) -> Event {
    Event::new(ToolEvent::ToolStarted {
        tool_name: "shell".into(),
        call_id: format!("{run}:1"),
        input: None,
        run_id: Some(run.into()),
        parent_call_id: None,
    })
}

fn queue_with_limit(
    max_event_bytes: u64,
) -> (
    EventQueue,
    mpsc::Receiver<WriteRequest>,
    StorageBridgeMonitor,
) {
    let (tx, rx) = mpsc::channel(WRITE_QUEUE_CAPACITY);
    let monitor = StorageBridgeMonitor::default();
    (
        EventQueue::new(
            tx,
            monitor.clone(),
            PersistencePolicy::default(),
            max_event_bytes,
        ),
        rx,
        monitor,
    )
}

fn queue() -> (
    EventQueue,
    mpsc::Receiver<WriteRequest>,
    StorageBridgeMonitor,
) {
    queue_with_limit(u64::MAX)
}

fn collect(rx: &mut mpsc::Receiver<WriteRequest>) -> Vec<Event> {
    let mut result = Vec::new();
    while let Ok(request) = rx.try_recv() {
        if let WriteRequest::Event(event) = request {
            result.push(*event.event);
        }
    }
    result
}

fn text(event: &Event) -> &str {
    match &event.kind {
        EventKind::Message(MessageEvent::ReasoningDelta { delta, .. }) => delta,
        other => panic!("expected reasoning delta: {other:?}"),
    }
}

#[tokio::test]
async fn one_runs_burst_merges_into_one_row_that_keeps_the_text() {
    let (mut queue, mut rx, monitor) = queue();
    let mut expected = String::new();
    for n in 0..512 {
        let part = format!("{n}:🦀\n");
        expected.push_str(&part);
        queue.push(reasoning(Some("run"), &part)).await.unwrap();
    }
    assert_eq!(monitor.snapshot().pending_events, 512);
    assert!(
        collect(&mut rx).is_empty(),
        "nothing is written before a boundary"
    );
    queue.flush().await.unwrap();
    let events = collect(&mut rx);
    assert_eq!(events.len(), 1);
    assert_eq!(text(&events[0]), expected);
    assert_eq!(monitor.snapshot().coalesced_events, 511);
    assert_eq!(monitor.snapshot().pending_events, 0);
    assert_eq!(monitor.snapshot().pending_bytes, 0);
    assert_eq!(monitor.snapshot().oldest_pending_age, Duration::ZERO);
}

#[tokio::test]
async fn interleaved_runs_merge_per_run_and_a_boundary_orders_only_its_own_run() {
    let (mut queue, mut rx, _) = queue();
    // Runs a and b alternate, so a single shared accumulator would never merge.
    queue.push(reasoning(Some("a"), "a1 ")).await.unwrap();
    queue.push(reasoning(Some("b"), "b1 ")).await.unwrap();
    queue.push(reasoning(Some("a"), "a2 ")).await.unwrap();
    queue.push(reasoning(Some("b"), "b2 ")).await.unwrap();
    assert!(collect(&mut rx).is_empty());

    // b's tool start is a boundary for b only: b's reasoning precedes it and a
    // keeps accumulating.
    let started = tool_started("b");
    queue.push(started.clone()).await.unwrap();
    let written = collect(&mut rx);
    assert_eq!(written.len(), 2);
    assert_eq!(text(&written[0]), "b1 b2 ");
    assert_eq!(written[1], started);

    queue.push(reasoning(Some("a"), "a3")).await.unwrap();
    queue.push(reasoning(Some("b"), "b3")).await.unwrap();
    queue.flush().await.unwrap();
    let rest = collect(&mut rx);
    // Remaining rows are written in order of their first fragment.
    assert_eq!(
        rest.iter().map(text).collect::<Vec<_>>(),
        ["a1 a2 a3", "b3"]
    );
}

#[tokio::test]
async fn events_of_other_runs_and_unattributed_events_do_not_flush_a_runs_reasoning() {
    let (mut queue, mut rx, _) = queue();
    queue.push(reasoning(Some("a"), "kept")).await.unwrap();
    queue.push(tool_started("b")).await.unwrap();
    queue
        .push(Event::new(LifecycleEvent::Completed {
            session_id: "session".into(),
        }))
        .await
        .unwrap();
    assert_eq!(collect(&mut rx).len(), 2, "only the two other events");
    queue.flush().await.unwrap();
    assert_eq!(
        collect(&mut rx).iter().map(text).collect::<Vec<_>>(),
        ["kept"]
    );
}

#[tokio::test]
async fn unattributed_reasoning_is_written_as_it_arrives_and_never_merged() {
    let (mut queue, mut rx, _) = queue();
    let expected = [reasoning(None, "one"), reasoning(None, "two")];
    for event in &expected {
        queue.push(event.clone()).await.unwrap();
    }
    assert_eq!(collect(&mut rx), expected);
}

#[tokio::test]
async fn orchestrator_boundaries_flush_only_their_runs_reasoning_first() {
    use event_bus::OrchestratorEvent;
    let boundaries = [
        Event::new(OrchestratorEvent::TaskProgressed {
            task_id: "task".into(),
            run_id: "a".into(),
            progress: serde_json::json!({"done": 1}),
            reason: "progress".into(),
        }),
        Event::new(OrchestratorEvent::TaskCheckpoint {
            task_id: "task".into(),
            run_id: "a".into(),
            tool_call_count: 2,
            cumulative_input_tokens: 3,
            cumulative_output_tokens: 4,
            elapsed_ms: 5,
        }),
    ];
    let (mut queue, mut rx, _) = queue();
    queue.push(reasoning(Some("b"), "other")).await.unwrap();
    for boundary in boundaries {
        queue.push(reasoning(Some("a"), "before")).await.unwrap();
        queue.push(boundary.clone()).await.unwrap();
        let written = collect(&mut rx);
        assert_eq!(written.len(), 2);
        assert_eq!(text(&written[0]), "before");
        assert_eq!(written[1], boundary);
    }
    queue.flush().await.unwrap();
    assert_eq!(
        collect(&mut rx).iter().map(text).collect::<Vec<_>>(),
        ["other"]
    );
}

#[tokio::test]
async fn unattributed_approval_and_fault_keep_their_position_between_reasoning() {
    let boundaries = [
        Event::new(ToolEvent::ApprovalRequested {
            tool_name: "shell".into(),
            call_id: "unattributed".into(),
            input: None,
        }),
        Event::new(event_bus::FaultEvent::SkillDiagnostic {
            kind: event_bus::SkillDiagnosticKind::DiscoveryError,
            skill: "example".into(),
            scope: "project".into(),
            detail: "unavailable".into(),
        }),
    ];
    let (mut queue, mut rx, _) = queue();
    for boundary in boundaries {
        queue.push(reasoning(Some("a"), "first")).await.unwrap();
        queue.push(reasoning(Some("b"), "second")).await.unwrap();
        queue.push(boundary.clone()).await.unwrap();
        let written = collect(&mut rx);
        assert_eq!(written.len(), 3);
        assert_eq!(text(&written[0]), "first");
        assert_eq!(text(&written[1]), "second");
        assert_eq!(written[2], boundary);
    }
}

#[test]
fn separately_split_responses_have_distinct_identities_even_when_opposite_ends_are_lost() {
    let guard = secret_guard::SecretRedactor::with_known_values([]);
    let response = |text: &str| {
        Event::new(MessageEvent::MessageCompleted {
            run_id: "run".into(),
            text: text.repeat(1000),
        })
    };
    let a = split_oversize(&response("A"), 512, &guard).unwrap();
    let b = split_oversize(&response("B"), 512, &guard).unwrap();
    let identity = |event: &Event| {
        let EventKind::Message(MessageEvent::MessagePart { response_id, .. }) = &event.kind else {
            panic!("expected a part")
        };
        response_id.clone()
    };
    assert_ne!(identity(&a[0]), identity(&b[0]));
    let mut assembler = event_bus::MessagePartAssembler::new();
    assert!(assembler.accept(&a[0]).is_none());
    for part in &b[1..] {
        assert!(assembler.accept(part).is_none());
    }
    let c = response("C");
    let mut recovered = Vec::new();
    for part in split_oversize(&c, 512, &guard).unwrap() {
        if let Some(event) = assembler.accept(&part) {
            recovered.push(event.into_owned());
        }
    }
    assert_eq!(recovered, [c]);
}

#[test]
fn impossible_row_limits_remain_rejectable_and_do_not_loop() {
    let guard = secret_guard::SecretRedactor::with_known_values([]);
    for limit in [0, 1, 80] {
        assert!(split_oversize(&reasoning(Some("run"), "🦀"), limit, &guard).is_none());
    }
}

#[test]
fn a_known_secret_crossing_a_split_boundary_is_scanned_before_splitting() {
    let credential = ["fixture", "-private", "-credential"].concat();
    let guard = secret_guard::SecretRedactor::with_known_values([credential.clone()]);
    let no_credentials = secret_guard::SecretRedactor::with_known_values([]);
    for completed in [false, true] {
        let event_with = |text: String| {
            if completed {
                Event::new(MessageEvent::MessageCompleted {
                    run_id: "r".into(),
                    text,
                })
            } else {
                reasoning(Some("r"), &text)
            }
        };
        let probe = split_oversize(&event_with(" ".repeat(300)), 200, &no_credentials).unwrap();
        let text_of = |event: &Event| match &event.kind {
            EventKind::Message(MessageEvent::MessagePart { text, .. }) => text.len(),
            EventKind::Message(MessageEvent::ReasoningDelta { delta, .. }) => delta.len(),
            _ => unreachable!(),
        };
        let prefix = text_of(&probe[0]) - credential.len() / 2;
        let event = event_with(format!(
            "{}{}{}",
            " ".repeat(prefix),
            credential,
            " ".repeat(200)
        ));
        assert!(json_len(&event.kind) > 200);
        let unguarded = split_oversize(&event, 200, &no_credentials).unwrap();
        assert!(unguarded.iter().all(|row| match &row.kind {
            EventKind::Message(MessageEvent::MessagePart { text, .. }) =>
                guard.detect(text).is_none(),
            EventKind::Message(MessageEvent::ReasoningDelta { delta, .. }) =>
                guard.detect(delta).is_none(),
            _ => false,
        }));
        assert!(split_oversize(&event, 200, &guard).is_none());
    }
}

#[tokio::test]
async fn message_deltas_are_dropped_and_do_not_end_a_reasoning_row() {
    let (mut queue, mut rx, monitor) = queue();
    queue.push(reasoning(Some("run"), "think ")).await.unwrap();
    for _ in 0..3 {
        queue.push(message_delta("run", "answer")).await.unwrap();
    }
    queue.push(reasoning(Some("run"), "more")).await.unwrap();
    queue.flush().await.unwrap();
    assert_eq!(
        collect(&mut rx).iter().map(text).collect::<Vec<_>>(),
        ["think more"]
    );
    assert_eq!(monitor.snapshot().skipped_message_deltas, 3);
}

#[tokio::test]
async fn a_stream_retry_ends_the_failed_attempts_row_without_being_stored() {
    let (mut queue, mut rx, _) = queue();
    queue
        .push(reasoning(Some("run"), "failed attempt"))
        .await
        .unwrap();
    queue
        .push(Event::new(MessageEvent::StreamRetried {
            run_id: Some("run".into()),
        }))
        .await
        .unwrap();
    queue.push(reasoning(Some("run"), "retry")).await.unwrap();
    queue.flush().await.unwrap();
    assert_eq!(
        collect(&mut rx).iter().map(text).collect::<Vec<_>>(),
        ["failed attempt", "retry"]
    );
}

#[tokio::test]
async fn escaped_json_size_bounds_merged_rows_at_64_kib_and_preserves_first_metadata() {
    assert_eq!(MAX_COALESCED_BYTES, 64 * 1024);
    let (mut queue, mut rx, _) = queue();
    // Every character escapes, so serialized size differs from the text length.
    let chunk = "\"\\\n🦀".repeat(900);
    let first = reasoning(Some("run"), &chunk);
    queue.push(first.clone()).await.unwrap();
    for _ in 0..19 {
        queue.push(reasoning(Some("run"), &chunk)).await.unwrap();
    }
    queue.flush().await.unwrap();
    let events = collect(&mut rx);
    assert!(
        (3..20).contains(&events.len()),
        "20 chunks of ~9 KB need at least three 64 KiB rows: {}",
        events.len()
    );
    assert_eq!(events[0].meta, first.meta);
    for event in &events {
        assert!(serde_json::to_vec(&event.kind).unwrap().len() <= MAX_COALESCED_BYTES);
    }
    assert_eq!(
        events.iter().map(text).collect::<String>(),
        chunk.repeat(20)
    );
}

#[tokio::test]
async fn the_storage_event_limit_caps_rows_below_64_kib() {
    let (mut queue, mut rx, _) = queue_with_limit(1024);
    for _ in 0..20 {
        queue
            .push(reasoning(Some("run"), &"x".repeat(100)))
            .await
            .unwrap();
    }
    queue.flush().await.unwrap();
    let events = collect(&mut rx);
    assert!(events.len() > 1);
    for event in &events {
        assert!(serde_json::to_vec(&event.kind).unwrap().len() <= 1024);
    }
    assert_eq!(
        events.iter().map(text).collect::<String>(),
        "x".repeat(2000)
    );
}

#[tokio::test(start_paused = true)]
async fn a_row_is_written_once_it_is_five_seconds_old_independently_per_run() {
    let (mut queue, mut rx, _) = queue();
    queue.push(reasoning(Some("a"), "old")).await.unwrap();
    tokio::time::advance(MAX_COALESCE_AGE - Duration::from_secs(1)).await;
    queue.push(reasoning(Some("b"), "young")).await.unwrap();
    // A later fragment does not renew the first one's deadline.
    queue.push(reasoning(Some("a"), " older")).await.unwrap();
    queue.flush_expired().await.unwrap();
    assert!(collect(&mut rx).is_empty());

    tokio::time::advance(Duration::from_secs(1)).await;
    queue.flush_expired().await.unwrap();
    assert_eq!(
        collect(&mut rx).iter().map(text).collect::<Vec<_>>(),
        ["old older"]
    );

    tokio::time::advance(MAX_COALESCE_AGE).await;
    queue.flush_expired().await.unwrap();
    assert_eq!(
        collect(&mut rx).iter().map(text).collect::<Vec<_>>(),
        ["young"]
    );
}

#[tokio::test]
async fn capacity_flushes_partial_rows_before_waiting_and_drop_clears_monitor() {
    let (mut queue, mut rx, monitor) = queue();
    queue.event_budget = Arc::new(Semaphore::new(3));
    for _ in 0..3 {
        queue.push(reasoning(Some("run"), "x")).await.unwrap();
    }
    let producer = tokio::spawn(async move {
        queue.push(reasoning(Some("run"), "y")).await.unwrap();
        queue
    });
    let first = rx.recv().await.unwrap();
    assert_eq!(monitor.snapshot().pending_events, 3);
    assert!(matches!(&first, WriteRequest::Event(event) if text(&event.event) == "xxx"));
    drop(first);
    let queue = producer.await.unwrap();
    assert_eq!(monitor.snapshot().pending_events, 1);
    assert_eq!(monitor.snapshot().peak_pending_events, 3);
    drop(queue);
    assert_eq!(monitor.snapshot().pending_events, 0);
    assert_eq!(monitor.snapshot().pending_bytes, 0);
}

#[tokio::test]
async fn byte_capacity_does_not_hold_partial_permits_while_waiting() {
    let (mut queue, mut rx, monitor) = queue();
    let event = reasoning(Some("run"), "x");
    let bytes = json_len(&event);
    queue.byte_budget = Arc::new(Semaphore::new(bytes * 2));
    queue.push(event.clone()).await.unwrap();
    queue.push(event.clone()).await.unwrap();
    let producer = tokio::spawn(async move {
        queue.push(event).await.unwrap();
        queue
    });
    let first = rx.recv().await.unwrap();
    assert_eq!(monitor.snapshot().pending_bytes, bytes * 2);
    drop(first);
    let queue = producer.await.unwrap();
    assert_eq!(monitor.snapshot().peak_pending_bytes, bytes * 2);
    drop(queue);
    assert_eq!(monitor.snapshot().pending_bytes, 0);
}

#[tokio::test]
async fn usage_tick_flushes_reasoning_first_and_cancelled_receiver_releases_capacity() {
    let (mut queue, mut rx, monitor) = queue();
    queue.push(reasoning(Some("run"), "pending")).await.unwrap();
    queue.flush_usage().await.unwrap();
    assert!(matches!(rx.try_recv().unwrap(), WriteRequest::Event(_)));
    assert!(matches!(rx.try_recv().unwrap(), WriteRequest::FlushUsage));
    queue
        .push(reasoning(Some("run"), "cancelled"))
        .await
        .unwrap();
    drop(rx);
    assert!(queue.flush().await.is_err());
    assert_eq!(monitor.snapshot().pending_events, 0);
    assert_eq!(monitor.snapshot().oldest_pending_age, Duration::ZERO);
}
