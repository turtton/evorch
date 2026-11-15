//! Durable learning membership and bounded boot diagnostics. Neither grants authority.
use super::*;
use crate::learning_jobs::{
    self, LearningAttemptChange, LearningAttemptKey, LearningAttemptWrite, LearningEndReason,
    LearningJob, LearningJobState, LearningOrigin, LearningOriginKind,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LearningMember {
    pub key: LearningAttemptKey,
    pub suspension_id: Option<String>,
    pub evidence_digest: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LearningInventory {
    pub origin: LearningOrigin,
    pub suspension_id: String,
    pub members: Vec<LearningMember>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LearningHandoffStatus {
    pub member: LearningMember,
    pub disposition: String,
    pub result: Option<LearningAttemptKey>,
}
/// Small diagnostic codes only. A receipt is never read as eligibility evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BootFailure {
    Bootstrap,
    AdoptRejected,
    AckUnknown,
    Interrupted,
    Terminated,
    BeforeReady,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RestartBootProgress {
    pub restarting_at_ms: u64,
    pub failure: Option<BootFailure>,
    pub exit_code: Option<i32>,
    pub diagnostic_revision: u64,
}
fn job(conn: &Connection, id: &str) -> Result<Option<LearningJob>, StorageError> {
    conn.query_row(
        &format!(
            "SELECT {} FROM learning_jobs WHERE task_id=?1",
            learning_jobs::JOB_COLUMNS
        ),
        [id],
        learning_jobs::job_row,
    )
    .optional()?
    .transpose()
}
fn evidence_digest(conn: &Connection, id: &str) -> Result<String, StorageError> {
    let (prompt, evidence, saved): (String, String, String) = conn.query_row(
        "SELECT prompt,evidence_json,sha256 FROM learning_job_evidence WHERE task_id=?1",
        [id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    // Use the queue's canonical content validation as well as the fixed identity.
    let digest = learning_jobs::digest(&prompt, &evidence);
    if digest != saved {
        return Err(invalid("learning evidence digest mismatch"));
    }
    Ok(digest)
}
pub(super) fn inventory(
    conn: &Connection,
    origin: LearningOrigin,
    suspension_id: String,
) -> Result<LearningInventory, StorageError> {
    let mut q=conn.prepare(&format!("SELECT {} FROM learning_jobs WHERE origin_instance=?1 AND state IN ('Pending','Started','Interrupted') ORDER BY task_id", learning_jobs::JOB_COLUMNS))?;
    let jobs = q
        .query_map([&origin.instance], learning_jobs::job_row)?
        .collect::<Result<Vec<_>, _>>()?;
    let mut members = Vec::new();
    for job in jobs {
        let job = job?;
        if job.origin != origin
            || job.state == LearningJobState::Started
            || (job.state == LearningJobState::Interrupted
                && job.suspension_id.as_deref() != Some(&suspension_id))
        {
            return Err(invalid("learning cohort has not settled"));
        }
        members.push(LearningMember {
            key: job.key(),
            suspension_id: job.suspension_id,
            evidence_digest: evidence_digest(conn, &job.task_id)?,
        });
    }
    Ok(LearningInventory {
        origin,
        suspension_id,
        members,
    })
}
impl Database {
    pub fn restart_learning_inventory(
        &self,
        origin: LearningOrigin,
        suspension_id: String,
    ) -> Result<LearningInventory, StorageError> {
        inventory(&self.conn, origin, suspension_id)
    }
    pub fn restart_saved_learning_inventory(
        &self,
        id: &str,
    ) -> Result<Option<LearningInventory>, StorageError> {
        let raw: Option<String> = self
            .conn
            .query_row(
                "SELECT inventory_json FROM restart_learning_inventory WHERE restart_id=?1",
                [id],
                |r| r.get(0),
            )
            .optional()?;
        raw.map(|s| decode(&s)).transpose()
    }
    pub fn restart_learning_members(
        &self,
        id: &str,
    ) -> Result<Vec<LearningHandoffStatus>, StorageError> {
        let mut q=self.conn.prepare("SELECT member_json,disposition,result_key_json FROM restart_learning_members WHERE restart_id=?1 ORDER BY task_id")?;
        q.query_map([id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get(1)?,
                r.get::<_, Option<String>>(2)?,
            ))
        })?
        .map(|row| {
            let (m, disposition, result) = row?;
            Ok(LearningHandoffStatus {
                member: decode(&m)?,
                disposition,
                result: result.map(|r| decode(&r)).transpose()?,
            })
        })
        .collect()
    }
    pub fn restart_boot_progress(
        &self,
        id: &str,
    ) -> Result<Option<RestartBootProgress>, StorageError> {
        let value: Option<String> = self
            .conn
            .query_row(
                "SELECT boot_progress_json FROM restart_transactions WHERE restart_id=?1",
                [id],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        value.map(|s| decode(&s)).transpose()
    }
}
pub(super) fn save_inventory(
    conn: &Connection,
    tx: &RestartTransaction,
    value: LearningInventory,
) -> Result<(), StorageError> {
    phase(tx.phase, RestartPhase::Capturing)?;
    if value.origin.kind != LearningOriginKind::Core
        || value.origin.config_identity.as_deref() != Some(&tx.metadata.config_identity)
        || value.origin.instance.is_empty()
        || value.suspension_id.is_empty()
        || value.suspension_id.len() > 256
        || inventory(conn, value.origin.clone(), value.suspension_id.clone())? != value
    {
        return Err(invalid("learning handoff inventory mismatch"));
    }
    let encoded = encode(&value)?;
    exact_json(&encoded)?;
    let prior: Option<String> = conn
        .query_row(
            "SELECT inventory_json FROM restart_learning_inventory WHERE restart_id=?1",
            [&tx.metadata.restart_id],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(prior) = prior {
        if prior == encoded {
            return Ok(());
        }
        return Err(invalid("learning inventory already fixed"));
    }
    conn.execute(
        "INSERT INTO restart_learning_inventory VALUES(?1,?2)",
        params![tx.metadata.restart_id, encoded],
    )?;
    for member in value.members {
        conn.execute(
            "INSERT INTO restart_learning_members(restart_id,task_id,member_json) VALUES(?1,?2,?3)",
            params![tx.metadata.restart_id, member.key.task_id, encode(&member)?],
        )?;
    }
    Ok(())
}
pub(super) fn resume(
    conn: &Connection,
    tx: &RestartTransaction,
    id: &str,
    to: &LearningOrigin,
) -> Result<(), StorageError> {
    if !matches!(tx.phase, RestartPhase::Adopted | RestartPhase::Completed)
        || tx.ready_at_ms.is_none()
        || tx.adopted_epoch != Some(tx.metadata.successor_epoch)
        || to.kind != LearningOriginKind::Core
        || to.config_identity.as_deref() != Some(&tx.metadata.config_identity)
    {
        return Err(invalid("learning requires adopted Ready successor"));
    }
    let (encoded, disposition):(String,String)=conn.query_row("SELECT member_json,disposition FROM restart_learning_members WHERE restart_id=?1 AND task_id=?2",params![tx.metadata.restart_id,id],|r|Ok((r.get(0)?,r.get(1)?)))?;
    // Receipt/readback of the same member never requeues it again, even after Start.
    if disposition != "pending" {
        return Ok(());
    }
    let raw: String = conn.query_row(
        "SELECT inventory_json FROM restart_learning_inventory WHERE restart_id=?1",
        [&tx.metadata.restart_id],
        |r| r.get(0),
    )?;
    let inventory: LearningInventory = decode(&raw)?;
    if to.instance == inventory.origin.instance {
        return Err(invalid("successor learning origin did not change"));
    }
    let member: LearningMember = decode(&encoded)?;
    let current = job(conn, id)?.ok_or_else(|| invalid("learning member missing"))?;
    if current.key() != member.key
        || current.origin != inventory.origin
        || current.suspension_id != member.suspension_id
        || evidence_digest(conn, id)? != member.evidence_digest
    {
        return Err(invalid("learning member changed"));
    }
    let at = crate::system_time_to_ns(std::time::SystemTime::now())?;
    let disposition = if current.state == LearningJobState::Interrupted {
        if !learning_jobs::attempts::apply_in_transaction(
            conn,
            LearningAttemptWrite {
                key: member.key,
                change: LearningAttemptChange::Requeue {
                    suspension_id: inventory.suspension_id,
                    to: to.clone(),
                },
            },
            at,
        )? {
            return Err(invalid("learning requeue rejected"));
        }
        if current.attempt >= current.max_attempts {
            "attempt_limit"
        } else {
            "requeued"
        }
    } else if current.state == LearningJobState::Pending {
        conn.execute("UPDATE learning_jobs SET origin_instance=?2,origin_pid=?3,origin_start_ticks=?4,origin_boot_id=?5,origin_pid_ns=?6,revision=revision+1 WHERE task_id=?1",params![id,to.instance,to.pid,to.start_ticks,to.boot_id,to.pid_ns])?;
        "requeued"
    } else {
        return Err(invalid("learning member is not resumable"));
    };
    let result = job(conn, id)?
        .ok_or_else(|| invalid("learning result missing"))?
        .key();
    conn.execute("UPDATE restart_learning_members SET disposition=?3,result_key_json=?4 WHERE restart_id=?1 AND task_id=?2",params![tx.metadata.restart_id,id,disposition,encode(&result)?])?;
    Ok(())
}
/// Cold boot never requeues Interrupted. Exact membership fences prevent harming a later origin.
pub(super) fn quarantine_learning(conn: &Connection, id: &str) -> Result<(), StorageError> {
    let mut q=conn.prepare("SELECT task_id,member_json FROM restart_learning_members WHERE restart_id=?1 AND disposition='pending'")?;
    let rows = q
        .query_map([id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    for (task, raw) in rows {
        // Corrupt membership grants no authority over a job and cannot block cold graph quarantine.
        let member: Option<LearningMember> = decode(&raw).ok();
        if let Some(member) = member
            && let Some(job) = job(conn, &task)?
            && job.key() == member.key
        {
            learning_jobs::attempts::apply_in_transaction(
                conn,
                LearningAttemptWrite {
                    key: member.key,
                    change: LearningAttemptChange::Abandon(LearningEndReason::NoResumeAuthority),
                },
                crate::system_time_to_ns(std::time::SystemTime::now())?,
            )?;
        }
        conn.execute("UPDATE restart_learning_members SET disposition='quarantined' WHERE restart_id=?1 AND task_id=?2",params![id,task])?;
    }
    Ok(())
}
pub(super) fn save_boot_progress(
    conn: &Connection,
    tx: &RestartTransaction,
    value: &RestartBootProgress,
) -> Result<(), StorageError> {
    if !matches!(
        tx.phase,
        RestartPhase::Armed | RestartPhase::Adopted | RestartPhase::Completed
    ) {
        return Err(invalid("boot progress requires armed restart"));
    }
    let raw: Option<String> = conn.query_row(
        "SELECT boot_progress_json FROM restart_transactions WHERE restart_id=?1",
        [&tx.metadata.restart_id],
        |r| r.get(0),
    )?;
    if let Some(raw) = raw {
        let prior: RestartBootProgress = decode(&raw)?;
        if prior == *value {
            return Ok(());
        }
        if prior.restarting_at_ms != value.restarting_at_ms
            || prior.diagnostic_revision >= value.diagnostic_revision
        {
            return Err(invalid("boot diagnostic revision conflict"));
        }
    }
    let expected = match value.failure {
        None => None,
        Some(BootFailure::Bootstrap) => Some(70),
        Some(BootFailure::AdoptRejected) => Some(71),
        Some(BootFailure::AckUnknown) => Some(72),
        Some(BootFailure::Interrupted) => Some(130),
        Some(BootFailure::Terminated) => Some(143),
        Some(BootFailure::BeforeReady) => Some(73),
    };
    if value.exit_code != expected {
        return Err(invalid("boot diagnostic code mismatch"));
    }
    conn.execute(
        "UPDATE restart_transactions SET boot_progress_json=?2 WHERE restart_id=?1",
        params![tx.metadata.restart_id, encode(value)?],
    )?;
    Ok(())
}
