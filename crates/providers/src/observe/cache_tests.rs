use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use event_bus::EventKind;
use futures_util::FutureExt;
use serde_json::{Value, json};
use tracing::field::{Field, Visit};
use tracing_subscriber::prelude::*;

use super::*;

fn wire() -> Value {
    json!({"model": "test", "messages": [{"role": "user", "content": "prefix"}]})
}

fn scoped_observer(bus: &Arc<EventBus>, run: &str, request: &Value) -> AttemptObserver {
    AttemptObserver::new(
        Some(bus.clone()),
        "test",
        None,
        "test",
        "test",
        false,
        Some(ObservationContext { run_id: run.into() }),
    )
    .with_cache_observation(request)
}

fn usage(cached: u64) -> Usage {
    Usage {
        input_tokens: 110,
        cache_read_tokens: cached,
        ..Usage::default()
    }
}

fn diagnostics(receiver: &mut event_bus::EventReceiver) -> usize {
    let mut count = 0;
    while let Some(event) = receiver.recv().now_or_never() {
        let event = event.expect("test events must not lag");
        count += usize::from(matches!(event.kind, EventKind::Diagnostic(_)));
    }
    count
}

#[test]
fn cache_write_establishes_a_baseline_only_after_completion() {
    let bus = Arc::new(EventBus::new(16));
    let mut receiver = bus.subscribe();
    let mut first = scoped_observer(&bus, "run", &wire());
    first.emit_completed(
        &Usage {
            input_tokens: 110,
            cache_write_tokens: 100,
            ..Usage::default()
        },
        FinishReason::Stop,
    );
    assert_eq!(diagnostics(&mut receiver), 0);
    scoped_observer(&bus, "run", &wire()).emit_completed(&usage(20), FinishReason::Stop);
    assert_eq!(diagnostics(&mut receiver), 1);
}

#[test]
fn concurrent_cold_attempt_stays_cold_when_peer_completes() {
    let bus = Arc::new(EventBus::new(16));
    let mut receiver = bus.subscribe();
    let mut first = scoped_observer(&bus, "run", &wire());
    let mut concurrent = scoped_observer(&bus, "run", &wire());
    first.emit_completed(&usage(100), FinishReason::Stop);
    concurrent.emit_completed(&usage(0), FinishReason::Stop);
    assert_eq!(diagnostics(&mut receiver), 0);
}

#[test]
fn warm_scope_is_isolated_when_bus_changes() {
    let bus = Arc::new(EventBus::new(16));
    scoped_observer(&bus, "run", &wire()).emit_completed(&usage(100), FinishReason::Stop);
    let other = Arc::new(EventBus::new(16));
    let mut receiver = other.subscribe();
    scoped_observer(&other, "run", &wire()).emit_completed(&usage(0), FinishReason::Stop);
    assert_eq!(diagnostics(&mut receiver), 0);
}

#[test]
fn rewritten_history_and_changed_request_settings_reset_the_baseline() {
    for changed in [
        json!({"model":"test", "messages":[{"role":"user","content":"compacted"}]}),
        json!({"model":"test", "messages":[], "tools":[{"name":"new"}]}),
        json!({"model":"test", "messages":[{"role":"user","content":"prefix"}], "reasoning":{"effort":"high"}}),
        json!({"model":"test", "messages":[]}),
    ] {
        let bus = Arc::new(EventBus::new(16));
        let mut receiver = bus.subscribe();
        scoped_observer(&bus, "run", &wire()).emit_completed(&usage(100), FinishReason::Stop);
        scoped_observer(&bus, "run", &changed).emit_completed(&usage(0), FinishReason::Stop);
        assert_eq!(diagnostics(&mut receiver), 0, "{changed}");
    }
}

#[test]
fn a_large_new_suffix_is_not_a_cache_regression() {
    let bus = Arc::new(EventBus::new(16));
    let mut receiver = bus.subscribe();
    scoped_observer(&bus, "run", &wire()).emit_completed(&usage(100), FinishReason::Stop);
    let mut appended = wire();
    appended["messages"]
        .as_array_mut()
        .unwrap()
        .push(json!({"role":"user","content":"new".repeat(10000)}));
    scoped_observer(&bus, "run", &appended).emit_completed(
        &Usage {
            input_tokens: 10000,
            cache_read_tokens: 100,
            ..Usage::default()
        },
        FinishReason::Stop,
    );
    assert_eq!(diagnostics(&mut receiver), 0);
}

#[tokio::test(start_paused = true)]
async fn stale_baseline_does_not_claim_cache_residency() {
    let bus = Arc::new(EventBus::new(16));
    let mut receiver = bus.subscribe();
    scoped_observer(&bus, "run", &wire()).emit_completed(&usage(100), FinishReason::Stop);
    tokio::time::advance(std::time::Duration::from_secs(301)).await;
    scoped_observer(&bus, "run", &wire()).emit_completed(&usage(0), FinishReason::Stop);
    assert_eq!(diagnostics(&mut receiver), 0);
}

#[tokio::test(start_paused = true)]
async fn late_older_completion_does_not_overwrite_the_newer_baseline() {
    let bus = Arc::new(EventBus::new(16));
    let mut receiver = bus.subscribe();
    let mut older = scoped_observer(&bus, "run", &wire());
    tokio::time::advance(std::time::Duration::from_millis(1)).await;
    let mut newer = scoped_observer(&bus, "run", &wire());
    newer.emit_completed(&usage(100), FinishReason::Stop);
    older.emit_completed(&usage(20), FinishReason::Stop);
    scoped_observer(&bus, "run", &wire()).emit_completed(&usage(20), FinishReason::Stop);
    assert_eq!(diagnostics(&mut receiver), 1);
}

#[test]
fn invalid_or_missing_usage_does_not_warm_a_request() {
    for invalid in [
        Usage::default(),
        Usage {
            input_tokens: 1,
            ..usage(100)
        },
        Usage {
            input_tokens: u64::MAX,
            cache_read_tokens: u64::MAX,
            cache_write_tokens: 1,
            ..Usage::default()
        },
    ] {
        let bus = Arc::new(EventBus::new(16));
        let mut receiver = bus.subscribe();
        scoped_observer(&bus, "run", &wire()).emit_completed(&invalid, FinishReason::Stop);
        scoped_observer(&bus, "run", &wire()).emit_completed(&usage(0), FinishReason::Stop);
        assert_eq!(diagnostics(&mut receiver), 0);
    }
}

#[derive(Clone, Default)]
struct Records(Arc<Mutex<Vec<BTreeMap<String, String>>>>);
struct Fields(BTreeMap<String, String>);
impl Visit for Fields {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.0.insert(field.name().into(), format!("{value:?}"));
    }
}
impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Records {
    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        let mut fields = Fields(BTreeMap::new());
        event.record(&mut fields);
        self.0.lock().unwrap().push(fields.0);
    }
}

#[test]
fn cache_completion_records_actual_ratio_once_without_a_bus() {
    let records = Records::default();
    let subscriber = tracing_subscriber::registry().with(records.clone());
    // Keep both dispatchers alive for tracing-core's concurrent callsite registration.
    let _unscoped = tracing::Dispatch::new(tracing::subscriber::NoSubscriber::default());
    tracing::subscriber::with_default(subscriber, || {
        std::thread::spawn(|| {
            AttemptObserver::new(None, "other", None, "test", "test", false, None)
                .emit_completed(&Usage::default(), FinishReason::Stop);
        })
        .join()
        .unwrap();
        let mut observer = AttemptObserver::new(None, "test", None, "test", "test", false, None)
            .with_cache_observation(&wire());
        observer.emit_completed(
            &Usage {
                input_tokens: 11702,
                cache_read_tokens: 1792,
                ..Usage::default()
            },
            FinishReason::Stop,
        );
        observer.emit_completed(&Usage::default(), FinishReason::Stop);
    });
    let entries = records.0.lock().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["input_tokens"], "11702");
    assert_eq!(entries[0]["cache_read_tokens"], "1792");
    assert_eq!(
        entries[0]["cache_hit_ratio"],
        (1792.0 / 11702.0).to_string()
    );
    assert!(!entries[0].contains_key("expected_cacheable_tokens"));
    assert!(!entries[0].contains_key("cache_estimation"));
    assert!(entries[0].contains_key("request_id"));
}

#[tokio::test(start_paused = true)]
async fn a_long_completion_does_not_refresh_cache_residency() {
    let bus = Arc::new(EventBus::new(16));
    let mut receiver = bus.subscribe();
    let mut slow = scoped_observer(&bus, "run", &wire());
    tokio::time::advance(std::time::Duration::from_secs(301)).await;
    slow.emit_completed(&usage(100), FinishReason::Stop);
    scoped_observer(&bus, "run", &wire()).emit_completed(&usage(0), FinishReason::Stop);
    assert_eq!(diagnostics(&mut receiver), 0);
}
