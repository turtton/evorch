//! The stored event log as rows in id order, successes and failures together,
//! so a replay can show a gap exactly where the unreadable event sat.

use std::sync::Arc;

use super::backend::{DeferredEvent, EventFailure, FailureKind, HistoryRow};
use super::meter::{ImmediateClass, ImmediateMeter};
use super::split;
use super::types::{BodyError, BodyErrorKind, BodyRef};

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
pub struct StorageHistory<'a> {
    database: &'a storage::Database,
    meter: Option<ImmediateMeter>,
}

impl<'a> StorageHistory<'a> {
    #[must_use]
    pub const fn new(database: &'a storage::Database) -> Self {
        Self {
            database,
            meter: None,
        }
    }

    /// Counts what the replay reads at once in `meter`.
    #[must_use]
    pub fn with_meter(mut self, meter: Option<ImmediateMeter>) -> Self {
        self.meter = meter;
        self
    }
}

impl HistorySource for StorageHistory<'_> {
    fn scan(
        &self,
        through_id: i64,
        visit: &mut dyn FnMut(HistoryRow),
    ) -> Result<(), storage::StorageError> {
        let database = self.database;
        let meter = self.meter.clone().unwrap_or_default();
        database.for_each_event_ordered_deferred(through_id, |row| {
            visit(row_of(database, row, true, &meter));
        })
    }
}

/// The events concerning `runs` with `after < id <= through`, as history rows:
/// what a thread's runs were, found through the run index.
///
/// # Errors
/// The query itself failed.
pub fn rows_for_runs(
    database: &storage::Database,
    runs: &[String],
    after: i64,
    through: i64,
    meter: &ImmediateMeter,
) -> Result<Vec<HistoryRow>, storage::StorageError> {
    let mut rows = Vec::new();
    database.for_each_event_for_runs_deferred(runs, after, through, |row| {
        rows.push(row_of(database, row, true, meter));
    })?;
    Ok(rows)
}

/// One row of the log as a history row. With `defer_display` false the display
/// body is read too (a reader that joins strings); otherwise it stays a
/// reference. Only a tokenized event has token positions; an inline one
/// (whatever its strings look like) is taken as it is.
pub fn row_of(
    database: &storage::Database,
    row: storage::DeferredRow,
    defer_display: bool,
    meter: &ImmediateMeter,
) -> HistoryRow {
    match row {
        storage::DeferredRow::Event(event) => event_row(database, *event, defer_display, meter),
        storage::DeferredRow::Failed(failure) => HistoryRow::Failure(EventFailure {
            id: failure.id,
            run_id: failure.run_id,
            kind: FailureKind::Unreadable,
            reason: match failure.reason {
                storage::StoredEventFailureReason::Row(reason)
                | storage::StoredEventFailureReason::Payload(reason)
                | storage::StoredEventFailureReason::Variant(reason) => reason,
            },
            reference: None,
            wall_clock: failure.wall_clock,
        }),
    }
}

fn event_row(
    database: &storage::Database,
    stored: storage::DeferredStoredEvent,
    defer_display: bool,
    meter: &ImmediateMeter,
) -> HistoryRow {
    if stored.format != storage::StoredFormat::Tokenized {
        return HistoryRow::Event(DeferredEvent {
            id: stored.id,
            event: stored.event,
            body: None,
        });
    }
    let wall_clock = Some(stored.event.meta.wall_clock);
    let failed = |kind: FailureKind, reason: String, reference: Option<BodyRef>| {
        HistoryRow::Failure(EventFailure {
            id: stored.id,
            run_id: stored.run_id.clone(),
            kind,
            reason,
            reference,
            wall_clock,
        })
    };
    let container = match serde_json::to_value(&stored.event.kind) {
        Ok(container) => container,
        Err(error) => return failed(FailureKind::Unreadable, error.to_string(), None),
    };
    let mut held = 0_u64;
    let mut failing = None;
    let outcome = split::deferred_event(
        stored.id,
        stored.event.meta.clone(),
        container,
        stored.run_id.as_deref(),
        defer_display,
        &mut |reference, class| {
            let read = read_now(database, *reference, class, meter);
            match &read {
                Ok(text) => held += text.len() as u64,
                Err(_) => failing = Some(*reference),
            }
            read
        },
    );
    if held > 0 {
        meter.event_built(held);
    }
    match outcome {
        Ok(event) => HistoryRow::Event(event),
        Err(error) => failed(FailureKind::Body(error.kind), error.reason, failing),
    }
}

/// Reads one body at once, on the thread that builds the event, and counts it.
fn read_now(
    database: &storage::Database,
    reference: BodyRef,
    class: ImmediateClass,
    meter: &ImmediateMeter,
) -> Result<Arc<str>, BodyError> {
    let reference = storage::BodyRef::from(reference);
    let mut answers = database
        .resolve_bodies(&[reference])
        .map_err(|error| super::storage_backend::classify(&error))?;
    match answers.pop() {
        Some(Ok(text)) => {
            meter.record(class, text.len());
            Ok(Arc::from(text))
        }
        Some(Err(failure)) => Err(super::storage_backend::body_error(failure)),
        None => Err(BodyError::new(
            BodyErrorKind::Io,
            "the store answered nothing",
        )),
    }
}
