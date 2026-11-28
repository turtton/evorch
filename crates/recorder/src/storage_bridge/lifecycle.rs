//! Own the bridge thread and writer together so all shutdown paths drain first.

use std::{io, sync::Arc, thread::JoinHandle, time::Duration};

use event_bus::EventBus;
use storage::{Storage, StorageHandle};
use tokio::sync::oneshot;

use super::{FlushReply, StorageBridge, StorageBridgeMonitor, run_subscribed};

/// Owns durable storage and its event bridge. Dropping this guard signals a
/// finite drain, joins the bridge, and only then lets the SQLite writer close.
/// This also covers startup errors and window creation failures.
pub struct OwnedStorageBridge {
    storage: Storage,
    shutdown: Option<oneshot::Sender<()>>,
    flush_requests: tokio::sync::mpsc::UnboundedSender<FlushReply>,
    thread: Option<JoinHandle<Result<(), String>>>,
    monitor: StorageBridgeMonitor,
    stopped_result: Option<Result<(), String>>,
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
        let (flush_requests, flush_receiver) = tokio::sync::mpsc::unbounded_channel();
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
                        flush_receiver,
                    ))
                })
            })?;
        Ok(Self {
            storage,
            shutdown: Some(shutdown),
            flush_requests,
            thread: Some(thread),
            monitor,
            stopped_result: None,
        })
    }

    pub fn handle(&self) -> StorageHandle {
        self.storage.handle()
    }

    pub fn monitor(&self) -> StorageBridgeMonitor {
        self.monitor.clone()
    }

    /// Persist the accepted queue and a finite bus snapshot without stopping
    /// the subscriber. Quiesce producers first, then call this; events emitted
    /// afterwards can still reach the final shutdown snapshot. Reports the first
    /// write failure since the preceding barrier.
    /// This synchronous method waits for SQLite writes and usage flush to finish.
    pub fn flush(&mut self) -> io::Result<()> {
        tracing::debug!(target: "restart_drain", phase = "flush_begin", pending = self.monitor.snapshot().pending_events);
        let (reply, completed) = std::sync::mpsc::channel();
        self.flush_requests
            .send(reply)
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "storage bridge is closed"))?;
        let result = completed
            .recv()
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "storage bridge flush was interrupted",
                )
            })?
            .map_err(io::Error::other);
        tracing::debug!(target: "restart_drain", phase = "flush_ack", ok = result.is_ok());
        result
    }

    /// Drain accepted events and the snapshot pending when the stop is observed,
    /// even with live producers. Later arrivals are not persisted by this bridge.
    /// Returns after durable writes complete; may wait for storage I/O. Idempotent.
    pub fn shutdown(&mut self) {
        // A write that failed was already warned about where it failed; only a
        // thread that never ended cleanly adds a warning here.
        if self.stop().is_err() {
            self.monitor
                .warn("storage bridge thread failed during shutdown");
        }
    }

    /// [`shutdown`](Self::shutdown) that says whether the stop was clean. A
    /// restart must not go on after an `Err`: accepted events may be unwritten.
    ///
    /// # Errors
    /// The bridge thread panicked, an event or the usage metrics failed to be
    /// written since the last [`flush`](Self::flush), the final flush could not
    /// reach the writer. This only stops/joins the subscriber; storage and its
    /// writer remain owned and alive for Arm/readback/FailedClosed.
    pub fn try_shutdown(&mut self) -> io::Result<()> {
        match self.stop() {
            Ok(Ok(())) => Ok(()),
            Ok(Err(failure)) => Err(io::Error::other(failure)),
            Err(()) => Err(io::Error::other(
                "the storage bridge thread panicked during shutdown",
            )),
        }
    }

    /// Stop and join the thread. `Err(())`: it panicked. `Ok(Err(..))`: it
    /// ended with the failure it recorded.
    fn stop(&mut self) -> Result<Result<(), String>, ()> {
        if let Some(result) = &self.stopped_result {
            return Ok(result.clone());
        }
        if self.thread.is_some() {
            tracing::debug!(target: "restart_drain", phase = "subscriber_join_begin");
        }
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        let Some(thread) = self.thread.take() else {
            return Ok(Ok(()));
        };
        let joined = thread.join();
        tracing::debug!(target: "restart_drain", phase = "subscriber_join_end", ok = joined.as_ref().is_ok_and(|r| r.is_ok()));
        match joined {
            Ok(result) => {
                self.stopped_result = Some(result.clone());
                Ok(result)
            }
            Err(_) => {
                self.stopped_result = Some(Err("storage bridge thread panicked".into()));
                Err(())
            }
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
