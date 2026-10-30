//! Bound both the original event count and bytes, even when reasoning fragments are merged.

use std::{collections::BTreeMap, io, sync::Arc, time::Duration};

use event_bus::{Event, EventKind, MessageEvent};
use tokio::{
    sync::{OwnedSemaphorePermit, Semaphore, mpsc},
    time::Instant,
};

use super::{
    PersistencePolicy, StorageBridgeMonitor, WriteRequest, monitor::PendingRegistration,
    run_order::event_runs,
};

pub(super) const WRITE_QUEUE_CAPACITY: usize = 16_384;
const WRITE_QUEUE_BYTES: usize = 64 * 1024 * 1024;
/// How often the bridge checks pending reasoning for expiry.
pub(super) const COALESCE_INTERVAL: Duration = Duration::from_millis(100);
/// A reasoning row is written no later than this after its first fragment.
pub(super) const MAX_COALESCE_AGE: Duration = Duration::from_secs(5);
/// Payload bytes per merged row, further capped by the storage event-size limit.
pub(super) const MAX_COALESCED_BYTES: usize = 64 * 1024;

pub(super) struct QueuedEvent {
    pub(super) event: Box<Event>,
    payload_bytes: usize,
    // Decrement observations before releasing capacity to a waiting producer.
    registration: PendingRegistration,
    event_permit: OwnedSemaphorePermit,
    byte_permit: OwnedSemaphorePermit,
}

impl QueuedEvent {
    fn can_merge(&self, next: &Event, max_payload_bytes: usize) -> bool {
        self.event.meta.schema_version == next.meta.schema_version
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
            EventKind::Message(MessageEvent::ReasoningDelta { delta: text, .. }) => {
                text.push_str(delta);
            }
            _ => unreachable!("only reasoning deltas can coalesce"),
        }
        self.event_permit.merge(other.event_permit);
        self.byte_permit.merge(other.byte_permit);
        self.registration.merge(other.registration);
    }
}

/// One run's accumulating reasoning row.
struct PendingReasoning {
    queued: QueuedEvent,
    since: Instant,
    /// Arrival order of the row's first fragment; rows are written in this order.
    order: u64,
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
    let (a, _) = reasoning_delta(first)?;
    let (b, delta) = reasoning_delta(next)?;
    (a == b).then_some(delta)
}

/// The run and text of an attributable reasoning fragment, the only event kind
/// that coalesces. Unattributed fragments cannot be ordered against a run's
/// boundaries, so they are written as they arrive.
fn reasoning_delta(event: &Event) -> Option<(&str, &str)> {
    match &event.kind {
        EventKind::Message(MessageEvent::ReasoningDelta {
            run_id: Some(run),
            delta,
        }) => Some((run, delta)),
        _ => None,
    }
}

// Width of one character inside a JSON string literal.
fn json_char_len(c: char) -> usize {
    match c {
        '"' | '\\' | '\u{8}' | '\u{c}' | '\n' | '\r' | '\t' => 2,
        c if (c as u32) < 0x20 => 6,
        c => c.len_utf8(),
    }
}

/// Splits `text` on character boundaries so each piece's JSON-escaped length is
/// at most `room`. `None` when a single character does not fit.
fn split_json_bounded(text: &str, room: usize) -> Option<Vec<&str>> {
    let mut pieces = Vec::new();
    let (mut start, mut used) = (0, 0);
    for (index, c) in text.char_indices() {
        let width = json_char_len(c);
        if width > room {
            return None;
        }
        if used + width > room {
            pieces.push(&text[start..index]);
            (start, used) = (index, 0);
        }
        used += width;
    }
    pieces.push(&text[start..]);
    Some(pieces)
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
    /// Reasoning rows still accumulating, one per run.
    pending: BTreeMap<String, PendingReasoning>,
    next_order: u64,
    event_budget: Arc<Semaphore>,
    byte_budget: Arc<Semaphore>,
    monitor: StorageBridgeMonitor,
    policy: PersistencePolicy,
    max_coalesced_bytes: usize,
    max_event_bytes: usize,
    redactor: secret_guard::SecretRedactor,
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
            pending: BTreeMap::new(),
            next_order: 0,
            event_budget: Arc::new(Semaphore::new(WRITE_QUEUE_CAPACITY)),
            byte_budget: Arc::new(Semaphore::new(WRITE_QUEUE_BYTES)),
            monitor,
            policy,
            max_coalesced_bytes: max_event_bytes.min(MAX_COALESCED_BYTES as u64) as usize,
            max_event_bytes: usize::try_from(max_event_bytes).unwrap_or(usize::MAX),
            redactor: secret_guard::SecretRedactor::from_env(),
        }
    }

    pub(super) async fn push(&mut self, event: Event) -> Result<(), ()> {
        // The retry replays the response from its start. The marker is not
        // stored, but the failed attempt's reasoning must end its own row.
        if let EventKind::Message(MessageEvent::StreamRetried { run_id: Some(run) }) = &event.kind {
            self.flush_run(run).await?;
        }
        if self.policy.skip(&event, &self.monitor) {
            return Ok(());
        }
        match split_oversize(&event, self.max_event_bytes, &self.redactor) {
            Some(pieces) => {
                for piece in pieces {
                    self.push_stored(piece).await?;
                }
                Ok(())
            }
            None => self.push_stored(event).await,
        }
    }

    /// An event the policy keeps is either queued for the writer or merged into
    /// its run's accumulating reasoning row.
    async fn push_stored(&mut self, event: Event) -> Result<(), ()> {
        let delta_run = reasoning_delta(&event).map(|(run, _)| run.to_owned());
        match &delta_run {
            Some(run) => {
                if self.pending.get(run).is_some_and(|pending| {
                    !pending.queued.can_merge(&event, self.max_coalesced_bytes)
                }) {
                    self.flush_run(run).await?;
                }
            }
            // Every other event is a boundary for the runs it belongs to: their
            // reasoning is written first so row order matches live order. Other
            // runs keep accumulating.
            None => self.flush_boundary_runs(&event).await?,
        }
        let serialized_bytes = json_len(&event);
        // A single over-budget event is admitted exclusively and then subjected
        // to the existing storage event-size limit; it cannot share the queue.
        let bytes = serialized_bytes.min(WRITE_QUEUE_BYTES) as u32;
        let permits = match self.try_reserve(bytes) {
            Some(permits) => permits,
            None => {
                // Pending reasoning owns permits too. Send it before waiting so
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
            payload_bytes,
            event_permit: permits.0,
            byte_permit: permits.1,
            registration: self.monitor.register(serialized_bytes),
        };
        let Some(run) = delta_run else {
            return self.send(next).await;
        };
        if let Some(pending) = self.pending.get_mut(&run) {
            pending.queued.merge(next);
            self.monitor.coalesced();
        } else {
            self.pending.insert(
                run.clone(),
                PendingReasoning {
                    queued: next,
                    since: Instant::now(),
                    order: self.next_order,
                },
            );
            self.next_order += 1;
        }
        if self
            .pending
            .get(&run)
            .is_some_and(|pending| pending.queued.payload_bytes >= self.max_coalesced_bytes)
        {
            self.flush_run(&run).await?;
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

    async fn send(&self, event: QueuedEvent) -> Result<(), ()> {
        self.requests
            .send(WriteRequest::Event(event))
            .await
            .map_err(|_| ())
    }

    async fn flush_boundary_runs(&mut self, event: &Event) -> Result<(), ()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let runs = event_runs(event);
        // An unattributed event can still be visible (for example an approval
        // or fault). Preserve its position relative to all pending reasoning.
        if runs.is_empty() {
            return self.flush().await;
        }
        for run in runs {
            self.flush_run(run).await?;
        }
        Ok(())
    }

    pub(super) async fn barrier(&mut self, reply: super::FlushReply) -> Result<(), ()> {
        self.flush().await?;
        self.requests
            .send(WriteRequest::Barrier(reply))
            .await
            .map_err(|_| ())
    }

    pub(super) async fn flush_usage(&mut self) -> Result<(), ()> {
        self.flush().await?;
        self.requests
            .send(WriteRequest::FlushUsage)
            .await
            .map_err(|_| ())
    }

    async fn flush_run(&mut self, run: &str) -> Result<(), ()> {
        match self.pending.remove(run) {
            Some(pending) => self.send(pending.queued).await,
            None => Ok(()),
        }
    }

    /// Write every accumulating row, oldest first.
    pub(super) async fn flush(&mut self) -> Result<(), ()> {
        let mut rows: Vec<_> = std::mem::take(&mut self.pending).into_values().collect();
        rows.sort_by_key(|row| row.order);
        for row in rows {
            self.send(row.queued).await?;
        }
        Ok(())
    }

    /// Write rows whose first fragment is at least [`MAX_COALESCE_AGE`] old, so
    /// a quiet run's reasoning reaches storage without waiting for its boundary.
    pub(super) async fn flush_expired(&mut self) -> Result<(), ()> {
        let now = Instant::now();
        let mut expired: Vec<_> = self
            .pending
            .iter()
            .filter(|(_, row)| now.saturating_duration_since(row.since) >= MAX_COALESCE_AGE)
            .map(|(run, row)| (row.order, run.clone()))
            .collect();
        expired.sort();
        for (_, run) in expired {
            self.flush_run(&run).await?;
        }
        Ok(())
    }
}

/// Keeps the storage hard limit without dropping oversized accepted responses.
/// The whole text is scanned first: splitting must not hide a secret across
/// row boundaries. A hit stays whole for storage's ordinary fail-closed check.
pub(super) fn split_oversize(
    event: &Event,
    max_event_bytes: usize,
    redactor: &secret_guard::SecretRedactor,
) -> Option<Vec<Event>> {
    let (run_id, text, limit, complete) = match &event.kind {
        EventKind::Message(MessageEvent::ReasoningDelta { run_id, delta }) => (
            run_id.as_deref(),
            delta,
            max_event_bytes.min(MAX_COALESCED_BYTES),
            false,
        ),
        EventKind::Message(MessageEvent::MessageCompleted { run_id, text }) => {
            (Some(run_id.as_str()), text, max_event_bytes, true)
        }
        _ => return None,
    };
    if json_len(&event.kind) <= limit || redactor.detect(text).is_some() {
        return None;
    }
    let response_id = uuid::Uuid::new_v4().to_string();
    let row = |text: &str, index: usize, total: usize| {
        EventKind::Message(if complete {
            MessageEvent::MessagePart {
                run_id: run_id.expect("completed response has a run").to_owned(),
                response_id: response_id.clone(),
                text: text.to_owned(),
                index,
                total,
            }
        } else {
            MessageEvent::ReasoningDelta {
                run_id: run_id.map(str::to_owned),
                delta: text.to_owned(),
            }
        })
    };
    // Neither index nor total can exceed the original byte length.
    let room = limit.checked_sub(json_len(&row("", text.len(), text.len())))?;
    let pieces = split_json_bounded(text, room)?;
    let count = pieces.len();
    Some(
        pieces
            .into_iter()
            .enumerate()
            .map(|(index, piece)| Event {
                meta: event.meta.clone(),
                kind: row(piece, index, count),
            })
            .collect(),
    )
}

#[cfg(test)]
#[path = "coalescing_tests.rs"]
mod tests;
