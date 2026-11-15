use super::*;
use crate::memory::{Lesson, MemoryStatus};
use crate::repo::memory::{Mutation, apply};
use serde::{Deserialize, Serialize};

/// Admission records this limit; cancellations consume a start and never reset it.
pub const MAX_LEARNING_ATTEMPTS: u32 = 3;

/// Exact readback identity. Never replace this with a fresh key after an unknown ACK.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LearningAttemptKey {
    pub task_id: String,
    pub owner: String,
    pub attempt: u32,
    pub generation: u64,
    pub revision: u64,
    pub state: LearningJobState,
}

impl LearningJob {
    pub fn key(&self) -> LearningAttemptKey {
        LearningAttemptKey {
            task_id: self.task_id.clone(),
            owner: self.origin.instance.clone(),
            attempt: self.attempt,
            generation: self.generation,
            revision: self.revision,
            state: self.state,
        }
    }
}

/// Bounded reason codes, never provider errors or conversation text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LearningEndReason {
    AttemptLimit,
    UnconfirmedWrite,
    OriginDied,
    NoResumeAuthority,
    PipelineFailed,
    ProducerClosed,
    EvidenceUnavailable,
}

impl LearningEndReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AttemptLimit => "attempt_limit",
            Self::UnconfirmedWrite => "unconfirmed_write",
            Self::OriginDied => "origin_died",
            Self::NoResumeAuthority => "no_resume_authority",
            Self::PipelineFailed => "pipeline_failed",
            Self::ProducerClosed => "producer_closed",
            Self::EvidenceUnavailable => "evidence_unavailable",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LearningReviewDecision {
    pub status: MemoryStatus,
    pub rationale: String,
}

#[derive(Debug, Clone)]
pub enum LearningAttemptChange {
    Start,
    BindRun {
        run: String,
        review: bool,
    },
    /// Only the complete, validated extraction batch is durable.
    ExtractionReady(Vec<Lesson>),
    ReviewReady(Vec<LearningReviewDecision>),
    Publish,
    Fail(LearningEndReason),
    Interrupt {
        suspension_id: String,
    },
    /// Authority and old-task completion are checked by the caller (4b).
    Requeue {
        suspension_id: String,
        to: LearningOrigin,
    },
    Abandon(LearningEndReason),
}

#[derive(Debug, Clone)]
pub struct LearningAttemptWrite {
    pub key: LearningAttemptKey,
    pub change: LearningAttemptChange,
}

pub(super) fn write(
    conn: &Connection,
    command: LearningAttemptWrite,
    at_ns: i64,
) -> Result<bool, StorageError> {
    let receipt = command_hash(&command.key, &command.change);
    in_transaction(conn, |tx| {
        let job = tx
            .query_row(
                &format!("SELECT {JOB_COLUMNS} FROM learning_jobs WHERE task_id=?1"),
                [&command.key.task_id],
                job_row,
            )
            .optional()?
            .transpose()?;
        let Some(job) = job else { return Ok(false) };
        if job.key() != command.key || job.state.is_terminal() {
            return Ok(false);
        }
        let id = &job.task_id;
        use LearningAttemptChange as C;
        use LearningJobState as S;
        match command.change {
            C::Start if job.state == S::Pending => {
                if job.attempt >= job.max_attempts {
                    terminal(tx, &job, LearningEndReason::AttemptLimit, false, at_ns)?;
                } else {
                    tx.execute(
                        "UPDATE learning_jobs SET state='Started', attempt=attempt+1,
                         generation=generation+1, started_at_ns=?2, suspension_id=NULL,
                         extraction_run=NULL, review_run=NULL, revision=revision+1 WHERE task_id=?1",
                        params![id, at_ns],
                    )?;
                }
            }
            C::BindRun { run, review } if job.state == S::Started => {
                let field = if review {
                    "review_run"
                } else {
                    "extraction_run"
                };
                if run.is_empty()
                    || run.len() > 256
                    || if review {
                        job.review_run.is_some()
                    } else {
                        job.extraction_run.is_some()
                    }
                {
                    return Ok(false);
                }
                tx.execute(
                    &format!(
                        "UPDATE learning_jobs SET {field}=?2, revision=revision+1 WHERE task_id=?1"
                    ),
                    params![id, run],
                )?;
            }
            C::ExtractionReady(lessons) if job.state == S::Started => {
                if lessons.len() > 8 || job.extraction_run.is_none() {
                    return Ok(false);
                }
                for (slot, lesson) in lessons.iter().enumerate() {
                    // Public identity is independent of either internal RunId.
                    if lesson.id != format!("{}:{}:{}", job.project, id, slot + 1)
                        || lesson.task_id != *id
                        || lesson.project != job.project
                    {
                        return Err(corrupt("invalid learning publication identity".into()));
                    }
                }
                if tx.execute(
                    "INSERT INTO learning_job_staging(task_id,lessons_json) VALUES(?1,?2)
                     ON CONFLICT(task_id) DO NOTHING",
                    params![id, serde_json::to_string(&lessons).map_err(json_error)?],
                )? == 0
                {
                    return Ok(false);
                }
                bump(tx, id)?;
            }
            C::ReviewReady(reviews) if job.state == S::Started => {
                let Some((lessons, previous)) = staging(tx, id)? else {
                    return Ok(false);
                };
                if previous.is_some()
                    || job.review_run.is_none()
                    || reviews.len() != lessons.len()
                    || reviews.iter().any(|r| {
                        !matches!(
                            r.status,
                            MemoryStatus::Candidate
                                | MemoryStatus::Promoted
                                | MemoryStatus::Rejected
                        ) || r.rationale.len() > 1_000
                    })
                {
                    return Ok(false);
                }
                tx.execute(
                    "UPDATE learning_job_staging SET reviews_json=?2 WHERE task_id=?1",
                    params![id, serde_json::to_string(&reviews).map_err(json_error)?],
                )?;
                bump(tx, id)?;
            }
            C::Publish if job.state == S::Started => {
                let Some((lessons, reviews)) = staging(tx, id)? else {
                    return Ok(false);
                };
                // A genuinely empty extraction is complete without a reviewer.
                if !lessons.is_empty() && reviews.is_none() {
                    return Ok(false);
                }
                let reviews = reviews.unwrap_or_default();
                // Guard/validation failures roll back the entire batch, including promotes.
                // The caller then uses Fail, which falls back to no-publication cleanup.
                publish_batch(tx, &lessons, Some(&reviews))?;
                finish(
                    tx,
                    &job,
                    "Finished",
                    "Reviewed",
                    None,
                    Some(&reviews),
                    at_ns,
                )?;
            }
            C::Fail(reason) if job.state == S::Started => {
                terminal(tx, &job, reason, true, at_ns)?;
            }
            C::Interrupt { suspension_id } if job.state == S::Started => {
                if suspension_id.is_empty() || suspension_id.len() > 256 {
                    return Ok(false);
                }
                tx.execute(
                    "UPDATE learning_jobs SET state='Interrupted', generation=generation+1,
                     suspension_id=?2, revision=revision+1 WHERE task_id=?1",
                    params![id, suspension_id],
                )?;
                tx.execute("DELETE FROM learning_job_staging WHERE task_id=?1", [id])?;
            }
            C::Requeue { suspension_id, to } if job.state == S::Interrupted => {
                if job.suspension_id.as_deref() != Some(&suspension_id)
                    || job.origin.kind != LearningOriginKind::Core
                    || to.kind != LearningOriginKind::Core
                    || to.config_identity.is_none()
                    || job.origin.config_identity != to.config_identity
                {
                    return Ok(false);
                }
                if job.attempt >= job.max_attempts {
                    terminal(tx, &job, LearningEndReason::AttemptLimit, false, at_ns)?;
                } else {
                    tx.execute(
                        "UPDATE learning_jobs SET state='Pending', origin_instance=?2,
                         origin_pid=?3, origin_start_ticks=?4, origin_boot_id=?5, origin_pid_ns=?6,
                         suspension_id=NULL, revision=revision+1 WHERE task_id=?1",
                        params![
                            id,
                            to.instance,
                            to.pid,
                            to.start_ticks,
                            to.boot_id,
                            to.pid_ns
                        ],
                    )?;
                }
            }
            C::Abandon(reason) => terminal(
                tx,
                &job,
                reason,
                job.state == S::Started && reason == LearningEndReason::OriginDied,
                at_ns,
            )?,
            _ => return Ok(false),
        }
        let next = tx
            .query_row(
                &format!("SELECT {JOB_COLUMNS} FROM learning_jobs WHERE task_id=?1"),
                [id],
                job_row,
            )??
            .key();
        tx.execute(
            "INSERT INTO learning_attempt_receipts(task_id,revision,command_hash,result_key_json) VALUES(?1,?2,?3,?4)",
            params![id, command.key.revision, receipt, serde_json::to_string(&next).map_err(json_error)?],
        )?;
        Ok(true)
    })
}

fn bump(tx: &Connection, id: &str) -> Result<(), StorageError> {
    tx.execute(
        "UPDATE learning_jobs SET revision=revision+1 WHERE task_id=?1",
        [id],
    )?;
    Ok(())
}

type Staging = (Vec<Lesson>, Option<Vec<LearningReviewDecision>>);
fn staging(tx: &Connection, id: &str) -> Result<Option<Staging>, StorageError> {
    let raw: Option<(String, Option<String>)> = tx
        .query_row(
            "SELECT lessons_json,reviews_json FROM learning_job_staging WHERE task_id=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    raw.map(|(lessons, reviews)| {
        Ok((
            serde_json::from_str(&lessons).map_err(json_error)?,
            reviews
                .map(|v| serde_json::from_str(&v))
                .transpose()
                .map_err(json_error)?,
        ))
    })
    .transpose()
}

fn publish_batch(
    tx: &Connection,
    lessons: &[Lesson],
    reviews: Option<&[LearningReviewDecision]>,
) -> Result<(), StorageError> {
    for (slot, lesson) in lessons.iter().enumerate() {
        // Do not touch preexisting user favorites or promoted lessons, even on an ID collision.
        if tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM memory_entries WHERE id=?1)",
            [&lesson.id],
            |r| r.get::<_, bool>(0),
        )? {
            return Err(corrupt("learning lesson identity already published".into()));
        }
        apply(tx, &Mutation::Candidate(lesson.clone()))?;
        if let Some(review) = reviews.and_then(|r| r.get(slot)) {
            crate::entity::SecretGuard::from_env().check_text(
                "memory",
                "review",
                &review.rationale,
            )?;
            match review.status {
                MemoryStatus::Promoted => {
                    apply(
                        tx,
                        &Mutation::Validate {
                            id: lesson.id.clone(),
                            evidence: lesson.evidence.clone(),
                        },
                    )?;
                    apply(tx, &Mutation::Promote(lesson.id.clone()))?;
                }
                MemoryStatus::Rejected => apply(tx, &Mutation::Reject(lesson.id.clone()))?,
                _ => {}
            }
        }
    }
    Ok(())
}

fn terminal(
    tx: &Connection,
    job: &LearningJob,
    reason: LearningEndReason,
    candidates: bool,
    at_ns: i64,
) -> Result<(), StorageError> {
    let mut outcome = "None";
    let complete = if candidates {
        match staging(tx, &job.task_id) {
            Ok(batch) => batch,
            Err(StorageError::Serialization(_)) => None,
            Err(error) => return Err(error),
        }
    } else {
        None
    };
    if let Some((lessons, _)) = complete {
        // A rejected candidate must not keep sensitive evidence alive forever.
        // A savepoint undoes ALL batch writes, not just the offending lesson.
        tx.execute_batch("SAVEPOINT learning_candidates")?;
        match publish_batch(tx, &lessons, None) {
            Ok(()) => {
                tx.execute_batch("RELEASE learning_candidates")?;
                outcome = "Candidates";
            }
            Err(StorageError::SecretDetected { .. } | StorageError::Serialization(_)) => {
                tx.execute_batch("ROLLBACK TO learning_candidates; RELEASE learning_candidates")?;
            }
            Err(error) => return Err(error),
        }
    }
    finish(
        tx,
        job,
        if matches!(
            reason,
            LearningEndReason::PipelineFailed | LearningEndReason::ProducerClosed
        ) {
            "Failed"
        } else {
            "Abandoned"
        },
        outcome,
        Some(reason),
        None,
        at_ns,
    )
}

fn finish(
    tx: &Connection,
    job: &LearningJob,
    state: &str,
    outcome: &str,
    reason: Option<LearningEndReason>,
    reviews: Option<&[LearningReviewDecision]>,
    at_ns: i64,
) -> Result<(), StorageError> {
    tx.execute(
        "INSERT INTO learning_job_publications(task_id,attempt,outcome,reviews_json,committed_at_ns)
         VALUES(?1,?2,?3,?4,?5)",
        params![job.task_id, job.attempt, outcome, reviews.map(serde_json::to_string).transpose().map_err(json_error)?, at_ns],
    )?;
    tx.execute(
        "UPDATE learning_jobs SET state=?2, publication=?3, reason=?4, finished_at_ns=?5,
         generation=generation+1, revision=revision+1 WHERE task_id=?1",
        params![
            job.task_id,
            state,
            outcome,
            reason.map(LearningEndReason::as_str),
            at_ns
        ],
    )?;
    tx.execute(
        "DELETE FROM learning_job_evidence WHERE task_id=?1",
        [&job.task_id],
    )?;
    tx.execute(
        "DELETE FROM learning_job_staging WHERE task_id=?1",
        [&job.task_id],
    )?;
    Ok(())
}

fn json_error(error: serde_json::Error) -> StorageError {
    StorageError::Serialization(error.to_string())
}
fn command_hash(key: &LearningAttemptKey, change: &LearningAttemptChange) -> String {
    format!(
        "{:x}",
        Sha256::digest(format!("{key:?}:{change:?}").as_bytes())
    )
}
impl Database {
    /// The result belongs to this exact operation, not the job's latest authority.
    pub fn learning_attempt_receipt(
        &self,
        key: &LearningAttemptKey,
        change: &LearningAttemptChange,
    ) -> Result<Option<LearningAttemptKey>, StorageError> {
        let json: Option<String> = self.conn.query_row("SELECT result_key_json FROM learning_attempt_receipts WHERE task_id=?1 AND revision=?2 AND command_hash=?3", params![key.task_id,key.revision,command_hash(key,change)], |row| row.get(0)).optional()?;
        json.map(|json| serde_json::from_str(&json).map_err(json_error))
            .transpose()
    }
    pub fn learning_attempt_acknowledged(
        &self,
        key: &LearningAttemptKey,
        change: &LearningAttemptChange,
    ) -> Result<bool, StorageError> {
        Ok(self.learning_attempt_receipt(key, change)?.is_some())
    }
}
