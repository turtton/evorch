//! Cooperative shutdown shared by the server and every SSE stream.

use std::sync::Arc;
use tokio::sync::watch;

/// Fires once. Cloning shares the same signal.
#[derive(Clone, Debug)]
pub struct ShutdownSignal {
    tx: Arc<watch::Sender<bool>>,
}

impl ShutdownSignal {
    pub(crate) fn new() -> Self {
        Self {
            tx: Arc::new(watch::channel(false).0),
        }
    }

    /// Stop accepting work and end every SSE stream. Idempotent.
    pub fn trigger(&self) {
        self.tx.send_replace(true);
    }

    pub fn is_triggered(&self) -> bool {
        *self.tx.borrow()
    }

    /// Resolves once [`Self::trigger`] has been called, also if it already was.
    pub(crate) async fn triggered(&self) {
        let mut rx = self.tx.subscribe();
        // The sender lives in `self`, so the channel cannot close here.
        let _ = rx.wait_for(|fired| *fired).await;
    }
}
