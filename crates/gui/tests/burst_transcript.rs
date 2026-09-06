use std::future::Future;
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use event_bus::{
    AgentRunPhase, Event, EventBus, EventKind, EventReceiver, FaultEvent, LifecycleEvent,
    MessageEvent, RecvError,
};
use gui::app::WorkbenchState;
use gui::events::EventPump;
use gui::model::tasks::AgentRunSource;
use gui::model::transcript::{TranscriptEntry, TranscriptModel};
use runtime::AgentSummary;
use workspace_ui::{ThreadRunPhase, UiSettings};

const TOKENS_PER_TICK: usize = 1500;
const TICKS: usize = 20;
const BUS_CAPACITY: usize = 8192;
const MAX_FLUSH_ITERS: usize = 100_000;
const RUN_IDS: [&str; 2] = ["run_a", "run_b"];

struct EmptySource;

impl AgentRunSource for EmptySource {
    fn list(&self) -> Vec<AgentSummary> {
        Vec::new()
    }
}

struct Harness {
    pump: EventPump,
    raw: EventReceiver,
    bus: EventBus,
    rt: tokio::runtime::Runtime,
    state: WorkbenchState<EmptySource>,
}

impl Harness {
    fn new() -> Self {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        let bus = EventBus::new(BUS_CAPACITY);
        let raw = bus.subscribe();
        let pump = EventPump::spawn(rt.handle(), bus.subscribe(), None);
        let state = WorkbenchState::new(EmptySource, &UiSettings::default()).expect("state");
        Self {
            pump,
            raw,
            bus,
            rt,
            state,
        }
    }

    fn flush(&mut self, expected: &[Event]) -> Vec<Event> {
        let mut received = Vec::with_capacity(expected.len());
        for _ in 0..MAX_FLUSH_ITERS {
            self.rt.block_on(async { tokio::task::yield_now().await });
            let drained = self.pump.drain();
            assert_lag_free(&drained);
            received.extend(drained);
            if received.len() >= expected.len() {
                let watched = drain_watchdog(&mut self.raw).expect("watchdog lag or closure");
                assert_lag_free(&watched);
                assert_eq!(watched, expected, "watchdog delivery differs");
                assert_eq!(received, expected, "pump delivery differs");
                return received;
            }
        }
        panic!(
            "stall: drained {} of {} events",
            received.len(),
            expected.len()
        );
    }
}

fn assert_lag_free(events: &[Event]) {
    for event in events {
        assert!(
            !matches!(
                event.kind,
                EventKind::Fault(FaultEvent::SubscriberLagged { .. })
            ),
            "subscriber lag: {event:?}"
        );
    }
}

fn burst(tick: usize) -> Vec<Event> {
    (0..TOKENS_PER_TICK)
        .map(|i| {
            Event::new(MessageEvent::MessageDelta {
                delta: format!("t{tick}-{i} "),
                run_id: Some(RUN_IDS[i % 2].into()),
            })
        })
        .collect()
}

fn transcript_text(model: &TranscriptModel) -> String {
    model
        .entries()
        .iter()
        .map(|entry| match entry {
            TranscriptEntry::Message { text } => text.as_str(),
            TranscriptEntry::Reasoning { .. }
            | TranscriptEntry::Tool { .. }
            | TranscriptEntry::AgentMessage { .. } => panic!("unexpected transcript entry"),
        })
        .collect()
}

fn drain_watchdog(raw: &mut EventReceiver) -> Result<Vec<Event>, RecvError> {
    let mut events = Vec::new();
    loop {
        // EventReceiver exposes only recv; one poll provides a nonblocking probe.
        let mut receive = std::pin::pin!(raw.recv());
        match receive
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
        {
            Poll::Ready(Ok(event)) => events.push(event),
            Poll::Ready(Err(error)) => return Err(error),
            Poll::Pending => return Ok(events),
        }
    }
}

#[test]
fn burst_harness_detects_lag_with_tiny_capacity() {
    // Given: a watchdog subscribed before a burst exceeding capacity.
    let bus = EventBus::new(8);
    let mut raw = bus.subscribe();

    // When: 64 deltas arrive without a flush.
    for i in 0..64 {
        bus.emit(Event::new(MessageEvent::MessageDelta {
            delta: format!("t0-{i} "),
            run_id: Some("run_a".into()),
        }));
    }
    let observed = drain_watchdog(&mut raw);

    // Then: the detector reports lag or a short delivery.
    match observed {
        Err(RecvError::Lagged(n)) => assert!(n > 0),
        Err(RecvError::Closed) => panic!("watchdog closed"),
        Ok(events) => assert!(events.len() < 64),
    }
}

#[test]
fn burst_1500_tokens_per_tick_reaches_transcript_without_drops() {
    // Given: two interleaved runs, a real pump, and an independent watchdog.
    let mut harness = Harness::new();
    let mut expected: [Vec<String>; 2] = std::array::from_fn(|_| Vec::new());
    let mut received: [Vec<String>; 2] = std::array::from_fn(|_| Vec::new());
    let mut thread_expected = String::new();
    let mut latencies = Vec::with_capacity(TICKS);
    let mut total_received = 0;
    let start = Instant::now();

    // When: each logical second emits 1500 deltas, then flushes before the next.
    for tick in 0..TICKS {
        let events = burst(tick);
        for i in 0..TOKENS_PER_TICK {
            let delta = format!("t{tick}-{i} ");
            thread_expected.push_str(&delta);
            expected[i % 2].push(delta);
        }
        let tick_start = Instant::now();
        for event in &events {
            harness.bus.emit(event.clone());
        }
        let drained = harness.flush(&events);
        latencies.push(tick_start.elapsed());
        total_received += drained.len();
        for event in &drained {
            let EventKind::Message(MessageEvent::MessageDelta {
                delta,
                run_id: Some(id),
            }) = &event.kind
            else {
                panic!("expected attributed delta");
            };
            let index = RUN_IDS
                .iter()
                .position(|run_id| run_id == id)
                .expect("known run");
            received[index].push(delta.clone());
        }
        harness.state.apply_events(drained);
    }
    let elapsed = start.elapsed();

    // Then: counts, ordered deltas, and coalesced transcripts retain every token.
    assert_eq!(total_received, TICKS * TOKENS_PER_TICK);
    assert_eq!(received, expected);
    for (index, run_id) in RUN_IDS.iter().enumerate() {
        assert_eq!(
            transcript_text(
                harness
                    .state
                    .transcripts()
                    .run(run_id)
                    .expect("run transcript")
            ),
            expected[index].concat()
        );
    }
    assert_eq!(
        transcript_text(harness.state.transcripts().thread()),
        thread_expected
    );
    assert!(elapsed < Duration::from_secs(30), "burst exceeded failsafe");
    latencies.sort_unstable();
    println!(
        "BURST_MEASUREMENT total_events={} ticks={} tokens_per_tick={} elapsed_ms={:.3} events_per_sec={:.0} tick_latency_ms_min={:.3} tick_latency_ms_p50={:.3} tick_latency_ms_max={:.3}",
        total_received,
        TICKS,
        TOKENS_PER_TICK,
        elapsed.as_secs_f64() * 1000.0,
        f64::from(u32::try_from(total_received).expect("event count fits u32"))
            / elapsed.as_secs_f64(),
        latencies[0].as_secs_f64() * 1000.0,
        latencies[(TICKS - 1) / 2].as_secs_f64() * 1000.0,
        latencies[TICKS - 1].as_secs_f64() * 1000.0,
    );
}

#[test]
fn burst_pump_path_matches_apply_events_path() {
    // Given: identical lifecycle and burst inputs for pump and synchronous folds.
    let mut harness = Harness::new();
    let mut synchronous = WorkbenchState::new(EmptySource, &UiSettings::default()).expect("state");
    let lifecycle: Vec<Event> = RUN_IDS
        .iter()
        .map(|run_id| {
            Event::new(LifecycleEvent::AgentRunStateChanged {
                run_id: (*run_id).into(),
                from: AgentRunPhase::Pending,
                to: AgentRunPhase::Running,
                reason: None,
            })
        })
        .collect();

    // When: both paths process the same ordered batches, including real phases.
    for events in [lifecycle, burst(0), burst(1)] {
        for event in &events {
            harness.bus.emit(event.clone());
        }
        let drained = harness.flush(&events);
        harness.state.apply_events(drained);
        synchronous.apply_events(events);
    }

    // Then: run/thread text and nonempty phase maps agree exactly.
    for run_id in RUN_IDS {
        assert_eq!(
            transcript_text(harness.state.transcripts().run(run_id).expect("pump run")),
            transcript_text(synchronous.transcripts().run(run_id).expect("sync run")),
        );
        assert_eq!(
            harness.state.thread_phases().get(run_id),
            Some(&ThreadRunPhase::Running)
        );
    }
    assert_eq!(
        transcript_text(harness.state.transcripts().thread()),
        transcript_text(synchronous.transcripts().thread()),
    );
    assert_eq!(harness.state.thread_phases(), synchronous.thread_phases());
}
