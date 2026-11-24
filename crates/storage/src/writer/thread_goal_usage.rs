use super::{Command, StorageHandle};
use crate::{Database, StorageError};
use event_bus::ThreadGoalUsage;

impl StorageHandle {
    /// Persist counters without an event or event-feed notification. The transient
    /// binding is checked against tombstones. Success means commit, not admission.
    ///
    /// # Errors
    /// Suspended/closed writer, tombstones, corrupt counters or SQLite failure.
    pub fn upsert_thread_goal_usage(
        &self,
        goal_id: &str,
        thread_id: &str,
        root_run_id: &str,
        usage: &ThreadGoalUsage,
    ) -> Result<(), StorageError> {
        let result = self.request(|reply| {
            Command::UpsertThreadGoalUsage(
                goal_id.into(),
                thread_id.into(),
                root_run_id.into(),
                usage.clone(),
                reply,
            )
        });
        #[cfg(feature = "goal-usage-test-support")]
        crate::goal_usage_test_support::visit(
            goal_id,
            if result.is_ok() {
                crate::goal_usage_test_support::Point::AfterAck
            } else {
                crate::goal_usage_test_support::Point::AfterFailure
            },
        )?;
        result
    }
}
impl Database {
    /// Read absolute counters. Absence is normal for old or unused goals.
    ///
    /// # Errors
    /// SQLite failure or invalid counter type, length or range.
    pub fn thread_goal_usage(
        &self,
        goal_id: &str,
    ) -> Result<Option<ThreadGoalUsage>, StorageError> {
        #[cfg(feature = "goal-usage-test-support")]
        crate::goal_usage_test_support::visit(
            goal_id,
            crate::goal_usage_test_support::Point::Read,
        )?;
        crate::repo::thread_goal_usage::get(&self.conn, goal_id)
    }
}
