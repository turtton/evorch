//! The stored event log as rows in id order, successes and failures together,
//! so a replay can show a gap exactly where the unreadable event sat.

use super::backend::{DeferredEvent, EventFailure, FailureKind, HistoryRow};
use super::split;
use super::types::{BodyError, BodyErrorKind};

/// Reads the history on the thread that restores it (start-up, not a frame).
pub trait HistorySource {
    /// Hands every row up to `through_id` to `visit`, in id order.
    ///
    /// # Errors
    /// The query itself failed; a row that cannot be read is a
    /// [`HistoryRow::Failure`], not an error.
    fn scan(
        &self,
        through_id: i64,
        visit: &mut dyn FnMut(HistoryRow),
    ) -> Result<(), storage::StorageError>;
}

/// The database, read without its long bodies: an event whose long strings are
/// tokens comes back with its display body still in the store.
pub struct StorageHistory<'a>(pub &'a storage::Database);

impl HistorySource for StorageHistory<'_> {
    fn scan(
        &self,
        through_id: i64,
        visit: &mut dyn FnMut(HistoryRow),
    ) -> Result<(), storage::StorageError> {
        let database = self.0;
        database.for_each_event_ordered_deferred(through_id, |row| {
            visit(match row {
                storage::DeferredRow::Event(event) => row_of(database, *event),
                storage::DeferredRow::Failed(failure) => HistoryRow::Failure(EventFailure {
                    id: failure.id,
                    run_id: failure.run_id,
                    kind: FailureKind::Unreadable,
                    reason: match failure.reason {
                        storage::StoredEventFailureReason::Row(reason)
                        | storage::StoredEventFailureReason::Payload(reason)
                        | storage::StoredEventFailureReason::Variant(reason) => reason,
                    },
                }),
            });
        })
    }
}

/// One stored event as a history row. Only a tokenized event has token
/// positions; an inline one (whatever its strings look like) is taken as it is.
fn row_of(database: &storage::Database, stored: storage::DeferredStoredEvent) -> HistoryRow {
    if stored.format != storage::StoredFormat::Tokenized {
        return HistoryRow::Event(DeferredEvent {
            id: stored.id,
            event: stored.event,
            body: None,
        });
    }
    let failed = |kind: FailureKind, reason: String| {
        HistoryRow::Failure(EventFailure {
            id: stored.id,
            run_id: stored.run_id.clone(),
            kind,
            reason,
        })
    };
    let container = match serde_json::to_value(&stored.event.kind) {
        Ok(container) => container,
        Err(error) => return failed(FailureKind::Unreadable, error.to_string()),
    };
    match split::deferred_event(
        stored.id,
        stored.event.meta.clone(),
        container,
        stored.run_id.as_deref(),
        &mut |reference| read_now(database, *reference),
    ) {
        Ok(event) => HistoryRow::Event(event),
        Err(error) => failed(FailureKind::Body(error.kind), error.reason),
    }
}

/// Reads one body at once: a token that is not the display body is needed by
/// whatever reads the event, so it is read during the replay.
fn read_now(
    database: &storage::Database,
    reference: super::BodyRef,
) -> Result<std::sync::Arc<str>, BodyError> {
    let reference = storage::BodyRef::from(reference);
    let mut answers = database
        .resolve_bodies(&[reference])
        .map_err(|error| BodyError::new(BodyErrorKind::Io, error.to_string()))?;
    match answers.pop() {
        Some(Ok(text)) => Ok(std::sync::Arc::from(text)),
        Some(Err(failure)) => Err(super::storage_backend::body_error(failure)),
        None => Err(BodyError::new(
            BodyErrorKind::Io,
            "the store answered nothing",
        )),
    }
}
