//! Explicit offline repair and conservative, replay-equivalent live retention.

pub(crate) mod offline;
pub(crate) mod retention;

pub use offline::{StripOptions, StripSummary, strip_task_progress_cursor};
pub use retention::{RetentionSummary, preview_task_progress_retention};

pub(crate) fn invalid(message: impl Into<String>) -> crate::StorageError {
    crate::StorageError::Serialization(message.into())
}

#[cfg(test)]
mod tests;
