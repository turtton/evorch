//! What a core connection records about how it ended. A client that loses its
//! socket sees only "closed"; this is where the core says which of its tasks
//! stopped first and with which I/O error.
use std::{
    future::Future,
    io,
    sync::atomic::{AtomicU64, Ordering},
    time::Instant,
};
use tokio::sync::watch;

static NEXT_CONNECTION: AtomicU64 = AtomicU64::new(1);

/// Per-connection correlation: every line carries `conn`, the number of the
/// connection within this core process.
pub(crate) struct ConnectionLog {
    id: u64,
    started: Instant,
    shutdown: watch::Receiver<bool>,
}

impl ConnectionLog {
    pub(crate) fn new(shutdown: &watch::Receiver<bool>) -> Self {
        let id = NEXT_CONNECTION.fetch_add(1, Ordering::Relaxed);
        tracing::info!(conn = id, "connection accepted");
        Self {
            id,
            started: Instant::now(),
            shutdown: shutdown.clone(),
        }
    }

    pub(crate) fn established(&self, client_instance_id: impl std::fmt::Display) {
        tracing::info!(conn = self.id, client = %client_instance_id, "handshake accepted");
    }

    /// The connection is being closed by the core for a reason of its own.
    pub(crate) fn closing(&self, reason: &str) {
        tracing::warn!(conn = self.id, reason, "closing the connection");
    }

    /// A task of the connection ended; the connection ends with it.
    pub(crate) fn task_ended(
        &self,
        ended: &Option<Result<io::Result<bool>, tokio::task::JoinError>>,
    ) {
        match ended {
            Some(Ok(Ok(finished))) => {
                tracing::info!(conn = self.id, finished, "a connection task ended; closing");
            }
            Some(Ok(Err(error))) => {
                tracing::warn!(conn = self.id, kind = ?error.kind(), %error, "a connection task failed; closing");
            }
            Some(Err(error)) => {
                tracing::error!(conn = self.id, panicked = error.is_panic(), %error, "a connection task panicked or was lost; closing");
            }
            None => {}
        }
    }

    /// Wrap a task so its end is recorded with the role that ended.
    pub(crate) fn task<F: Future<Output = io::Result<bool>>>(
        &self,
        role: &'static str,
        work: F,
    ) -> impl Future<Output = io::Result<bool>> + use<F> {
        let conn = self.id;
        async move {
            let result = work.await;
            match &result {
                Ok(_) => tracing::info!(conn, role, "connection task ended"),
                Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => {
                    tracing::info!(conn, role, "the peer closed the connection");
                }
                Err(error) => tracing::warn!(
                    conn,
                    role,
                    kind = ?error.kind(),
                    os_error = ?error.raw_os_error(),
                    %error,
                    "connection task ended with an I/O error"
                ),
            }
            result
        }
    }

    pub(crate) fn finished(&self, result: &io::Result<()>) {
        let elapsed_ms = u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let core_stopping = *self.shutdown.borrow();
        match result {
            Ok(()) => tracing::info!(
                conn = self.id,
                elapsed_ms,
                core_stopping,
                "connection closed"
            ),
            Err(error) => tracing::warn!(
                conn = self.id,
                elapsed_ms,
                core_stopping,
                kind = ?error.kind(),
                %error,
                "connection closed with an error"
            ),
        }
    }
}
