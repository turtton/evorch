//! Explicit offline repair and conservative, replay-equivalent live retention.

mod compact;
pub(crate) mod offline;
pub(crate) mod retention;
pub(crate) mod retention_events;
mod vacuum;

pub use compact::{CompactOptions, CompactSummary, DeltaMode, backfill_events, compact_events};
pub use offline::{StripOptions, StripSummary, strip_task_progress_cursor};
pub use retention::{RetentionSummary, preview_task_progress_retention};
pub use vacuum::{VacuumOptions, VacuumSummary, vacuum_database};

pub(crate) fn invalid(message: impl Into<String>) -> crate::StorageError {
    crate::StorageError::Serialization(message.into())
}

#[cfg(test)]
mod tests;
