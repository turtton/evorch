//! Bound both the original event count and bytes, even when deltas are merged.

use std::{io, sync::Arc, time::Duration};

use event_bus::{Event, EventKind, MessageEvent};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};

use super::{PersistencePolicy, StorageBridgeMonitor, WriteRequest, monitor::PendingRegistration};

pub(super) const WRITE_QUEUE_CAPACITY: usize = 16_384;
const WRITE_QUEUE_BYTES: usize = 64 * 1024 * 1024;
pub(super) const COALESCE_INTERVAL: Duration = Duration::from_millis(100);
pub(super) const MAX_COALESCED_EVENTS: usize = 256;
pub(super) const MAX_COALESCED_BYTES: usize = 32 * 1024;

pub(super) struct QueuedEvent {
    pub(super) event: Box<Event>,
    source_events: usize,
    payload_bytes: usize,
    // Decrement observations before releasing capacity to a waiting producer.
    registration: PendingRegistration,
    event_permit: OwnedSemaphorePermit,
    byte_permit: OwnedSemaphorePermit,
}

impl QueuedEvent {
    fn can_merge(&self, next: &Event, max_payload_bytes: usize) -> bool {
        self.source_events < MAX_COALESCED_EVENTS
            && self.event.meta.schema_version == next.meta.schema_version
            && same_accounting_day(&self.event, next)
            && matching_delta(&self.event, next).is_some_and(|delta| {
                self.payload_bytes
                    .saturating_add(json_len(delta).saturating_sub(2))
                    <= max_payload_bytes
            })
    }

    fn merge(&mut self, other: Self) {
        let delta = matching_delta(&self.event, &other.event).expect("compatible delta");
        self.payload_bytes += json_len(delta) - 2;
        match &mut self.event.kind {
            EventKind::Message(
                MessageEvent::MessageDelta { delta: text, .. }
                | MessageEvent::ReasoningDelta { delta: text, .. },
            ) => text.push_str(delta),
            _ => unreachable!("only deltas can coalesce"),
        }
        self.source_events += other.source_events;
        self.event_permit.merge(other.event_permit);
        self.byte_permit.merge(other.byte_permit);
        self.registration.merge(other.registration);
    }
}

// Merged rows retain the first timestamp, so crossing midnight would charge
// tomorrow's bytes to yesterday. Invalid timestamps must remain independently
// rejectable rather than being hidden behind valid metadata from another delta.
fn same_accounting_day(first: &Event, next: &Event) -> bool {
    const NANOS_PER_DAY: i64 = 86_400_000_000_000;
    storage::system_time_to_ns(first.meta.wall_clock)
        .ok()
        .zip(storage::system_time_to_ns(next.meta.wall_clock).ok())
        .is_some_and(|(a, b)| a / NANOS_PER_DAY == b / NANOS_PER_DAY)
}

fn matching_delta<'a>(first: &Event, next: &'a Event) -> Option<&'a str> {
    match (&first.kind, &next.kind) {
        (
            EventKind::Message(MessageEvent::MessageDelta {
                run_id: Some(a), ..
            }),
            EventKind::Message(MessageEvent::MessageDelta {
                run_id: Some(b),
                delta,
            }),
        )
        | (
            EventKind::Message(MessageEvent::ReasoningDelta {
                run_id: Some(a), ..
            }),
            EventKind::Message(MessageEvent::ReasoningDelta {
                run_id: Some(b),
                delta,
            }),
        ) if a == b => Some(delta),
        _ => None,
    }
}

fn is_delta(event: &Event) -> bool {
    matches!(
        &event.kind,
        EventKind::Message(
            MessageEvent::MessageDelta {
                run_id: Some(_),
                ..
            } | MessageEvent::ReasoningDelta {
                run_id: Some(_),
                ..
            }
        )
    )
}

// Count JSON escapes without allocating another full serialized payload or
// repeatedly serializing the growing combined delta (which would be quadratic).
fn json_len(value: &(impl serde::Serialize + ?Sized)) -> usize {
    #[derive(Default)]
    struct Counter(usize);
    impl io::Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0 = self.0.saturating_add(bytes.len());
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut counter = Counter::default();
    if serde_json::to_writer(&mut counter, value).is_err() {
        // Let the storage writer report serialization failure, never merge it.
        return usize::MAX;
    }
    counter.0
}

pub(super) struct EventQueue {
    requests: mpsc::Sender<WriteRequest>,
    pending: Option<QueuedEvent>,
    event_budget: Arc<Semaphore>,
    byte_budget: Arc<Semaphore>,
    monitor: StorageBridgeMonitor,
    policy: PersistencePolicy,
    max_coalesced_bytes: usize,
}

impl EventQueue {
    pub(super) fn new(
        requests: mpsc::Sender<WriteRequest>,
        monitor: StorageBridgeMonitor,
        policy: PersistencePolicy,
        max_event_bytes: u64,
    ) -> Self {
        Self {
            requests,
            pending: None,
            event_budget: Arc::new(Semaphore::new(WRITE_QUEUE_CAPACITY)),
            byte_budget: Arc::new(Semaphore::new(WRITE_QUEUE_BYTES)),
            monitor,
            policy,
            max_coalesced_bytes: max_event_bytes.min(MAX_COALESCED_BYTES as u64) as usize,
        }
    }

    pub(super) async fn push(&mut self, event: Event) -> Result<(), ()> {
        if self.policy.skip(&event, &self.monitor) {
            return Ok(());
        }
        if self
            .pending
            .as_ref()
            .is_some_and(|pending| !pending.can_merge(&event, self.max_coalesced_bytes))
        {
            self.flush().await?;
        }
        let serialized_bytes = json_len(&event);
        // A single over-budget event is admitted exclusively and then subjected
        // to the existing storage event-size limit; it cannot share the queue.
        let bytes = serialized_bytes.min(WRITE_QUEUE_BYTES) as u32;
        let permits = match self.try_reserve(bytes) {
            Some(permits) => permits,
            None => {
                // Pending deltas own permits too. Send them before waiting so
                // the writer can release capacity, avoiding a partial-batch deadlock.
                self.flush().await?;
                let acquire = async {
                    let events = self
                        .event_budget
                        .clone()
                        .acquire_owned()
                        .await
                        .map_err(|_| ())?;
                    let bytes = self
                        .byte_budget
                        .clone()
                        .acquire_many_owned(bytes)
                        .await
                        .map_err(|_| ())?;
                    Ok::<_, ()>((events, bytes))
                };
                tokio::pin!(acquire);
                loop {
                    tokio::select! {
                        result = &mut acquire => break result?,
                        _ = self.requests.closed() => return Err(()),
                        _ = tokio::time::sleep(Duration::from_secs(5)) => self.monitor.warn_if_backlogged(),
                    }
                }
            }
        };
        // Storage limits count EventKind JSON, excluding timestamp metadata.
        let payload_bytes = json_len(&event.kind);
        let next = QueuedEvent {
            event: Box::new(event),
            source_events: 1,
            payload_bytes,
            event_permit: permits.0,
            byte_permit: permits.1,
            registration: self.monitor.register(serialized_bytes),
        };
        if let Some(pending) = &mut self.pending {
            pending.merge(next);
            self.monitor.coalesced();
        } else {
            self.pending = Some(next);
        }
        if self.pending.as_ref().is_some_and(|pending| {
            !is_delta(&pending.event)
                || pending.source_events == MAX_COALESCED_EVENTS
                || pending.payload_bytes >= self.max_coalesced_bytes
        }) {
            self.flush().await?;
        }
        Ok(())
    }

    fn try_reserve(&self, bytes: u32) -> Option<(OwnedSemaphorePermit, OwnedSemaphorePermit)> {
        let events = self.event_budget.clone().try_acquire_owned().ok()?;
        let bytes = self
            .byte_budget
            .clone()
            .try_acquire_many_owned(bytes)
            .ok()?;
        Some((events, bytes))
    }

    pub(super) async fn flush(&mut self) -> Result<(), ()> {
        if let Some(event) = self.pending.take() {
            self.requests
                .send(WriteRequest::Event(event))
                .await
                .map_err(|_| ())?;
        }
        Ok(())
    }

    pub(super) async fn flush_usage(&mut self) -> Result<(), ()> {
        self.flush().await?;
        self.requests
            .send(WriteRequest::FlushUsage)
            .await
            .map_err(|_| ())
    }
}

#[cfg(test)]
#[path = "coalescing_tests.rs"]
mod tests;
