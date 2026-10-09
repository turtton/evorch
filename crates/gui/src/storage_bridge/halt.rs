//! Durable record of storage refusing every event, which storage cannot hold itself.

use std::path::PathBuf;

use event_bus::event::diagnostic_codes;
use storage::{LimitKind, StorageError};

/// Spools one fault per halt episode; an accepted write ends the episode.
#[derive(Default)]
pub(super) struct HaltRecorder {
    spool: Option<PathBuf>,
    halted: Option<&'static str>,
}

impl HaltRecorder {
    pub(super) fn spooling_to(dir: PathBuf) -> Self {
        Self {
            spool: Some(dir),
            halted: None,
        }
    }

    pub(super) fn resumed(&mut self) {
        self.halted = None;
    }

    pub(super) fn failed(&mut self, error: &StorageError) {
        let Some(cause) = halt_cause(error) else {
            return;
        };
        if self.halted == Some(cause) {
            return;
        }
        self.halted = Some(cause);
        let Some(dir) = &self.spool else {
            return;
        };
        if let Err(spool_error) = runtime::self_improvement::spool_fault(
            dir,
            diagnostic_codes::STORAGE_WRITER_HALTED,
            &format!("storage:{cause}"),
            &format!("event writes halted: {error}"),
        ) {
            tracing::warn!(%spool_error, cause, "storage halt could not be spooled");
        }
    }
}

/// Errors after which no later event is accepted until something changes; per-event
/// rejections (size of one event, secret guard, stale mutation) are not halts.
fn halt_cause(error: &StorageError) -> Option<&'static str> {
    match error {
        StorageError::WriterClosed => Some("writer_closed"),
        StorageError::LimitExceeded { limit, .. } => match limit {
            LimitKind::DbSize => Some("db_size_limit"),
            LimitKind::SessionSize => Some("session_size_limit"),
            LimitKind::DailyBytes => Some("daily_bytes_limit"),
            LimitKind::WalSize => Some("wal_size_limit"),
            LimitKind::EventSize => None,
        },
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spooled(dir: &std::path::Path) -> Vec<Option<String>> {
        runtime::self_improvement::drain_crash_spool(dir)
            .into_iter()
            .map(|fault| fault.location)
            .collect()
    }

    #[test]
    fn spools_on_entering_a_halt_and_again_only_after_resuming_or_a_new_cause() {
        let dir = tempfile::tempdir().unwrap();
        let mut recorder = HaltRecorder::spooling_to(dir.path().to_path_buf());
        let db_size = StorageError::LimitExceeded {
            limit: LimitKind::DbSize,
            actual: 2,
            max: 1,
        };
        recorder.failed(&StorageError::WriterClosed);
        recorder.failed(&StorageError::WriterClosed);
        assert_eq!(spooled(dir.path()), [Some("storage:writer_closed".into())]);
        recorder.failed(&db_size);
        assert_eq!(spooled(dir.path()), [Some("storage:db_size_limit".into())]);
        recorder.resumed();
        recorder.failed(&db_size);
        assert_eq!(spooled(dir.path()), [Some("storage:db_size_limit".into())]);
    }

    #[test]
    fn per_event_rejections_are_not_halts() {
        let dir = tempfile::tempdir().unwrap();
        let mut recorder = HaltRecorder::spooling_to(dir.path().to_path_buf());
        recorder.failed(&StorageError::LimitExceeded {
            limit: LimitKind::EventSize,
            actual: 2,
            max: 1,
        });
        recorder.failed(&StorageError::StaleMutation);
        assert!(spooled(dir.path()).is_empty());
    }
}
