//! GUI event persistence and downsampled usage delivery.

use std::{future::Future, sync::Arc, task::Poll, time::Duration};

use event_bus::{Event, EventBus, EventKind, RecvError, UsageAggregator};
use storage::{StorageError, StorageHandle};

/// Routes GUI events to the storage writer.
pub struct StorageBridge {
    storage: StorageHandle,
    session_id: &'static str,
    usage: UsageAggregator,
    validator: Option<event_bus::MutationValidator>,
}

impl StorageBridge {
    pub fn new(storage: StorageHandle, session_id: &'static str) -> Self {
        Self {
            storage,
            session_id,
            usage: UsageAggregator::new(),
            validator: None,
        }
    }

    pub fn handle_event(&mut self, event: &Event) -> Result<(), StorageError> {
        match &event.kind {
            EventKind::Usage(usage) => {
                self.usage.record(usage, &event.meta);
                Ok(())
            }
            EventKind::Lifecycle(_)
            | EventKind::Message(_)
            | EventKind::Tool(_)
            | EventKind::Provider(_)
            | EventKind::Fault(_)
            | EventKind::AgentMessage(_)
            | EventKind::Compaction(_)
            | EventKind::Orchestrator(_)
            | EventKind::Diagnostic(_)
            | EventKind::Ownership(_)
            | EventKind::Snapshot(_) => match &self.validator {
                Some(validator) => self.storage.append_fenced_event(
                    Some(self.session_id),
                    event,
                    validator.clone(),
                ),
                None => self.storage.append_event(Some(self.session_id), event),
            },
        }
    }

    pub fn flush_usage(&mut self) {
        self.usage.flush_into(&self.storage);
        if let Err(error) = self.storage.flush_usage_now() {
            tracing::warn!(%error, "failed to flush usage metrics");
        }
    }
}

/// Persists events without blocking the caller's runtime, flushing on ticks and shutdown.
///
/// `flush_every` must be nonzero. Bus ownership is released after subscribing so
/// dropping the last producer ends the bridge at the next tick, after draining.
pub async fn run(bus: Arc<EventBus>, mut bridge: StorageBridge, flush_every: Duration) {
    bridge.validator = Some(bus.mutation_validator());
    let mut subscriber = bus.subscribe();
    let bus_lifetime = Arc::downgrade(&bus);
    drop(bus);
    let mut ticker =
        tokio::time::interval_at(tokio::time::Instant::now() + flush_every, flush_every);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut draining = false;
    loop {
        let received = if draining {
            // EventReceiver owns a Sender: Closed cannot signal producer shutdown.
            // Poll once to drain buffered events without waiting on that Sender.
            std::future::poll_fn(|cx| {
                let receive = subscriber.recv();
                tokio::pin!(receive);
                Poll::Ready(match receive.poll(cx) {
                    Poll::Ready(result) => Some(result),
                    Poll::Pending => None,
                })
            })
            .await
        } else {
            tokio::select! {
                result = subscriber.recv() => Some(result),
                _ = ticker.tick() => None,
            }
        };
        let event = match received {
            Some(Ok(event)) => Some(event),
            Some(Err(RecvError::Lagged(skipped))) => {
                tracing::warn!(skipped, "storage bridge lagged");
                continue;
            }
            Some(Err(RecvError::Closed)) => break,
            None if draining => break,
            None => {
                draining = bus_lifetime.strong_count() == 0;
                None
            }
        };
        let dispatch = tracing::dispatcher::get_default(Clone::clone);
        let result = tokio::task::spawn_blocking(move || {
            tracing::dispatcher::with_default(&dispatch, || {
                match event {
                    Some(event) => {
                        if let Err(error) = bridge.handle_event(&event) {
                            tracing::warn!(%error, "failed to persist event");
                        }
                    }
                    None => bridge.flush_usage(),
                }
                bridge
            })
        })
        .await;
        match result {
            Ok(returned) => bridge = returned,
            Err(error) => {
                tracing::error!(%error, "storage bridge worker failed");
                return;
            }
        }
    }
    let dispatch = tracing::dispatcher::get_default(Clone::clone);
    if let Err(error) = tokio::task::spawn_blocking(move || {
        tracing::dispatcher::with_default(&dispatch, || bridge.flush_usage())
    })
    .await
    {
        tracing::error!(%error, "storage bridge final flush worker failed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use event_bus::{LifecycleEvent, OwnershipAction, OwnershipEvent, UsageEvent};
    use std::time::{Duration, UNIX_EPOCH};
    use storage::{Database, Storage, StorageConfig};

    fn fixture() -> (tempfile::TempDir, Storage, Database) {
        let dir = tempfile::tempdir().unwrap();
        let config = StorageConfig {
            db_path: dir.path().join("events.db"),
            ..StorageConfig::default()
        };
        let storage = Storage::open(config.clone()).unwrap();
        let db = Database::open(&config).unwrap();
        (dir, storage, db)
    }

    fn usage_event(tokens: u64) -> Event {
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

    #[test]
    fn usage_event_is_not_persisted_raw() {
        // Given: a real storage writer.
        let (_dir, storage, db) = fixture();
        let mut bridge = StorageBridge::new(storage.handle(), "session");
        // When: usage reaches the bridge.
        let result = bridge.handle_event(&usage_event(10));
        // Then: raw usage is accepted but never persisted.
        assert!(result.is_ok(), "{result:?}");
        assert!(db.events_all_ordered().unwrap().is_empty());
    }

    #[test]
    fn lifecycle_event_is_persisted() {
        // Given: a session lifecycle event.
        let (_dir, storage, db) = fixture();
        let mut bridge = StorageBridge::new(storage.handle(), "session");
        let event = Event::new(LifecycleEvent::Started {
            session_id: "session".into(),
        });
        // When: it reaches the bridge.
        bridge.handle_event(&event).unwrap();
        // Then: the event remains available for replay.
        let events = db.events_all_ordered().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event, event);
    }

    #[test]
    fn ownership_event_is_persisted() {
        let (_dir, storage, db) = fixture();
        let mut bridge = StorageBridge::new(storage.handle(), "session");
        let event = Event::new(OwnershipEvent {
            thread_id: "thread-1".into(),
            owner_id: "owner-1".into(),
            generation: 2,
            action: OwnershipAction::Quiescing,
        });

        bridge.handle_event(&event).unwrap();

        let events = db.events_all_ordered().unwrap();
        assert_eq!(
            events,
            vec![storage::StoredEvent {
                id: events[0].id,
                session_id: Some("session".into()),
                event,
            }]
        );
    }

    #[test]
    fn flush_usage_produces_metrics_bucket() {
        // Given: two observations in one minute.
        let (_dir, storage, db) = fixture();
        let mut bridge = StorageBridge::new(storage.handle(), "session");
        bridge.handle_event(&usage_event(10)).unwrap();
        bridge.handle_event(&usage_event(20)).unwrap();
        // When: usage is flushed twice (the second flush must be empty).
        bridge.flush_usage();
        bridge.flush_usage();
        storage.handle().flush_usage_now().unwrap();
        // Then: one additive bucket, without duplicate accounting.
        let metrics = db.metrics_range(120, 180).unwrap();
        assert_eq!(metrics.len(), 1);
        assert_eq!(metrics[0].input_tokens, 30);
        assert_eq!(metrics[0].output_tokens, 4);
        assert_eq!(metrics[0].cache_read_tokens, 6);
        assert_eq!(metrics[0].cache_write_tokens, 8);
        assert_eq!(metrics[0].request_count, 2);
    }
}
