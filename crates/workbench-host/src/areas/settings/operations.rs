//! The bounded table of operations the `settings` and `auth` areas keep, so a
//! client that disconnected can read an outcome later and a repeated request
//! is answered from the table instead of running twice.
use sha2::{Digest, Sha256};
use std::collections::{HashMap, VecDeque};
use workbench_protocol::errors::{ErrorCode, RetryAdvice, WorkbenchError};
use workbench_protocol::ids::OperationId;
use workbench_protocol::operation::OperationView;
use workbench_protocol::wire::Revision;

/// What the table needs of a view: the shared part every operation view holds.
pub(in crate::areas) trait Tracked: Clone {
    fn operation(&self) -> &OperationView;
    fn operation_mut(&mut self) -> &mut OperationView;
}

struct Entry<V> {
    view: V,
    /// Digest of the request without any secret, to tell a repeat from a
    /// different request under the same id.
    digest: [u8; 32],
}

/// What the table says about a request's id.
pub(in crate::areas) enum Lookup<V> {
    New,
    /// The same request was accepted before.
    Same(V),
    /// The id belongs to a different request.
    Conflict,
}

pub(in crate::areas) struct Operations<V: Tracked> {
    entries: HashMap<OperationId, Entry<V>>,
    /// Oldest first.
    order: VecDeque<OperationId>,
    capacity: usize,
    counter: u64,
    /// Bumped by every update, so a waiter sees an operation settle.
    changed: tokio::sync::watch::Sender<u64>,
}

pub(in crate::areas) fn digest_of(parts: &[&[u8]]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update((part.len() as u64).to_be_bytes());
        hasher.update(part);
    }
    hasher.finalize().into()
}

impl<V: Tracked> Operations<V> {
    pub(in crate::areas) fn new(capacity: usize) -> Self {
        Self {
            entries: HashMap::new(),
            order: VecDeque::new(),
            capacity,
            counter: 0,
            changed: tokio::sync::watch::channel(0).0,
        }
    }

    pub(in crate::areas) fn subscribe(&self) -> tokio::sync::watch::Receiver<u64> {
        self.changed.subscribe()
    }

    pub(in crate::areas) fn lookup(&self, id: &OperationId, digest: &[u8; 32]) -> Lookup<V> {
        match self.entries.get(id) {
            None => Lookup::New,
            Some(entry) if &entry.digest == digest => Lookup::Same(entry.view.clone()),
            Some(_) => Lookup::Conflict,
        }
    }

    /// The next view revision.
    pub(in crate::areas) fn next_revision(&mut self) -> Revision {
        self.counter += 1;
        Revision(self.counter)
    }

    /// Retain `view` (which must not be known), dropping the oldest finished
    /// operation when the table is full.
    ///
    /// # Errors
    /// `LimitExceeded` when every retained operation is still running.
    pub(in crate::areas) fn insert(
        &mut self,
        view: V,
        digest: [u8; 32],
    ) -> Result<(), WorkbenchError> {
        if self.entries.len() >= self.capacity {
            let finished = self.order.iter().position(|id| {
                self.entries
                    .get(id)
                    .is_none_or(|entry| entry.view.operation().state.is_final())
            });
            let Some(position) = finished else {
                return Err(WorkbenchError::new(
                    ErrorCode::LimitExceeded,
                    "too many operations are still running",
                    RetryAdvice::UserActionRequired,
                ));
            };
            if let Some(oldest) = self.order.remove(position) {
                self.entries.remove(&oldest);
            }
        }
        self.order.push_back(view.operation().operation_id.clone());
        self.entries.insert(
            view.operation().operation_id.clone(),
            Entry { view, digest },
        );
        Ok(())
    }

    pub(in crate::areas) fn forget(&mut self, id: &OperationId) {
        self.entries.remove(id);
        self.order.retain(|known| known != id);
    }

    pub(in crate::areas) fn get(&self, id: &OperationId) -> Option<&V> {
        self.entries.get(id).map(|entry| &entry.view)
    }

    /// Change the view of `id` and give it a new revision; the changed view.
    pub(in crate::areas) fn update(
        &mut self,
        id: &OperationId,
        change: impl FnOnce(&mut V),
    ) -> Option<V> {
        self.counter += 1;
        let revision = Revision(self.counter);
        let entry = self.entries.get_mut(id)?;
        change(&mut entry.view);
        entry.view.operation_mut().revision = revision;
        let view = entry.view.clone();
        self.changed
            .send_modify(|version| *version = version.wrapping_add(1));
        Some(view)
    }

    /// Newest first.
    pub(in crate::areas) fn list(&self) -> Vec<V> {
        self.order
            .iter()
            .rev()
            .filter_map(|id| self.entries.get(id).map(|entry| entry.view.clone()))
            .collect()
    }
}
