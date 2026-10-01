use super::*;
use event_bus::LifecycleEvent;

fn delta(run: Option<&str>, text: &str, reasoning: bool) -> Event {
    Event::new(if reasoning {
        MessageEvent::ReasoningDelta {
            run_id: run.map(str::to_owned),
            delta: text.into(),
        }
    } else {
        MessageEvent::MessageDelta {
            run_id: run.map(str::to_owned),
            delta: text.into(),
        }
    })
}

fn queue() -> (
    EventQueue,
    mpsc::Receiver<WriteRequest>,
    StorageBridgeMonitor,
) {
    let (tx, rx) = mpsc::channel(WRITE_QUEUE_CAPACITY);
    let monitor = StorageBridgeMonitor::default();
    (
        EventQueue::new(tx, monitor.clone(), PersistencePolicy::default(), u64::MAX),
        rx,
        monitor,
    )
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
        EventKind::Message(
            MessageEvent::MessageDelta { delta, .. } | MessageEvent::ReasoningDelta { delta, .. },
        ) => delta,
        other => panic!("expected delta: {other:?}"),
    }
}

#[tokio::test]
async fn burst_preserves_text_and_bounds_original_event_count() {
    let (mut queue, mut rx, monitor) = queue();
    let mut expected = String::new();
    for n in 0..512 {
        let part = format!("{n}:🦀\n");
        expected.push_str(&part);
        queue.push(delta(Some("run"), &part, false)).await.unwrap();
    }
    queue.flush().await.unwrap();
    assert_eq!(monitor.snapshot().pending_events, 512);
    let events = collect(&mut rx);
    assert_eq!(events.len(), 2);
    assert_eq!(events.iter().map(text).collect::<String>(), expected);
    assert_eq!(monitor.snapshot().coalesced_events, 510);
    assert_eq!(monitor.snapshot().pending_events, 0);
    assert_eq!(monitor.snapshot().pending_bytes, 0);
    assert_eq!(monitor.snapshot().oldest_pending_age, Duration::ZERO);
}

#[tokio::test]
async fn reasoning_run_and_semantic_boundaries_preserve_order() {
    let (mut queue, mut rx, _) = queue();
    let boundary = Event::new(LifecycleEvent::Completed {
        session_id: "session".into(),
    });
    let expected = [
        delta(Some("a"), "message", false),
        delta(Some("a"), "thought", true),
        delta(Some("b"), "other", true),
        Event::new(event_bus::ToolEvent::ToolStarted {
            tool_name: "shell".into(),
            call_id: "b:1".into(),
            input: None,
            run_id: Some("b".into()),
        }),
        delta(Some("b"), "after tool", true),
        boundary,
        delta(Some("b"), "after", true),
        delta(None, "uncorrelated 1", false),
        delta(None, "uncorrelated 2", false),
    ];
    for event in &expected {
        queue.push(event.clone()).await.unwrap();
    }
    queue.flush().await.unwrap();
    assert_eq!(collect(&mut rx), expected);
}

#[tokio::test]
async fn escaped_json_size_bounds_merged_rows_and_preserves_first_metadata() {
    let (mut queue, mut rx, _) = queue();
    let chunk = "\"\\\n🦀".repeat(900);
    let first = delta(Some("run"), &chunk, false);
    queue.push(first.clone()).await.unwrap();
    for _ in 0..10 {
        queue.push(delta(Some("run"), &chunk, false)).await.unwrap();
    }
    queue.flush().await.unwrap();
    let events = collect(&mut rx);
    assert!(events.len() > 1 && events.len() < 11);
    assert_eq!(events[0].meta, first.meta);
    for event in &events {
        assert!(serde_json::to_vec(&event.kind).unwrap().len() <= MAX_COALESCED_BYTES);
    }
    assert_eq!(
        events.iter().map(text).collect::<String>(),
        chunk.repeat(11)
    );
}

#[tokio::test]
async fn capacity_flushes_partial_delta_before_waiting_and_drop_clears_monitor() {
    let (mut queue, mut rx, monitor) = queue();
    queue.event_budget = Arc::new(Semaphore::new(3));
    for _ in 0..3 {
        queue.push(delta(Some("run"), "x", false)).await.unwrap();
    }
    let producer = tokio::spawn(async move {
        queue.push(delta(Some("run"), "y", false)).await.unwrap();
        queue
    });
    let first = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(monitor.snapshot().pending_events, 3);
    assert!(matches!(&first, WriteRequest::Event(event) if text(&event.event) == "xxx"));
    drop(first);
    let queue = tokio::time::timeout(Duration::from_secs(2), producer)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(monitor.snapshot().pending_events, 1);
    assert_eq!(monitor.snapshot().peak_pending_events, 3);
    drop(queue);
    assert_eq!(monitor.snapshot().pending_events, 0);
    assert_eq!(monitor.snapshot().pending_bytes, 0);
}

#[tokio::test]
async fn byte_capacity_does_not_hold_partial_permits_while_waiting() {
    let (mut queue, mut rx, monitor) = queue();
    let event = delta(Some("run"), "x", false);
    let bytes = json_len(&event);
    queue.byte_budget = Arc::new(Semaphore::new(bytes * 2));
    queue.push(event.clone()).await.unwrap();
    queue.push(event.clone()).await.unwrap();
    let producer = tokio::spawn(async move {
        queue.push(event).await.unwrap();
        queue
    });
    let first = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(monitor.snapshot().pending_bytes, bytes * 2);
    drop(first);
    let queue = tokio::time::timeout(Duration::from_secs(2), producer)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(monitor.snapshot().peak_pending_bytes, bytes * 2);
    drop(queue);
    assert_eq!(monitor.snapshot().pending_bytes, 0);
}

#[tokio::test]
async fn usage_tick_flushes_delta_first_and_cancelled_receiver_releases_capacity() {
    let (mut queue, mut rx, monitor) = queue();
    queue
        .push(delta(Some("run"), "pending", false))
        .await
        .unwrap();
    queue.flush_usage().await.unwrap();
    assert!(matches!(rx.try_recv().unwrap(), WriteRequest::Event(_)));
    assert!(matches!(rx.try_recv().unwrap(), WriteRequest::FlushUsage));
    queue
        .push(delta(Some("run"), "cancelled", false))
        .await
        .unwrap();
    drop(rx);
    assert!(queue.flush().await.is_err());
    assert_eq!(monitor.snapshot().pending_events, 0);
    assert_eq!(monitor.snapshot().oldest_pending_age, Duration::ZERO);
}
