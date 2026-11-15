//! Durable learning job queue (07b-2).
//!
//! A job is admitted together with its dedup claim and immutable evidence in one
//! transaction. It then moves `Pending -> Started -> Finished | Failed | Abandoned`;
//! every step is a compare-and-set, and nothing here ever moves a job backwards.
//! Only `Pending` jobs may run again after their origin died: a `Started` job (or one
//! whose start may or may not have been recorded) is ended, never queued again.
//!
//! The origin names the process that owns a job, with enough identity for a caller to
//! judge whether that process is still alive. This module stores and compares the
//! identity; deciding liveness is the caller's job.
use crate::{Database, StorageError};
use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};

/// Who may run a job again after its origin died: only a core, and only its own config.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LearningOriginKind {
    Core,
    Gui,
}

impl LearningOriginKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Core => "Core",
            Self::Gui => "Gui",
        }
    }
    fn parse(text: &str) -> Result<Self, StorageError> {
        match text {
            "Core" => Ok(Self::Core),
            "Gui" => Ok(Self::Gui),
            other => Err(corrupt(format!("unknown learning origin kind {other}"))),
        }
    }
}

/// The process (and incarnation of it) that owns a job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LearningOrigin {
    pub kind: LearningOriginKind,
    /// Random per incarnation. A process that `exec`s keeps its pid, so the pid alone
    /// cannot tell the successor from its predecessor; the instance can.
    pub instance: String,
    pub pid: u32,
    /// `/proc/<pid>/stat` start time, so a recycled pid is not mistaken for the origin.
    pub start_ticks: Option<u64>,
    pub boot_id: Option<String>,
    /// Pid namespace, so a pid that this namespace cannot see is "unknown", not "dead".
    pub pid_ns: Option<String>,
    /// Identity of the config directory a core serves; a core only takes over its own.
    pub config_identity: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LearningJobState {
    Pending,
    Started,
    Finished,
    Failed,
    Abandoned,
}

impl LearningJobState {
    pub const ALL: [Self; 5] = [
        Self::Pending,
        Self::Started,
        Self::Finished,
        Self::Failed,
        Self::Abandoned,
    ];
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "Pending",
            Self::Started => "Started",
            Self::Finished => "Finished",
            Self::Failed => "Failed",
            Self::Abandoned => "Abandoned",
        }
    }
    pub fn is_terminal(self) -> bool {
        !matches!(self, Self::Pending | Self::Started)
    }
    fn parse(text: &str) -> Result<Self, StorageError> {
        Self::ALL
            .into_iter()
            .find(|state| state.as_str() == text)
            .ok_or_else(|| corrupt(format!("unknown learning job state {text}")))
    }
}

/// Everything admission stores. `evidence_json` is opaque to storage and immutable.
#[derive(Debug, Clone)]
pub struct NewLearningJob {
    pub task_id: String,
    pub roots: Vec<String>,
    pub goal: bool,
    pub daily_limit: u32,
    pub origin: LearningOrigin,
    pub project: String,
    pub source_run_id: String,
    pub prompt: String,
    pub evidence_json: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LearningAdmission {
    Admitted,
    /// The task or one of its roots was already admitted.
    Duplicate,
    QuotaExhausted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LearningJob {
    pub task_id: String,
    pub state: LearningJobState,
    pub goal: bool,
    pub project: String,
    pub source_run_id: String,
    pub origin: LearningOrigin,
    pub revision: u64,
    pub admitted_at_ns: i64,
    pub started_at_ns: Option<i64>,
    pub finished_at_ns: Option<i64>,
    pub reason: Option<String>,
}

/// Evidence whose digest was verified against the stored one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LearningJobEvidence {
    pub prompt: String,
    pub evidence_json: String,
}

#[derive(Debug)]
pub(crate) enum Write {
    Admit(Box<NewLearningJob>, i64),
    Start {
        task_id: String,
        instance: String,
        at_ns: i64,
    },
    Finish {
        task_id: String,
        instance: String,
        ok: bool,
        at_ns: i64,
    },
    Adopt {
        task_id: String,
        from_instance: String,
        revision: u64,
        to: Box<LearningOrigin>,
    },
    Abandon {
        task_id: String,
        instance: String,
        reason: String,
        at_ns: i64,
    },
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Written {
    Admission(LearningAdmission),
    Applied(bool),
}

pub(crate) fn write(conn: &Connection, write: Write) -> Result<Written, StorageError> {
    match write {
        Write::Admit(job, at_ns) => admit(conn, &job, at_ns).map(Written::Admission),
        Write::Start {
            task_id,
            instance,
            at_ns,
        } => in_transaction(conn, |tx| {
            Ok(tx.execute(
                "UPDATE learning_jobs SET state='Started', started_at_ns=?3, revision=revision+1
                 WHERE task_id=?1 AND state='Pending' AND origin_instance=?2",
                params![task_id, instance, at_ns],
            )? == 1)
        })
        .map(Written::Applied),
        Write::Finish {
            task_id,
            instance,
            ok,
            at_ns,
        } => in_transaction(conn, |tx| {
            let state = if ok { "Finished" } else { "Failed" };
            end(tx, &task_id, &instance, state, None, at_ns, &["Started"])
        })
        .map(Written::Applied),
        Write::Adopt {
            task_id,
            from_instance,
            revision,
            to,
        } => in_transaction(conn, |tx| {
            Ok(tx.execute(
                "UPDATE learning_jobs SET origin_instance=?4, origin_pid=?5, origin_start_ticks=?6,
                        origin_boot_id=?7, origin_pid_ns=?8, config_identity=?9, revision=revision+1
                 WHERE task_id=?1 AND state='Pending' AND origin_kind='Core'
                   AND origin_instance=?2 AND revision=?3",
                params![
                    task_id,
                    from_instance,
                    i64::try_from(revision).map_err(|_| StorageError::OutOfRange("revision"))?,
                    to.instance,
                    i64::from(to.pid),
                    to.start_ticks
                        .map(i64::try_from)
                        .transpose()
                        .map_err(|_| StorageError::OutOfRange("start ticks"))?,
                    to.boot_id,
                    to.pid_ns,
                    to.config_identity,
                ],
            )? == 1)
        })
        .map(Written::Applied),
        Write::Abandon {
            task_id,
            instance,
            reason,
            at_ns,
        } => in_transaction(conn, |tx| {
            end(
                tx,
                &task_id,
                &instance,
                "Abandoned",
                Some(&reason),
                at_ns,
                &["Pending", "Started"],
            )
        })
        .map(Written::Applied),
    }
}

fn in_transaction<T>(
    conn: &Connection,
    body: impl FnOnce(&rusqlite::Transaction<'_>) -> Result<T, StorageError>,
) -> Result<T, StorageError> {
    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
    let value = body(&tx)?;
    tx.commit()?;
    Ok(value)
}

/// Ends a job owned by `instance` and deletes its evidence in the same transaction.
fn end(
    tx: &rusqlite::Transaction<'_>,
    task_id: &str,
    instance: &str,
    state: &str,
    reason: Option<&str>,
    at_ns: i64,
    from: &[&str],
) -> Result<bool, StorageError> {
    let current: Option<(String, String)> = tx
        .query_row(
            "SELECT state, origin_instance FROM learning_jobs WHERE task_id=?1",
            [task_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((current_state, owner)) = current else {
        return Ok(false);
    };
    if owner != instance || !from.contains(&current_state.as_str()) {
        return Ok(false);
    }
    tx.execute(
        "UPDATE learning_jobs SET state=?2, reason=?3, finished_at_ns=?4, revision=revision+1
         WHERE task_id=?1",
        params![task_id, state, reason, at_ns],
    )?;
    tx.execute(
        "DELETE FROM learning_job_evidence WHERE task_id=?1",
        [task_id],
    )?;
    Ok(true)
}

fn admit(
    conn: &Connection,
    job: &NewLearningJob,
    at_ns: i64,
) -> Result<LearningAdmission, StorageError> {
    // UTC days, shared across reopen and concurrent runtime instances.
    let day = at_ns.div_euclid(86_400_000_000_000);
    in_transaction(conn, |tx| {
        if !crate::learning::claim_in(tx, &job.task_id, &job.roots, job.goal, job.daily_limit, day)?
        {
            return refusal(tx, job, day);
        }
        let origin = &job.origin;
        tx.execute(
            "INSERT INTO learning_jobs(task_id, state, is_goal, project, source_run_id,
                 origin_kind, origin_instance, origin_pid, origin_start_ticks, origin_boot_id,
                 origin_pid_ns, config_identity, admitted_at_ns)
             VALUES(?1,'Pending',?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
            params![
                job.task_id,
                job.goal,
                job.project,
                job.source_run_id,
                origin.kind.as_str(),
                origin.instance,
                i64::from(origin.pid),
                origin
                    .start_ticks
                    .map(i64::try_from)
                    .transpose()
                    .map_err(|_| StorageError::OutOfRange("start ticks"))?,
                origin.boot_id,
                origin.pid_ns,
                origin.config_identity,
                at_ns,
            ],
        )?;
        tx.execute(
            "INSERT INTO learning_job_evidence(task_id, prompt, evidence_json, sha256)
             VALUES(?1,?2,?3,?4)",
            params![
                job.task_id,
                job.prompt,
                job.evidence_json,
                digest(&job.prompt, &job.evidence_json)
            ],
        )?;
        Ok(LearningAdmission::Admitted)
    })
}

/// Why `claim_in` refused, so a caller can tell a repeat from an exhausted quota.
fn refusal(
    tx: &rusqlite::Transaction<'_>,
    job: &NewLearningJob,
    day: i64,
) -> Result<LearningAdmission, StorageError> {
    let count: u32 = tx.query_row(
        "SELECT count(*) FROM learning_claims WHERE utc_day=?1",
        [day],
        |row| row.get(0),
    )?;
    let known: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM learning_claims WHERE task_id=?1)",
        [&job.task_id],
        |row| row.get(0),
    )?;
    let root_seen = job.roots.iter().any(|root| {
        tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM learning_claim_roots r JOIN learning_claims c USING(task_id)
             WHERE r.run_id=?1 AND (?2=0 OR c.is_goal=0))",
            params![root, job.goal],
            |row| row.get::<_, bool>(0),
        )
        .unwrap_or(true)
    });
    Ok(if known || root_seen || count < job.daily_limit {
        LearningAdmission::Duplicate
    } else {
        LearningAdmission::QuotaExhausted
    })
}

fn digest(prompt: &str, evidence_json: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update((prompt.len() as u64).to_le_bytes());
    hasher.update(prompt.as_bytes());
    hasher.update(evidence_json.as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn corrupt(message: String) -> StorageError {
    StorageError::Serialization(message)
}

const JOB_COLUMNS: &str = "task_id, state, is_goal, project, source_run_id, origin_kind,
    origin_instance, origin_pid, origin_start_ticks, origin_boot_id, origin_pid_ns,
    config_identity, revision, admitted_at_ns, started_at_ns, finished_at_ns, reason";

fn job_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Result<LearningJob, StorageError>> {
    let kind: String = row.get(5)?;
    let state: String = row.get(1)?;
    let pid: i64 = row.get(7)?;
    let ticks: Option<i64> = row.get(8)?;
    let job = (|| {
        Ok(LearningJob {
            task_id: row.get(0)?,
            state: LearningJobState::parse(&state)?,
            goal: row.get(2)?,
            project: row.get(3)?,
            source_run_id: row.get(4)?,
            origin: LearningOrigin {
                kind: LearningOriginKind::parse(&kind)?,
                instance: row.get(6)?,
                pid: u32::try_from(pid).map_err(|_| StorageError::OutOfRange("pid"))?,
                start_ticks: ticks
                    .map(u64::try_from)
                    .transpose()
                    .map_err(|_| StorageError::OutOfRange("start ticks"))?,
                boot_id: row.get(9)?,
                pid_ns: row.get(10)?,
                config_identity: row.get(11)?,
            },
            revision: row.get(12)?,
            admitted_at_ns: row.get(13)?,
            started_at_ns: row.get(14)?,
            finished_at_ns: row.get(15)?,
            reason: row.get(16)?,
        })
    })();
    Ok(job)
}

impl Database {
    pub fn learning_job(&self, task_id: &str) -> Result<Option<LearningJob>, StorageError> {
        self.conn
            .query_row(
                &format!("SELECT {JOB_COLUMNS} FROM learning_jobs WHERE task_id=?1"),
                [task_id],
                job_row,
            )
            .optional()?
            .transpose()
    }

    /// Jobs in any of `states`, oldest admission first.
    pub fn learning_jobs(
        &self,
        states: &[LearningJobState],
    ) -> Result<Vec<LearningJob>, StorageError> {
        let mut statement = self.conn.prepare(&format!(
            "SELECT {JOB_COLUMNS} FROM learning_jobs ORDER BY admitted_at_ns, task_id"
        ))?;
        let mut jobs = Vec::new();
        for row in statement.query_map([], job_row)? {
            let job = row??;
            if states.contains(&job.state) {
                jobs.push(job);
            }
        }
        Ok(jobs)
    }

    /// The evidence of a live job; `None` once the job ended. A digest mismatch is an error.
    pub fn learning_job_evidence(
        &self,
        task_id: &str,
    ) -> Result<Option<LearningJobEvidence>, StorageError> {
        let row: Option<(String, String, String)> = self
            .conn
            .query_row(
                "SELECT prompt, evidence_json, sha256 FROM learning_job_evidence WHERE task_id=?1",
                [task_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        let Some((prompt, evidence_json, stored)) = row else {
            return Ok(None);
        };
        if digest(&prompt, &evidence_json) != stored {
            return Err(corrupt(format!(
                "learning evidence of {task_id} does not match its digest"
            )));
        }
        Ok(Some(LearningJobEvidence {
            prompt,
            evidence_json,
        }))
    }
}

#[cfg(test)]
mod tests;
