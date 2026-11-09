//! Atomic lane ledger contracts. No timers, providers or external processes.
use crate::{StorageError, lane_control::*, lanes::LaneRecord};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
mod attention;
mod episode;
mod forward;
mod verification;
pub(crate) use attention::*;
pub(crate) use episode::*;
pub(crate) use forward::*;
pub(crate) use verification::*;
fn err(reason: LaneOperationError) -> StorageError {
    StorageError::LaneOperation(reason)
}
fn tx(conn: &Connection) -> Result<Transaction<'_>, StorageError> {
    Ok(Transaction::new_unchecked(
        conn,
        TransactionBehavior::Immediate,
    )?)
}
fn json(value: &impl Serialize) -> Result<String, StorageError> {
    serde_json::to_string(value).map_err(|e| StorageError::Serialization(e.to_string()))
}
fn parse<T: DeserializeOwned>(s: &str) -> Result<T, StorageError> {
    serde_json::from_str(s).map_err(|e| StorageError::Serialization(e.to_string()))
}
fn digest(value: &impl Serialize) -> Result<String, StorageError> {
    Ok(format!("{:x}", Sha256::digest(json(value)?.as_bytes())))
}
fn bounded(value: &impl Serialize, limit: usize) -> Result<String, StorageError> {
    let s = json(value)?;
    if s.len() > limit {
        return Err(err(LaneOperationError::InvalidInput));
    }
    crate::entity::SecretGuard::from_env().check_text("lane_control", "payload", &s)?;
    Ok(s)
}
fn id(s: &str) -> Result<(), StorageError> {
    if s.trim().is_empty() || s.len() > 256 {
        Err(err(LaneOperationError::InvalidInput))
    } else {
        Ok(())
    }
}
fn lane(conn: &Connection, id: &str) -> Result<LaneRecord, StorageError> {
    crate::repo::lanes::get(conn, id)?
        .ok_or_else(|| StorageError::LaneNotFound { lane_id: id.into() })
}
fn managed(
    conn: &Connection,
    id: &str,
    revision: u64,
    lead: Option<&str>,
) -> Result<LaneRecord, StorageError> {
    let l = lane(conn, id)?;
    if !l.policy.is_managed() {
        return Err(err(LaneOperationError::NotAttached));
    }
    if l.management_revision != revision {
        return Err(err(LaneOperationError::StaleManagement));
    }
    let current = l
        .lead_thread_id
        .as_deref()
        .ok_or_else(|| err(LaneOperationError::NotAttached))?;
    if lead.is_some_and(|v| v != current) {
        return Err(err(LaneOperationError::NotCurrentLead));
    }
    crate::repo::pull_requests::tombstone_check(conn, current)?;
    Ok(l)
}
pub(crate) fn progress_anchor(
    conn: &Connection,
    lane_id: &str,
) -> Result<LaneProgressAnchor, StorageError> {
    lane(conn, lane_id)?;
    Ok(LaneProgressAnchor {
        epoch: conn.query_row(
            "SELECT progress_epoch FROM lanes WHERE lane_id=?1",
            [lane_id],
            |r| r.get(0),
        )?,
    })
}
/// Called in the same transaction as the source write. Lead-only user input
/// advances the nudge anchor, but cannot invalidate a lane verification.
pub(crate) fn input_progress(
    conn: &Connection,
    lane_id: &str,
    invalidate: bool,
    progress: bool,
    at: i64,
) -> Result<(), StorageError> {
    conn.execute("UPDATE lanes SET request_revision=request_revision+?2,progress_epoch=progress_epoch+?3,current_close_id=CASE WHEN ?2 THEN NULL ELSE current_close_id END WHERE lane_id=?1",params![lane_id,invalidate,progress])?;
    if progress {
        resolve_episodes(conn, lane_id, at)?;
        let current = lane(conn, lane_id)?;
        crate::repo::lane_delivery::resolve_recovery_destination(conn, &current.thread_id)?;
        if let Some(lead) = current.lead_thread_id {
            crate::repo::lane_delivery::resolve_recovery_destination(conn, &lead)?;
        }
    }
    Ok(())
}
pub(crate) fn report_progress(
    conn: &Connection,
    lane_id: &str,
    previous_seq: u64,
    input: &LaneReportInput,
    at: i64,
) -> Result<(), StorageError> {
    let previous:Option<(String,Option<String>)>=conn.query_row("SELECT status,artifact_revision_json FROM lane_reports WHERE lane_id=?1 AND report_seq=?2",params![lane_id,previous_seq],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
    let previous_artifact = previous
        .as_ref()
        .and_then(|(_, s)| s.as_deref())
        .map(parse::<ArtifactRevision>)
        .transpose()?
        .and_then(|r| r.fingerprint());
    let artifact = input
        .artifact_revision
        .as_ref()
        .and_then(ArtifactRevision::fingerprint);
    // Only a transition into a terminal status resolves a no-progress episode.
    // Working/Waiting, report numbering and questions cannot renew a nudge slot.
    let transition = matches!(input.status, LaneStatus::Ready | LaneStatus::Failed)
        && previous
            .as_ref()
            .is_none_or(|(s, _)| s != input.status.as_str());
    // A recovery failure is a diagnostic, not work progress or a repaired target.
    let recovery_failure = input.status == LaneStatus::Failed
        && input.producer == LaneReportProducer::Harness
        && input.reason_code.as_deref() == Some("restore_failed");
    input_progress(
        conn,
        lane_id,
        true,
        !recovery_failure && (transition || previous_artifact != artifact),
        at,
    )
}
