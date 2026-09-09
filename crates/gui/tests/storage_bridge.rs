use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, UNIX_EPOCH};

use event_bus::{Event, EventBus, LifecycleEvent, UsageEvent};
use gui::storage_bridge::{self, StorageBridge};
use storage::{Database, Storage, StorageConfig};
use tracing::instrument::WithSubscriber;
use tracing_subscriber::prelude::*;

struct WarningCount(Arc<AtomicUsize>);

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for WarningCount {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        if *event.metadata().level() == tracing::Level::WARN {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }
}

fn fixture() -> (tempfile::TempDir, Storage, Database) {
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("events.db"),
        flush_interval: Duration::from_secs(3600),
        ..StorageConfig::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    let db = Database::open(&config).unwrap();
    (dir, storage, db)
}

fn usage(tokens: u64) -> Event {
    let mut event = Event::new(UsageEvent::Usage {
        provider: "provider".into(),
        model: "model".into(),
        input_tokens: tokens,
        output_tokens: 2,
        cache_read_tokens: 3,
        cache_write_tokens: 4,
    });
    event.meta.wall_clock = UNIX_EPOCH + Duration::from_secs(125);
    event
}

async fn wait_for_subscription(bus: &EventBus) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while bus.receiver_count() == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("bridge subscribed");
}

#[tokio::test]
async fn usage_is_flushed_periodically_while_lifecycle_is_persisted() {
    // Given: the real bridge and a writer with automatic flush disabled for this test.
    let (_dir, storage, db) = fixture();
    let bus = Arc::new(EventBus::new(32));
    let bridge = StorageBridge::new(storage.handle(), "session");
    let warnings = Arc::new(AtomicUsize::new(0));
    let subscriber = tracing_subscriber::registry().with(WarningCount(warnings.clone()));
    let task = tokio::spawn(
        storage_bridge::run(bus.clone(), bridge, Duration::from_millis(50))
            .with_subscriber(subscriber),
    );
    wait_for_subscription(&bus).await;
    let lifecycle = Event::new(LifecycleEvent::Started {
        session_id: "session".into(),
    });

    // When: two usage observations and one lifecycle event arrive.
    assert_eq!(bus.emit(usage(10)), 1);
    assert_eq!(bus.emit(usage(20)), 1);
    assert_eq!(bus.emit(lifecycle.clone()), 1);
    tokio::time::sleep(Duration::from_millis(60)).await;

    // Then: the periodic tick persists one summed bucket, without raw usage or warnings.
    // A bounded observation wait tolerates slow CI scheduling, not a second writer flush.
    let metrics = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let metrics = db.metrics_range(120, 180).unwrap();
            if metrics
                .first()
                .is_some_and(|bucket| bucket.request_count == 2)
            {
                break metrics;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(metrics.len(), 1);
    assert_eq!(metrics[0].input_tokens, 30);
    let events = db.events_all_ordered().unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event, lifecycle);
    drop(bus);
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(warnings.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn dropping_bus_drains_pending_events_and_flushes_usage() {
    // Given: a subscribed bridge with queued events at producer shutdown.
    let (_dir, storage, db) = fixture();
    let bus = Arc::new(EventBus::new(32));
    let bridge = StorageBridge::new(storage.handle(), "session");
    let task = tokio::spawn(storage_bridge::run(
        bus.clone(),
        bridge,
        Duration::from_millis(50),
    ));
    wait_for_subscription(&bus).await;
    assert_eq!(bus.emit(usage(10)), 1);
    assert_eq!(bus.emit(usage(20)), 1);

    // When: the final producer is dropped before the next flush tick.
    drop(bus);
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap();

    // Then: completion guarantees pending usage has reached SQLite, with no raw rows.
    let metrics = db.metrics_range(120, 180).unwrap();
    assert_eq!(metrics.len(), 1);
    assert_eq!(metrics[0].input_tokens, 30);
    assert_eq!(metrics[0].request_count, 2);
    assert!(db.events_all_ordered().unwrap().is_empty());
}
