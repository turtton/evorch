//! The snapshot revision one in-memory run context last committed or read.
//!
//! Saving passes it as the compare-and-swap precondition, so a context that
//! another writer has replaced since cannot overwrite the newer snapshot. The
//! value belongs to one context: it is never shared between two handles of the
//! same run, and never read back from storage just before a save.
use std::sync::atomic::{AtomicU64, Ordering};

const UNKNOWN: u64 = u64::MAX;

/// Interior mutability lets a save through `&LoopState` record its commit.
#[derive(Debug)]
pub(crate) struct SnapshotRevision(AtomicU64);

impl SnapshotRevision {
    /// A run ID that has no snapshot yet.
    pub(crate) const fn fresh() -> Self {
        Self(AtomicU64::new(0))
    }

    /// The revision of the snapshot this context was read from or just saved.
    pub(crate) const fn known(revision: u64) -> Self {
        Self(AtomicU64::new(revision))
    }

    /// The context was not read together with its revision. Saving it fails
    /// instead of guessing, so it can never replace a newer snapshot.
    pub(crate) const fn unknown() -> Self {
        Self(AtomicU64::new(UNKNOWN))
    }

    pub(crate) fn get(&self) -> Option<u64> {
        match self.0.load(Ordering::Acquire) {
            UNKNOWN => None,
            revision => Some(revision),
        }
    }

    /// Record the revision a transaction committed. Never call it for a failed save.
    pub(crate) fn commit(&self, revision: u64) {
        self.0.store(revision, Ordering::Release);
    }
}

impl Clone for SnapshotRevision {
    fn clone(&self) -> Self {
        Self(AtomicU64::new(self.0.load(Ordering::Acquire)))
    }
}

impl PartialEq for SnapshotRevision {
    fn eq(&self, other: &Self) -> bool {
        self.get() == other.get()
    }
}

/// Another writer's save of the revision a test has just read, so that the
/// code under test loses its compare-and-swap deterministically.
#[cfg(test)]
pub(crate) mod conflict_support {
    use storage::thread_index::SnapshotBundle;

    /// Save `bundle` again with a later timestamp; the stored revision is `bundle`'s + 1.
    pub(crate) fn replace_snapshot(handle: &storage::StorageHandle, bundle: &SnapshotBundle) {
        let mut record = bundle.record.clone();
        record.updated_at_ns += 1;
        let write = crate::restore::provenance::ConversationProvenance::from_bundle(bundle)
            .write(record, bundle.snapshot_revision)
            .unwrap();
        handle.save_snapshot_bundle(&write).unwrap();
    }

    /// Events emitted so far: everything up to a sentinel published afterwards.
    pub(crate) async fn drain(
        bus: &event_bus::EventBus,
        subscriber: &mut event_bus::EventReceiver,
    ) -> Vec<event_bus::Event> {
        let sentinel = "snapshot-conflict-sentinel";
        bus.emit(event_bus::Event::new(
            event_bus::MessageEvent::MessageDelta {
                delta: sentinel.into(),
                run_id: None,
            },
        ));
        let mut seen = Vec::new();
        loop {
            let event = subscriber.recv().await.unwrap();
            if matches!(&event.kind, event_bus::EventKind::Message(
                event_bus::MessageEvent::MessageDelta { delta, .. }) if delta == sentinel)
            {
                return seen;
            }
            seen.push(event);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::SnapshotRevision;

    #[test]
    fn clones_track_their_own_commits() {
        let first = SnapshotRevision::known(3);
        let second = first.clone();
        first.commit(4);
        assert_eq!(first.get(), Some(4));
        assert_eq!(second.get(), Some(3), "a clone is an independent handle");
    }

    #[test]
    fn unknown_is_distinct_from_fresh() {
        assert_eq!(SnapshotRevision::fresh().get(), Some(0));
        assert_eq!(SnapshotRevision::unknown().get(), None);
    }
}
