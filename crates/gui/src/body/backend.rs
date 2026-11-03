//! What the GUI needs from storage to show bodies lazily, as traits. The real
//! implementation reads the database on a worker thread; tests use a fake whose
//! completions they control. Nothing here is called from a frame.

use std::sync::Arc;

use event_bus::Event;

use super::text::BodyText;
use super::types::{BodyError, BodyErrorKind, BodyRef};

/// A stored event whose big body, if it has one, is not read yet.
#[derive(Debug, Clone, PartialEq)]
pub struct DeferredEvent {
    pub id: i64,
    /// The event as the reducers see it. When `body` is set, the string it
    /// stands for is empty here: only the transcript consumes `body`.
    pub event: Event,
    /// The event's one display body (message text, tool output, ...), still in
    /// the store, possibly in several pieces.
    pub body: Option<BodyText>,
}

/// Why a stored event cannot be shown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FailureKind {
    /// The row does not decode (damaged payload or an unknown variant).
    Unreadable,
    /// The row decodes but a body it names could not be read.
    Body(BodyErrorKind),
}

/// A stored event that could not be turned into an [`Event`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventFailure {
    pub id: i64,
    /// The run the `run_id` column names, when it names one.
    pub run_id: Option<String>,
    pub kind: FailureKind,
    /// Never contains stored text.
    pub reason: String,
}

/// One row of the stored event log, in id order.
#[derive(Debug, Clone, PartialEq)]
pub enum HistoryRow {
    Event(DeferredEvent),
    Failure(EventFailure),
}

/// The outcome of reading one event again.
#[derive(Debug, Clone, PartialEq)]
pub enum EventReread {
    Ready(DeferredEvent),
    Failed(EventFailure),
    /// The row no longer exists.
    Gone,
}

/// The per-body result of one storage call.
pub type BodyResult = Result<Arc<str>, BodyError>;

/// Reads from the store on the resolver's worker thread.
pub trait BodyBackend: Send + Sync {
    /// Reads up to 256 distinct bodies. A failure of the whole call (the
    /// connection, not one body) is the outer error and applies to all of them.
    fn resolve(&self, refs: &[BodyRef]) -> Result<Vec<BodyResult>, BodyError>;

    /// Reads one event again, as a deferred row.
    fn read_event(&self, id: i64) -> EventReread;
}

/// A backend that holds nothing: every body is missing. Used when no store is
/// attached, so a deferred body fails loudly instead of hanging.
pub struct NoBodies;

impl BodyBackend for NoBodies {
    fn resolve(&self, refs: &[BodyRef]) -> Result<Vec<BodyResult>, BodyError> {
        Ok(refs
            .iter()
            .map(|_| {
                Err(BodyError::new(
                    BodyErrorKind::Missing,
                    "no body store is attached",
                ))
            })
            .collect())
    }

    fn read_event(&self, _id: i64) -> EventReread {
        EventReread::Gone
    }
}
