//! In-memory diagnostics; observing the writer never adds storage traffic.

use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

#[derive(Default)]
struct State {
    pending_events: usize,
    pending_bytes: usize,
    oldest: BTreeMap<u64, Instant>,
    next_id: u64,
    last_warning: Option<Instant>,
}

#[derive(Default)]
struct Inner {
    state: Mutex<State>,
    peak_pending_events: AtomicUsize,
    peak_pending_bytes: AtomicUsize,
    persisted_events: AtomicU64,
    coalesced_events: AtomicU64,
    skipped_heartbeats: AtomicU64,
    skipped_diagnostics: AtomicU64,
    failed_events: AtomicU64,
    #[cfg(test)]
    changes: TestChanges,
}

#[cfg(test)]
struct TestChanges(tokio::sync::watch::Sender<()>);

#[cfg(test)]
impl Default for TestChanges {
    fn default() -> Self {
        Self(tokio::sync::watch::channel(()).0)
    }
}

/// Memory-only health counters for one GUI storage bridge.
#[derive(Clone, Default)]
pub struct StorageBridgeMonitor(Arc<Inner>);

/// A point-in-time view, including the current write and the delta accumulator.
#[derive(Debug, Default, Clone)]
pub struct StorageBridgeSnapshot {
    /// Original events accepted but not yet completed, including coalesced deltas.
    pub pending_events: usize,
    /// Sum of original serialized sizes (conservative after coalescing).
    /// One event exceeding the byte budget is admitted exclusively and then
    /// checked against the storage event-size limit.
    pub pending_bytes: usize,
    pub peak_pending_events: usize,
    pub peak_pending_bytes: usize,
    pub oldest_pending_age: Duration,
    /// Successfully persisted rows, excluding usage buckets and skipped events.
    pub persisted_events: u64,
    /// Original deltas merged into an earlier delta rather than stored separately.
    pub coalesced_events: u64,
    pub skipped_heartbeats: u64,
    pub skipped_diagnostics: u64,
    /// Failed storage requests, including stale-generation rejection.
    pub failed_events: u64,
}

impl StorageBridgeMonitor {
    /// Only tests subscribe to these changes; production adds no notification
    /// work or synchronization to the persistence path.
    #[cfg(test)]
    pub(super) async fn wait_for(&self, ready: impl Fn(&StorageBridgeSnapshot) -> bool) {
        let mut changes = self.0.changes.0.subscribe();
        loop {
            if ready(&self.snapshot()) {
                return;
            }
            changes.changed().await.unwrap();
        }
    }

    #[cfg(test)]
    pub(super) fn wait_for_blocking(&self, ready: impl Fn(&StorageBridgeSnapshot) -> bool) {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(self.wait_for(ready));
    }

    pub fn snapshot(&self) -> StorageBridgeSnapshot {
        let state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        StorageBridgeSnapshot {
            pending_events: state.pending_events,
            pending_bytes: state.pending_bytes,
            peak_pending_events: self.0.peak_pending_events.load(Ordering::Relaxed),
            peak_pending_bytes: self.0.peak_pending_bytes.load(Ordering::Relaxed),
            oldest_pending_age: state
                .oldest
                .first_key_value()
                .map_or(Duration::ZERO, |(_, age)| age.elapsed()),
            persisted_events: self.0.persisted_events.load(Ordering::Relaxed),
            coalesced_events: self.0.coalesced_events.load(Ordering::Relaxed),
            skipped_heartbeats: self.0.skipped_heartbeats.load(Ordering::Relaxed),
            skipped_diagnostics: self.0.skipped_diagnostics.load(Ordering::Relaxed),
            failed_events: self.0.failed_events.load(Ordering::Relaxed),
        }
    }

    pub(super) fn persisted(&self) {
        self.0.persisted_events.fetch_add(1, Ordering::Relaxed);
        #[cfg(test)]
        self.0.changes.0.send_replace(());
    }

    pub(super) fn failed(&self) {
        self.0.failed_events.fetch_add(1, Ordering::Relaxed);
        #[cfg(test)]
        self.0.changes.0.send_replace(());
    }

    pub(super) fn skipped_heartbeat(&self) {
        self.0.skipped_heartbeats.fetch_add(1, Ordering::Relaxed);
    }

    pub(super) fn skipped_diagnostic(&self) {
        self.0.skipped_diagnostics.fetch_add(1, Ordering::Relaxed);
    }

    pub(super) fn coalesced(&self) {
        self.0.coalesced_events.fetch_add(1, Ordering::Relaxed);
    }

    pub(super) fn register(&self, bytes: usize) -> PendingRegistration {
        let mut state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        let id = state.next_id;
        state.next_id += 1;
        state.oldest.insert(id, Instant::now());
        state.pending_events += 1;
        state.pending_bytes += bytes;
        self.0
            .peak_pending_events
            .fetch_max(state.pending_events, Ordering::Relaxed);
        self.0
            .peak_pending_bytes
            .fetch_max(state.pending_bytes, Ordering::Relaxed);
        #[cfg(test)]
        self.0.changes.0.send_replace(());
        PendingRegistration {
            monitor: self.clone(),
            id,
            count: 1,
            bytes,
        }
    }

    pub(super) fn warn_if_backlogged(&self) {
        let snapshot = self.snapshot();
        if snapshot.oldest_pending_age >= Duration::from_secs(5) {
            self.warn(&format!(
                "storage backlog: {} events / {} bytes; oldest pending {:.1}s",
                snapshot.pending_events,
                snapshot.pending_bytes,
                snapshot.oldest_pending_age.as_secs_f64(),
            ));
        }
    }

    // Bypass tracing/EventBus so a failed persistence path cannot persist its own
    // warnings. All bridge warnings share a one-minute rate limit.
    pub(super) fn warn(&self, message: &str) {
        let mut state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        if state
            .last_warning
            .is_some_and(|at| at.elapsed() < Duration::from_secs(60))
        {
            return;
        }
        state.last_warning = Some(Instant::now());
        drop(state);
        use std::io::Write;
        // A closed stderr pipe must not terminate the persistence worker.
        let _ = writeln!(std::io::stderr().lock(), "evorch: {message}");
    }
}

pub(super) struct PendingRegistration {
    monitor: StorageBridgeMonitor,
    id: u64,
    count: usize,
    bytes: usize,
}

impl PendingRegistration {
    pub(super) fn merge(&mut self, mut other: Self) {
        self.count += other.count;
        self.bytes += other.bytes;
        let mut state = self
            .monitor
            .0
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        state.oldest.remove(&other.id);
        other.count = 0;
        other.bytes = 0;
    }
}

impl Drop for PendingRegistration {
    fn drop(&mut self) {
        if self.count == 0 {
            return;
        }
        let mut state = self
            .monitor
            .0
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        state.oldest.remove(&self.id);
        state.pending_events -= self.count;
        state.pending_bytes -= self.bytes;
        #[cfg(test)]
        self.monitor.0.changes.0.send_replace(());
    }
}
