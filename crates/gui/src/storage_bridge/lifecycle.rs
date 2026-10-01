//! Own the bridge thread and writer together so all shutdown paths drain first.

use std::{io, sync::Arc, thread::JoinHandle, time::Duration};

use event_bus::EventBus;
use storage::{Storage, StorageHandle};
use tokio::sync::oneshot;

use super::{StorageBridge, StorageBridgeMonitor, run_subscribed};

/// Owns durable storage and its GUI event bridge. Dropping this guard signals a
/// finite drain, joins the bridge, and only then lets the SQLite writer close.
/// This also covers startup errors and window creation failures.
pub struct OwnedStorageBridge {
    storage: Storage,
    shutdown: Option<oneshot::Sender<()>>,
    thread: Option<JoinHandle<()>>,
    monitor: StorageBridgeMonitor,
}

impl OwnedStorageBridge {
    /// Start an independently owned runtime and subscribe before returning, so
    /// callers may emit immediately. `configure` sets persistence policies.
    pub fn spawn(
        bus: Arc<EventBus>,
        storage: Storage,
        configure: impl FnOnce(StorageHandle) -> StorageBridge,
        flush_every: Duration,
    ) -> io::Result<Self> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let bridge = configure(storage.handle());
        let monitor = bridge.monitor();
        let subscriber = bus.subscribe();
        let lifetime = Arc::downgrade(&bus);
        drop(bus);
        let (shutdown, stop) = oneshot::channel();
        let dispatch = tracing::dispatcher::get_default(Clone::clone);
        let thread = std::thread::Builder::new()
            .name("evorch-storage-bridge".into())
            .spawn(move || {
                tracing::dispatcher::with_default(&dispatch, || {
                    runtime.block_on(run_subscribed(
                        subscriber,
                        lifetime,
                        bridge,
                        flush_every,
                        async {
                            let _ = stop.await;
                        },
                    ));
                });
            })?;
        Ok(Self {
            storage,
            shutdown: Some(shutdown),
            thread: Some(thread),
            monitor,
        })
    }

    pub fn handle(&self) -> StorageHandle {
        self.storage.handle()
    }

    pub fn monitor(&self) -> StorageBridgeMonitor {
        self.monitor.clone()
    }

    /// Drain accepted events and the snapshot pending when the stop is observed,
    /// even with live producers. Later arrivals are not persisted by this bridge.
    /// Returns after durable writes complete; may wait for storage I/O. Idempotent.
    pub fn shutdown(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(thread) = self.thread.take()
            && thread.join().is_err()
        {
            self.monitor
                .warn("storage bridge thread failed during shutdown");
        }
    }
}

impl Drop for OwnedStorageBridge {
    fn drop(&mut self) {
        // Storage remains alive throughout shutdown; it drops only after this.
        self.shutdown();
    }
}

#[cfg(test)]
#[path = "lifecycle_tests.rs"]
mod tests;
