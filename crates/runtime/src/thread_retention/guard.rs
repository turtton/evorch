//! Refusal of work on threads and runs whose deletion was confirmed.
//!
//! Startup installs one shared view. Online commits extend it before admission
//! reopens. A refusal here is the early, cheap answer; the storage
//! writer's fence ([`storage::StorageError::Tombstoned`]) is the last line.

use std::sync::{Arc, RwLock};

use storage::thread_retention::TombstoneSet;

/// The identity a refused operation named.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ThreadDeleted {
    #[error("スレッド {0} は保持期間を過ぎて削除されました")]
    Thread(String),
    #[error("run {0} は保持期間を過ぎて削除されたスレッドに属します")]
    Run(String),
}

/// The shared live tombstone view, published by the writer before releasing its seal.
#[derive(Debug, Clone, Default)]
pub struct TombstoneGuard(Arc<RwLock<TombstoneSet>>);

impl TombstoneGuard {
    #[must_use]
    pub fn new(tombstones: TombstoneSet) -> Self {
        Self(Arc::new(RwLock::new(tombstones)))
    }

    /// # Errors
    /// [`ThreadDeleted::Thread`] when `thread_id` was deleted.
    pub fn check_thread(&self, thread_id: &str) -> Result<(), ThreadDeleted> {
        if self
            .0
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .contains_thread(thread_id)
        {
            return Err(ThreadDeleted::Thread(thread_id.to_owned()));
        }
        Ok(())
    }

    /// # Errors
    /// [`ThreadDeleted::Run`] when `run_id` belonged to a deleted family.
    pub fn check_run(&self, run_id: &str) -> Result<(), ThreadDeleted> {
        if self
            .0
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .contains_run(run_id)
        {
            return Err(ThreadDeleted::Run(run_id.to_owned()));
        }
        Ok(())
    }

    #[must_use]
    pub fn tombstones(&self) -> TombstoneSet {
        self.0.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Memory-only, infallible publication after SQL commit, before admission reopens.
    pub(crate) fn publish(&self, family: &storage::thread_retention::ConfirmedFamily) {
        let mut set = self.0.write().unwrap_or_else(|e| e.into_inner());
        set.threads.extend(family.threads.iter().cloned());
        set.runs.extend(family.runs.iter().cloned());
        set.tasks.extend(family.tasks.iter().cloned());
    }
}

impl crate::AgentRuntime {
    /// Refuse new work on the deleted threads and runs in `guard` from now on. The set
    /// is updated by online commits; its shared instance can be installed only once.
    ///
    /// # Errors
    /// Returns the guard when one is already installed.
    pub fn set_tombstone_guard(&self, guard: TombstoneGuard) -> Result<(), TombstoneGuard> {
        self.shared.tombstones.set(guard.clone())?;
        self.shared.retention_gate.set_guard(guard);
        Ok(())
    }

    /// Read-only access to the same mutable guard installed at bootstrap.
    pub fn tombstone_guard(&self) -> Option<TombstoneGuard> {
        self.shared.tombstones.get().cloned()
    }

    /// # Errors
    /// [`ThreadDeleted::Thread`] when retention deleted `thread_id`.
    pub(crate) fn deny_deleted_thread(&self, thread_id: &str) -> Result<(), ThreadDeleted> {
        self.shared
            .tombstones
            .get()
            .map_or(Ok(()), |guard| guard.check_thread(thread_id))
    }

    /// # Errors
    /// [`ThreadDeleted::Run`] when `run_id` belonged to a deleted family.
    pub(crate) fn deny_deleted_run(&self, run_id: &str) -> Result<(), ThreadDeleted> {
        self.shared
            .tombstones
            .get()
            .map_or(Ok(()), |guard| guard.check_run(run_id))
    }
}
