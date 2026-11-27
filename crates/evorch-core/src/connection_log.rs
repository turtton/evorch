//! What a core connection records about how it ended. A client that loses its
//! socket sees only "closed"; this is where the core says which of its tasks
//! stopped first and with which I/O error.
use std::{
    future::Future,
    io,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
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
    finished: AtomicBool,
}

impl ConnectionLog {
    pub(crate) fn new(shutdown: &watch::Receiver<bool>) -> Self {
        let id = NEXT_CONNECTION.fetch_add(1, Ordering::Relaxed);
        tracing::info!(conn = id, "connection accepted");
        Self {
            id,
            started: Instant::now(),
            shutdown: shutdown.clone(),
            finished: AtomicBool::new(false),
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
        self.finished.store(true, Ordering::Relaxed);
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

impl Drop for ConnectionLog {
    /// A connection whose handler panicked or was cancelled never reaches
    /// `finished`; this is the record that still names it.
    fn drop(&mut self) {
        if self.finished.load(Ordering::Relaxed) {
            return;
        }
        tracing::error!(
            conn = self.id,
            elapsed_ms = u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX),
            panicked = std::thread::panicking(),
            "connection ended without a result: its handler panicked or was cancelled"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct Captured(Arc<Mutex<Vec<u8>>>);

    impl io::Write for Captured {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn captured(run: impl FnOnce()) -> String {
        let log = Captured::default();
        let writer = log.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, run);
        String::from_utf8(log.0.lock().unwrap().clone()).unwrap()
    }

    #[test]
    fn a_handler_that_panics_still_leaves_a_record_naming_its_connection() {
        let (_stop, stopped) = watch::channel(false);
        let text = captured(|| {
            let log = ConnectionLog::new(&stopped);
            let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _log = log;
                panic!("handler bug");
            }));
            assert!(panicked.is_err());
        });
        let ended = text
            .lines()
            .find(|line| line.contains("connection ended without a result"))
            .unwrap_or_else(|| panic!("{text}"));
        assert!(
            ended.contains("panicked=true") && ended.contains("conn="),
            "{ended}"
        );
    }

    #[test]
    fn a_connection_that_finished_is_recorded_once_and_not_as_lost() {
        let (_stop, stopped) = watch::channel(false);
        let text = captured(|| {
            let log = ConnectionLog::new(&stopped);
            log.finished(&Ok(()));
        });
        assert!(text.contains("connection closed"), "{text}");
        assert!(!text.contains("without a result"), "{text}");
    }
}
