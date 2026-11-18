//! The explicit core process, not an implicitly spawned daemon, owns indexing.
use storage::{
    StorageError, StorageHandle,
    thread_index::{IndexOwner, IndexOwnerKind},
};

pub(super) struct ThreadIndexOwner {
    writer: StorageHandle,
    stopped: bool,
    changed: std::sync::Arc<tokio::sync::Notify>,
    _observer: std::sync::Arc<dyn Fn() + Send + Sync>,
}
impl ThreadIndexOwner {
    pub(super) fn start(writer: StorageHandle) -> Result<Self, StorageError> {
        let changed = std::sync::Arc::new(tokio::sync::Notify::new());
        let notify = changed.clone();
        let observer: std::sync::Arc<dyn Fn() + Send + Sync> =
            std::sync::Arc::new(move || notify.notify_one());
        writer.observe_pending_reindex(&observer)?;
        // v27 accepts only gui/cli. Distinguish the core by its unique identity,
        // not a new schema kind; the legacy GUI still competes for this lease.
        let owner = Self {
            writer,
            stopped: false,
            changed,
            _observer: observer,
        };
        owner.writer.start_thread_index_owner(IndexOwner {
            id: format!("core-{}-{}", std::process::id(), uuid::Uuid::new_v4()),
            kind: IndexOwnerKind::Cli,
        })?;
        Ok(owner)
    }
    pub(super) fn changed(&self) -> std::sync::Arc<tokio::sync::Notify> {
        self.changed.clone()
    }
    pub(super) fn stop(&mut self) -> Result<(), StorageError> {
        if !self.stopped {
            self.writer.stop_thread_index_owner()?;
            self.stopped = true;
        }
        Ok(())
    }
}
impl Drop for ThreadIndexOwner {
    fn drop(&mut self) {
        // Also release on bootstrap errors; explicit shutdown reports failures.
        if let Err(error) = self.stop() {
            tracing::warn!(%error, "thread index owner could not be stopped");
        }
    }
}

#[cfg(test)]
mod tests;
