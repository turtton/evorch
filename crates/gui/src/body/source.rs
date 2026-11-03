//! The stored event log as rows in id order, successes and failures together,
//! so a replay can show a gap exactly where the unreadable event sat.

use super::backend::{DeferredEvent, EventFailure, FailureKind, HistoryRow};

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

/// The database as it is today: events carry their text inline.
pub struct StorageHistory<'a>(pub &'a storage::Database);

impl HistorySource for StorageHistory<'_> {
    fn scan(
        &self,
        through_id: i64,
        visit: &mut dyn FnMut(HistoryRow),
    ) -> Result<(), storage::StorageError> {
        let visit = std::cell::RefCell::new(visit);
        self.0.for_each_event_isolated(
            through_id,
            |stored| {
                (visit.borrow_mut())(HistoryRow::Event(DeferredEvent {
                    id: stored.id,
                    event: stored.event,
                    body: None,
                }));
            },
            |id, run, reason| {
                (visit.borrow_mut())(HistoryRow::Failure(EventFailure {
                    id,
                    run_id: run.map(str::to_owned),
                    kind: FailureKind::Unreadable,
                    reason: reason.to_owned(),
                }));
            },
        )
    }
}
