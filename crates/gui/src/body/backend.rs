//! What the GUI needs from storage to show bodies lazily, as traits. The real
//! implementation reads the database on a worker thread; tests use a fake whose
//! completions they control. Nothing here is called from a frame.

use std::sync::Arc;

use event_bus::Event;

use super::meter::ImmediateMeter;
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
    /// The id of the first slice when this event was joined from the slices of
    /// a response: the response stands for every id from there to `id`.
    pub first_id: Option<i64>,
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
    /// The body whose read failed, when the failure is a body's: the event
    /// itself decodes. Lets the gap say which body, and its details name it.
    pub reference: Option<BodyRef>,
    /// When the event happened, if its columns still say: where a message the
    /// user sent before it belongs in the replay.
    pub wall_clock: Option<std::time::SystemTime>,
}

/// One row of the stored event log, in id order.
#[derive(Debug, Clone, PartialEq)]
// A row is handed on once, by value, for every event of a replay: boxing the
// common variant would add an allocation to each of them.
#[allow(clippy::large_enum_variant)]
pub enum HistoryRow {
    Event(DeferredEvent),
    Failure(EventFailure),
}

/// The per-body result of one storage call.
pub type BodyResult = Result<Arc<str>, BodyError>;

/// Reads from the store on the resolver's worker thread.
pub trait BodyBackend: Send + Sync {
    /// Reads up to 256 distinct bodies. A failure of the whole call (the
    /// connection, not one body) is the outer error and applies to all of them.
    fn resolve(&self, refs: &[BodyRef]) -> Result<Vec<BodyResult>, BodyError>;

    /// Reads the events concerning `runs` with `after < id <= through` as history
    /// rows (a row that cannot be read in its place), in id order. Nested
    /// bodies are read here, on the worker; display bodies stay references.
    /// Counts what it reads at once in `meter`.
    ///
    /// # Errors
    /// The query itself failed.
    fn read_rows(
        &self,
        runs: &[String],
        after: i64,
        through: i64,
        meter: &ImmediateMeter,
    ) -> Result<Vec<HistoryRow>, BodyError>;
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

    fn read_rows(
        &self,
        _runs: &[String],
        _after: i64,
        _through: i64,
        _meter: &ImmediateMeter,
    ) -> Result<Vec<HistoryRow>, BodyError> {
        Ok(Vec::new())
    }
}
