//! Explicit offline repair and conservative, replay-equivalent live retention.

pub(crate) mod archive_purge;
mod compact;
mod convert_bodies;
pub(crate) mod offline;
pub(crate) mod retention;
pub(crate) mod retention_events;
mod vacuum;

pub use compact::{CompactOptions, CompactSummary, DeltaMode, backfill_events, compact_events};
#[cfg(any(test, feature = "unchecked-open"))]
pub use convert_bodies::testing::{
    downgrade_to_v39, downgrade_to_v40, downgrade_to_v41, downgrade_to_v42, downgrade_to_v43,
};
pub use convert_bodies::{
    Analysis, CanonicalizedRuns, ConvertBodiesFailure, ConvertBodiesOptions, ConvertBodiesSummary,
    EventProblem, EventsReport, KindFigure, Problem, RetentionReport, convert_bodies,
};
pub use offline::{StripOptions, StripSummary, strip_task_progress_cursor};
pub use retention::{RetentionSummary, preview_task_progress_retention};
pub use vacuum::{VacuumOptions, VacuumSummary, vacuum_database};

pub(crate) fn invalid(message: impl Into<String>) -> crate::StorageError {
    crate::StorageError::Serialization(message.into())
}

#[cfg(test)]
mod tests;
