use super::{Command, StorageHandle};
use crate::StorageError;
impl StorageHandle {
    /// Acknowledged by the single writer before admission or checkpoint completion.
    pub fn save_resume_intent(
        &self,
        record: crate::resume_intent::ResumeIntentRecord,
        expected_incarnation: Option<String>,
    ) -> Result<(), StorageError> {
        self.request(|reply| Command::SaveResumeIntent(record, expected_incarnation, reply))
    }

    /// Consume a checkpoint and admit its new incarnation atomically. The writer
    /// commits with FULL synchronous durability; no fallible post-commit flush
    /// can turn a committed consumption into an apparently retryable error.
    pub fn save_snapshot_bundle_with_resume_intent(
        &self,
        write: &crate::thread_index::SnapshotWrite,
        record: crate::resume_intent::ResumeIntentRecord,
        expected_incarnation: Option<String>,
    ) -> Result<crate::thread_index::SnapshotReceipt, crate::thread_index::SnapshotApiError> {
        self.snapshot_request(|reply| {
            Command::SaveSnapshotBundleWithResumeIntent(
                Box::new(write.clone()),
                record,
                expected_incarnation,
                reply,
            )
        })
    }

    /// Flush buffered usage and sync the WAL into the database. A busy checkpoint
    /// is an error, not evidence that it is safe to exit.
    pub fn flush_resume_intents(&self) -> Result<(), StorageError> {
        self.request(Command::FlushResumeIntents)
    }
}
