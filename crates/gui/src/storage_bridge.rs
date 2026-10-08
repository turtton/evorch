//! GUI event persistence and downsampled usage delivery.

use std::{future::Future, sync::Arc, task::Poll, time::Duration};

use event_bus::{
    DiagnosticSeverity, Event, EventBus, EventKind, OwnershipAction, RecvError, ToolEvent,
    UsageAggregator,
};
use storage::{StorageError, StorageHandle};

mod coalescing;
mod lifecycle;
mod monitor;
mod usage_ledger;
use coalescing::{COALESCE_INTERVAL, EventQueue, QueuedEvent, WRITE_QUEUE_CAPACITY};
pub use lifecycle::OwnedStorageBridge;
pub use monitor::{StorageBridgeMonitor, StorageBridgeSnapshot};

/// Controls durable diagnostic rows; live diagnostics continue on the event bus.
/// Sandbox escalation review audit records are always persisted, including `Off`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum DiagnosticPersistence {
    Off,
    #[default]
    Warnings,
    All,
}

#[derive(Clone, Copy)]
struct PersistencePolicy {
    diagnostics: DiagnosticPersistence,
    metrics_enabled: bool,
}

impl Default for PersistencePolicy {
    fn default() -> Self {
        Self {
            diagnostics: DiagnosticPersistence::default(),
            metrics_enabled: true,
        }
    }
}

impl PersistencePolicy {
    fn skip(self, event: &Event, monitor: &StorageBridgeMonitor) -> bool {
        match &event.kind {
            EventKind::Ownership(event) if event.action == OwnershipAction::Heartbeat => {
                monitor.skipped_heartbeat();
                true
            }
            EventKind::Diagnostic(event)
                if !(event.source == "sandbox" && event.code == "escalation_review")
                    && (self.diagnostics == DiagnosticPersistence::Off
                        || (self.diagnostics == DiagnosticPersistence::Warnings
                            && event.severity == DiagnosticSeverity::Info)) =>
            {
                monitor.skipped_diagnostic();
                true
            }
            EventKind::Usage(_) if !self.metrics_enabled => true,
            // Live shell output is display-only; the job's output artifact is
            // the durable record.
            EventKind::Tool(ToolEvent::ShellJobOutput { .. }) => true,
            _ => false,
        }
    }
}

/// Routes GUI events to the storage writer.
pub struct StorageBridge {
    storage: StorageHandle,
    session_id: &'static str,
    usage: UsageAggregator,
    usage_dirty: bool,
    validator: Option<event_bus::MutationValidator>,
    policy: PersistencePolicy,
    monitor: StorageBridgeMonitor,
    ledger: Option<usage_ledger::UsageRecorder>,
    #[cfg(test)]
    automatic_flush_enabled: bool,
}

impl StorageBridge {
    pub fn new(storage: StorageHandle, session_id: &'static str) -> Self {
        Self {
            storage,
            session_id,
            usage: UsageAggregator::new(),
            usage_dirty: false,
            validator: None,
            policy: PersistencePolicy::default(),
            monitor: StorageBridgeMonitor::default(),
            ledger: None,
            #[cfg(test)]
            automatic_flush_enabled: true,
        }
    }

    pub fn with_diagnostic_persistence(mut self, policy: DiagnosticPersistence) -> Self {
        self.policy.diagnostics = policy;
        self
    }

    pub fn with_metrics_enabled(mut self, enabled: bool) -> Self {
        self.policy.metrics_enabled = enabled;
        if !enabled {
            self.usage = UsageAggregator::new();
            self.usage_dirty = false;
            self.ledger = None;
        }
        self
    }

    /// Record one usage ledger row per provider attempt, priced from `pricing`.
    /// Has no effect while metrics are disabled.
    pub fn with_usage_ledger(
        mut self,
        pricing: crate::model::telemetry::pricing::SharedUsagePricing,
    ) -> Self {
        self.ledger = self
            .policy
            .metrics_enabled
            .then(|| usage_ledger::UsageRecorder::new(pricing));
        self
    }

    /// Retain this handle before passing the bridge to [`run`].
    pub fn monitor(&self) -> StorageBridgeMonitor {
        self.monitor.clone()
    }

    pub fn handle_event(&mut self, event: &Event) -> Result<(), StorageError> {
        if self.policy.skip(event, &self.monitor) {
            return Ok(());
        }
        if let EventKind::Usage(usage) = &event.kind {
            self.usage.record(usage, &event.meta);
            self.usage_dirty = true;
            return Ok(());
        }
        if let Some(record) = self
            .ledger
            .as_mut()
            .and_then(|ledger| ledger.observe(event))
            && let Err(error) = self.storage.record_usage_requests(vec![record])
        {
            self.monitor
                .warn(&format!("failed to record usage ledger row: {error}"));
        }
        let result =
            self.storage
                .append_stream_event(self.session_id, event, self.validator.clone());
        if result.is_ok() {
            self.monitor.persisted();
        } else {
            self.monitor.failed();
        }
        result
    }

    pub fn flush_usage(&mut self) {
        if let Err(error) = self.flush_usage_checked() {
            self.monitor
                .warn(&format!("failed to flush usage metrics: {error}"));
        }
    }

    fn flush_usage_checked(&mut self) -> Result<(), StorageError> {
        if !self.usage_dirty {
            return Ok(());
        }
        self.usage_dirty = false;
        self.usage.flush_into(&self.storage);
        self.storage.flush_usage_now()
    }
}

type FlushReply = std::sync::mpsc::Sender<Result<(), String>>;

enum WriteRequest {
    Event(QueuedEvent),
    FlushUsage,
    Barrier(FlushReply),
}

// A transient stack value from select, never retained in a collection. Boxing
// would add an allocation to every token before the bounded queue takes it.
#[allow(clippy::large_enum_variant)]
enum BridgeInput {
    Event(Result<Event, RecvError>),
    UsageTick,
    DeltaTick,
    Drained,
    Shutdown,
    Flush(FlushReply),
}

/// Persists events without blocking the caller's runtime, flushing on ticks and shutdown.
///
/// Adjacent deltas from one run coalesce for at most 100 ms (256 originals or
/// 32 KiB of payload per row, capped by the configured event-size limit).
/// All semantic boundaries flush preceding deltas. The original
/// event bus remains untouched; merged rows retain the first delta's timestamp.
/// `flush_every` must be nonzero. Dropping the last producer drains the bridge.
pub async fn run(bus: Arc<EventBus>, bridge: StorageBridge, flush_every: Duration) {
    run_until_shutdown(bus, bridge, flush_every, std::future::pending()).await;
}

/// Stop accepting new events when `shutdown` resolves, then persist the already
/// accepted queue and a finite snapshot of the bus backlog before returning.
/// Events arriving after that snapshot are left to live bus subscribers and are
/// not persisted by this bridge. Continuous producers cannot extend the drain.
pub async fn run_until_shutdown(
    bus: Arc<EventBus>,
    bridge: StorageBridge,
    flush_every: Duration,
    shutdown: impl Future<Output = ()>,
) {
    let subscriber = bus.subscribe();
    let bus_lifetime = Arc::downgrade(&bus);
    drop(bus);
    let (_flush_sender, flush_requests) = tokio::sync::mpsc::unbounded_channel();
    run_subscribed(
        subscriber,
        bus_lifetime,
        bridge,
        flush_every,
        shutdown,
        flush_requests,
    )
    .await;
}

async fn run_subscribed(
    mut subscriber: event_bus::EventReceiver,
    bus_lifetime: std::sync::Weak<EventBus>,
    mut bridge: StorageBridge,
    flush_every: Duration,
    shutdown: impl Future<Output = ()>,
    mut flush_requests: tokio::sync::mpsc::UnboundedReceiver<FlushReply>,
) {
    #[cfg(test)]
    let automatic_flush_enabled = bridge.automatic_flush_enabled;
    #[cfg(not(test))]
    let automatic_flush_enabled = true;
    bridge.validator = Some(subscriber.mutation_validator());
    tokio::pin!(shutdown);
    let (requests, mut pending) = tokio::sync::mpsc::channel(WRITE_QUEUE_CAPACITY);
    let monitor = bridge.monitor();
    let mut queue = EventQueue::new(
        requests,
        monitor.clone(),
        bridge.policy,
        bridge.storage.max_event_bytes(),
    );
    let dispatch = tracing::dispatcher::get_default(Clone::clone);
    // A single blocking worker keeps SQLite off the async runtime. Queue permits
    // remain held until persistence completes, including during cancellation.
    let writer = tokio::task::spawn_blocking(move || {
        tracing::dispatcher::with_default(&dispatch, || {
            let mut failure = None;
            while let Some(request) = pending.blocking_recv() {
                let result = match request {
                    WriteRequest::Event(queued) => bridge.handle_event(&queued.event),
                    WriteRequest::FlushUsage => bridge.flush_usage_checked(),
                    WriteRequest::Barrier(reply) => {
                        if let Err(error) = bridge.flush_usage_checked() {
                            failure.get_or_insert_with(|| error.to_string());
                        }
                        // This FIFO barrier runs only after preceding appends
                        // return from SQLite. Keep the subscriber alive for the
                        // final ownership Released event before shutdown.
                        let _ = reply.send(failure.take().map_or(Ok(()), Err));
                        continue;
                    }
                };
                if let Err(error) = result {
                    failure.get_or_insert_with(|| error.to_string());
                    bridge
                        .monitor
                        .warn(&format!("failed to persist event or usage: {error}"));
                }
            }
            bridge.flush_usage();
        });
    });
    let mut ticker =
        tokio::time::interval_at(tokio::time::Instant::now() + flush_every, flush_every);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut delta_ticker = tokio::time::interval_at(
        tokio::time::Instant::now() + COALESCE_INTERVAL,
        COALESCE_INTERVAL,
    );
    delta_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut draining = false;
    loop {
        let received = if draining {
            // EventReceiver owns a Sender: poll buffered events without waiting
            // on the receiver's own sender after the final producer is gone.
            std::future::poll_fn(|cx| {
                let receive = subscriber.recv();
                tokio::pin!(receive);
                Poll::Ready(match receive.poll(cx) {
                    Poll::Ready(result) => BridgeInput::Event(result),
                    Poll::Pending => BridgeInput::Drained,
                })
            })
            .await
        } else {
            tokio::select! {
                biased;
                _ = &mut shutdown => BridgeInput::Shutdown,
                Some(reply) = flush_requests.recv() => BridgeInput::Flush(reply),
                _ = ticker.tick(), if automatic_flush_enabled => BridgeInput::UsageTick,
                _ = delta_ticker.tick(), if automatic_flush_enabled => BridgeInput::DeltaTick,
                result = subscriber.recv() => BridgeInput::Event(result),
            }
        };
        let result = match received {
            BridgeInput::Flush(reply) => {
                for event in subscriber.drain_pending_snapshot() {
                    if queue.push(event).await.is_err() {
                        break;
                    }
                }
                queue.barrier(reply).await
            }
            BridgeInput::Shutdown => {
                // Capture before awaiting any writes: later emissions cannot
                // keep an application close waiting forever on live producers.
                for event in subscriber.drain_pending_snapshot() {
                    if queue.push(event).await.is_err() {
                        break;
                    }
                }
                break;
            }
            BridgeInput::Event(Ok(event)) => queue.push(event).await,
            BridgeInput::Event(Err(RecvError::Lagged(skipped))) => {
                monitor.warn(&format!("storage bridge lagged; {skipped} events skipped"));
                queue.flush().await
            }
            BridgeInput::Event(Err(RecvError::Closed)) | BridgeInput::Drained => break,
            BridgeInput::UsageTick => {
                draining = bus_lifetime.strong_count() == 0;
                queue.flush_usage().await
            }
            BridgeInput::DeltaTick => {
                draining = bus_lifetime.strong_count() == 0;
                monitor.warn_if_backlogged();
                queue.flush().await
            }
        };
        if result.is_err() {
            break;
        }
    }
    let _ = queue.flush().await;
    drop(queue);
    if let Err(error) = writer.await {
        monitor.warn(&format!("storage bridge worker failed: {error}"));
    }
}

/// Subscribe synchronously so events emitted immediately after starting a test
/// cannot be lost while the spawned task is waiting to be scheduled.
#[cfg(test)]
fn spawn_test_bridge(
    bus: &Arc<EventBus>,
    bridge: StorageBridge,
    flush_every: Duration,
) -> tokio::task::JoinHandle<()> {
    spawn_test_bridge_until_shutdown(bus, bridge, flush_every, std::future::pending())
}

#[cfg(test)]
fn spawn_test_bridge_until_shutdown(
    bus: &Arc<EventBus>,
    bridge: StorageBridge,
    flush_every: Duration,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> tokio::task::JoinHandle<()> {
    let subscriber = bus.subscribe();
    let lifetime = Arc::downgrade(bus);
    let (flush_sender, flush_requests) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        let _flush_sender = flush_sender;
        run_subscribed(
            subscriber,
            lifetime,
            bridge,
            flush_every,
            shutdown,
            flush_requests,
        )
        .await;
    })
}

#[cfg(test)]
#[path = "storage_bridge/tests.rs"]
mod tests;

#[cfg(test)]
#[path = "storage_bridge/limit_tests.rs"]
mod limit_tests;

#[cfg(test)]
#[path = "storage_bridge/usage_ledger_tests.rs"]
mod usage_ledger_tests;
