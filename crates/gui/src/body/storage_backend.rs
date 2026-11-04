//! The worker's connection to the database.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use super::backend::{BodyBackend, BodyResult, HistoryRow};
use super::meter::ImmediateMeter;
use super::types::{BodyError, BodyErrorKind, BodyRef};

/// Opens its read-only connection on first use, on the resolver's thread.
pub struct StorageBodies {
    path: PathBuf,
    db: Mutex<Option<storage::Database>>,
}

impl StorageBodies {
    #[must_use]
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            db: Mutex::new(None),
        }
    }

    fn with_db<T>(
        &self,
        read: impl FnOnce(&storage::Database) -> Result<T, storage::StorageError>,
    ) -> Result<T, BodyError> {
        let mut slot = self.db.lock().unwrap_or_else(|e| e.into_inner());
        if slot.is_none() {
            *slot = Some(storage::Database::open_read_only(&self.path).map_err(|e| classify(&e))?);
        }
        let Some(db) = slot.as_ref() else {
            return Err(BodyError::new(
                BodyErrorKind::Io,
                "the database is not open",
            ));
        };
        let outcome = read(db);
        if outcome.is_err() {
            // A connection that failed is reopened by the next try.
            *slot = None;
        }
        outcome.map_err(|e| classify(&e))
    }
}

/// Busy and I/O failures are worth another try; anything else is not.
pub(super) fn classify(error: &storage::StorageError) -> BodyError {
    let text = error.to_string();
    let lower = text.to_lowercase();
    let kind = if lower.contains("locked") || lower.contains("busy") {
        BodyErrorKind::Busy
    } else {
        BodyErrorKind::Io
    };
    BodyError::new(kind, text)
}

/// Storage's account of one body that could not be read.
pub(super) fn body_error(failure: storage::BodyFailure) -> BodyError {
    let kind = match failure.class {
        storage::BodyFailureClass::Missing => BodyErrorKind::Missing,
        storage::BodyFailureClass::Corrupt => BodyErrorKind::Corrupt,
        storage::BodyFailureClass::Busy => BodyErrorKind::Busy,
        storage::BodyFailureClass::Io => BodyErrorKind::Io,
    };
    BodyError::new(kind, failure.reason)
}

impl BodyBackend for StorageBodies {
    fn resolve(&self, refs: &[BodyRef]) -> Result<Vec<BodyResult>, BodyError> {
        let wanted: Vec<storage::BodyRef> = refs.iter().map(|&r| r.into()).collect();
        let answers = self.with_db(|db| db.resolve_bodies(&wanted))?;
        Ok(answers
            .into_iter()
            .map(|answer| match answer {
                Ok(text) => Ok(Arc::from(text)),
                Err(failure) => Err(body_error(failure)),
            })
            .collect())
    }

    fn read_rows(
        &self,
        runs: &[String],
        after: i64,
        through: i64,
        meter: &ImmediateMeter,
    ) -> Result<Vec<HistoryRow>, BodyError> {
        self.with_db(|db| super::source::rows_for_runs(db, runs, after, through, meter))
    }
}
