use super::{Command, StorageHandle};
use crate::StorageError;
use crate::learning_jobs::{LearningAdmission, LearningOrigin, NewLearningJob, Write, Written};
use std::time::SystemTime;

impl StorageHandle {
    /// Admit a learning job: the dedup claim, the `Pending` job and its immutable
    /// evidence are stored in one transaction, and the daily quota is spent once, on
    /// the UTC day of `at`.
    ///
    /// # Errors
    /// Returns an error for suspended writes, SQLite failure or a closed writer.
    pub fn admit_learning_job(
        &self,
        job: NewLearningJob,
        at: SystemTime,
    ) -> Result<LearningAdmission, StorageError> {
        let at_ns = crate::system_time_to_ns(at)?;
        match self.learning_write(Write::Admit(Box::new(job), at_ns))? {
            Written::Admission(admission) => Ok(admission),
            Written::Applied(_) => unreachable!("admit reply"),
        }
    }

    /// Fenced CAS. On an unknown ACK, read back this same job/key before any retry.
    pub fn write_learning_attempt(
        &self,
        command: crate::learning_jobs::LearningAttemptWrite,
        at: SystemTime,
    ) -> Result<bool, StorageError> {
        self.learning_applied(Write::Attempt(
            Box::new(command),
            crate::system_time_to_ns(at)?,
        ))
    }

    /// Move a core's `Pending` job from a dead instance to `to`. It fails if anyone
    /// else changed the job since it was read at `revision`, so two cores never take
    /// the same job.
    ///
    /// # Errors
    /// Returns an error for suspended writes, SQLite failure or a closed writer.
    pub fn adopt_learning_job(
        &self,
        task_id: &str,
        from_instance: &str,
        revision: u64,
        to: LearningOrigin,
    ) -> Result<bool, StorageError> {
        self.learning_applied(Write::Adopt {
            task_id: task_id.into(),
            from_instance: from_instance.into(),
            revision,
            to: Box::new(to),
        })
    }

    fn learning_applied(&self, write: Write) -> Result<bool, StorageError> {
        match self.learning_write(write)? {
            Written::Applied(applied) => Ok(applied),
            Written::Admission(_) => unreachable!("transition reply"),
        }
    }

    fn learning_write(&self, write: Write) -> Result<Written, StorageError> {
        self.request(|reply| Command::LearningJob(write, reply))
    }
}
