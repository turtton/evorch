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

    /// Flush buffered usage and sync the WAL into the database. A busy checkpoint
    /// is an error, not evidence that it is safe to exit.
    pub fn flush_resume_intents(&self) -> Result<(), StorageError> {
        self.request(Command::FlushResumeIntents)
    }
}
