//! Count admitted TCP connection lifetimes, including HTTP keep-alive and SSE.
use crate::{ActivityObserver, shutdown::ShutdownSignal};
use std::sync::{Arc, Mutex};

#[derive(Clone, Default)]
pub(super) struct Clients {
    admission: Arc<Mutex<()>>,
    activity: ActivityObserver,
}

impl Clients {
    pub(super) fn subscribe(&self) -> ActivityObserver {
        self.activity.clone()
    }

    pub(super) fn connect(&self, shutdown: &ShutdownSignal) -> Option<Connected> {
        let _admission = self
            .admission
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if shutdown.is_triggered() {
            return None;
        }
        // The copied observer records complete connect/disconnect pairs without
        // exposing any guard needed by connection Drop or admission.
        self.activity.started();
        Some(Connected(self.clone()))
    }

    fn close_if_empty(&self, shutdown: &ShutdownSignal, is_idle: impl FnOnce() -> bool) -> bool {
        let _admission = self
            .admission
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if self.activity.snapshot().0 == 0 && is_idle() {
            // A concurrent accept either retains the server or is refused after
            // this cutoff. Snapshot/notification never holds this private lock.
            shutdown.trigger();
            true
        } else {
            false
        }
    }
}

impl super::WebServer {
    /// Begin an idle shutdown only if no admitted connection exists.
    pub fn try_begin_idle_shutdown(&self) -> bool {
        self.try_begin_idle_shutdown_if(|| true)
    }

    /// Evaluate other retention reasons while new connection admission is fenced.
    pub fn try_begin_idle_shutdown_if(&self, is_idle: impl FnOnce() -> bool) -> bool {
        self.clients.close_if_empty(&self.shutdown, || {
            let (active, unconfirmed, _) = self.tasks.activity().snapshot();
            active == 0 && !unconfirmed && is_idle()
        })
    }

    /// Accepted commands may outlive their HTTP connection.
    pub fn accepted_activity(&self) -> ActivityObserver {
        self.tasks.activity()
    }
}

pub(super) struct Connected(Clients);

impl Drop for Connected {
    fn drop(&mut self) {
        self.0.activity.completed(true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coalesced_connections_still_notify_and_idle_cutoff_fences_admission() {
        let clients = Clients::default();
        let census = clients.subscribe();
        let initial = census.snapshot();
        let shutdown = ShutdownSignal::new();
        let connection = clients.connect(&shutdown).unwrap();
        assert_eq!(census.snapshot().0, 1);
        assert!(!clients.close_if_empty(&shutdown, || true));
        assert!(!shutdown.is_triggered());
        drop(connection);
        let after = census.snapshot();
        assert_eq!((after.0, after.1), (0, false));
        assert_ne!(after.2, initial.2, "zero again is still activity");
        assert!(clients.close_if_empty(&shutdown, || true));
        assert!(shutdown.is_triggered());
        assert!(clients.connect(&shutdown).is_none());
        assert_eq!(census.snapshot().0, 0);
    }

    #[tokio::test]
    async fn cancellation_drops_the_connection_guard_even_before_its_first_poll() {
        let clients = Clients::default();
        let census = clients.subscribe();
        let connection = clients.connect(&ShutdownSignal::new()).unwrap();
        let task = tokio::spawn(async move {
            let _connection = connection;
            std::future::pending::<()>().await;
        });
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert_eq!(census.snapshot().0, 0);
    }

    #[test]
    fn held_census_observation_cannot_block_connection_drop() {
        use std::{
            future::Future,
            task::{Context, Waker},
        };
        let clients = Clients::default();
        let census = clients.subscribe();
        let connection = clients.connect(&ShutdownSignal::new()).unwrap();
        let copied = census.snapshot();
        let mut changed = Box::pin(census.changed(copied.2));
        let mut context = Context::from_waker(Waker::noop());
        assert!(changed.as_mut().poll(&mut context).is_pending());
        std::thread::spawn(move || drop(connection)).join().unwrap();
        assert_eq!(copied.0, 1);
        assert!(changed.as_mut().poll(&mut context).is_ready());
        assert_eq!(census.snapshot().0, 0);
    }
}
