//! One IMMEDIATE transaction accepts an instruction: authorization, both revision CAS
//! checks, the brief change, the immutable text, the delivery reservation and the
//! operation receipt commit together or not at all.
use crate::{
    StorageError, lane_instructions::*, lane_management::LaneManagementError, lanes::LaneRecord,
};
use event_bus::lane::{LaneNotification, LaneNotificationKind};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Serialize, de::DeserializeOwned};

fn err(e: LaneManagementError) -> StorageError {
    StorageError::LaneManagement(e)
}
fn json(v: &impl Serialize) -> Result<String, StorageError> {
    serde_json::to_string(v).map_err(|e| StorageError::Serialization(e.to_string()))
}
fn parse<T: DeserializeOwned>(s: &str) -> Result<T, StorageError> {
    serde_json::from_str(s).map_err(|e| StorageError::Serialization(e.to_string()))
}
fn id(s: &str) -> Result<(), StorageError> {
    if s.trim().is_empty() || s.len() > 256 || s.chars().any(char::is_control) {
        return Err(err(LaneManagementError::InvalidInput));
    }
    Ok(())
}
fn lane(c: &Connection, id: &str) -> Result<LaneRecord, StorageError> {
    crate::repo::lanes::get(c, id)?.ok_or_else(|| StorageError::LaneNotFound { lane_id: id.into() })
}
fn to_i64(v: u64) -> Result<i64, StorageError> {
    i64::try_from(v).map_err(|_| StorageError::OutOfRange("lane instruction counter"))
}
fn validate(r: &LaneInstructionRequest, a: &LaneInstructionActor) -> Result<(), StorageError> {
    id(&r.operation_id)?;
    id(&r.lane_id)?;
    id(&a.thread_id)?;
    id(&a.root_run_id)?;
    if r.expected_management_revision == 0 || r.expected_brief_revision == 0 {
        return Err(err(LaneManagementError::InvalidInput));
    }
    let guard = crate::entity::SecretGuard::from_env();
    match &r.action {
        LaneInstructionAction::Supplement { message } => {
            if message.trim().is_empty() || message.len() > LANE_SUPPLEMENT_MAX_BYTES {
                return Err(err(LaneManagementError::InvalidInput));
            }
            guard.check_text("lane_instruction", "message", message)?;
        }
        LaneInstructionAction::ReviseBrief { brief } => {
            brief.validate().map_err(StorageError::LaneInvalid)?;
        }
    }
    Ok(())
}
/// Only the lane's current lead, as its live lane-external root, may send.
fn authorize(c: &Connection, l: &LaneRecord, a: &LaneInstructionActor) -> Result<(), StorageError> {
    if !l.policy.is_managed()
        || l.lead_thread_id.as_deref() != Some(&a.thread_id)
        || !crate::repo::lane_management::root(c, &a.thread_id, &a.root_run_id)?
    {
        return Err(err(LaneManagementError::NotCurrentLead));
    }
    Ok(())
}

pub(crate) fn apply(
    c: &Connection,
    r: &LaneInstructionRequest,
    a: &LaneInstructionActor,
    at: i64,
) -> Result<LaneInstructionResult, StorageError> {
    apply_inner(c, r, a, at, |_| Ok(()))
}

/// The hook is private and compiled away in production; tests inject a failure at each
/// durable step and immediately before commit without a second production path.
fn apply_inner(
    c: &Connection,
    r: &LaneInstructionRequest,
    a: &LaneInstructionActor,
    at: i64,
    mut checkpoint: impl FnMut(u8) -> Result<(), StorageError>,
) -> Result<LaneInstructionResult, StorageError> {
    validate(r, a)?;
    let request = json(r)?;
    let actor = json(a)?;
    let tx = Transaction::new_unchecked(c, TransactionBehavior::Immediate)?;
    // 1. A retry survives later edits and is answered from the receipt. The sender must
    // still be the lead: a detach does not let the old lead observe or repeat itself.
    let old: Option<(String, String, String)> = tx
        .query_row(
            "SELECT request_json,actor_json,result_json FROM lane_instruction_receipts WHERE operation_id=?1",
            [&r.operation_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    if let Some((old_request, old_actor, result)) = old {
        if old_request != request || old_actor != actor {
            return Err(err(LaneManagementError::OperationIdConflict));
        }
        authorize(&tx, &lane(&tx, &r.lane_id)?, a)?;
        let mut result: LaneInstructionResult = parse(&result)?;
        result.replayed = true;
        return Ok(result);
    }
    checkpoint(1)?;
    // 2. Authenticated actor, then both fences. A detach raised the management
    // revision, so a stale lead is rejected whichever check sees it first.
    let old = lane(&tx, &r.lane_id)?;
    authorize(&tx, &old, a)?;
    if old.management_revision != r.expected_management_revision {
        return Err(err(LaneManagementError::StaleManagementRevision));
    }
    if old.root_run_id.is_none() || old.generation == 0 {
        return Err(err(LaneManagementError::LaneNotStarted));
    }
    checkpoint(2)?;
    // 3. Brief CAS and replacement. A supplement never touches the brief or its revision.
    let (kind, changed) = match &r.action {
        LaneInstructionAction::Supplement { .. } => {
            if old.brief_revision != r.expected_brief_revision {
                return Err(StorageError::StaleRevision {
                    kind: "brief",
                    expected: r.expected_brief_revision,
                    current: old.brief_revision,
                });
            }
            (LaneInstructionKind::Supplement, true)
        }
        LaneInstructionAction::ReviseBrief { brief } => {
            let update = crate::repo::lanes::revise_brief_in(
                &tx,
                &r.lane_id,
                r.expected_brief_revision,
                brief,
                at,
            )?;
            (LaneInstructionKind::BriefRevision, update.changed)
        }
    };
    checkpoint(3)?;
    let current = lane(&tx, &r.lane_id)?;
    let mut result = LaneInstructionResult {
        operation_id: r.operation_id.clone(),
        lane_id: r.lane_id.clone(),
        instruction_seq: None,
        notification_id: None,
        accepted_management_revision: current.management_revision,
        brief_revision: current.brief_revision,
        changed,
        replayed: false,
    };
    if changed {
        let seq: i64 = tx.query_row(
            "SELECT COALESCE(MAX(instruction_seq),0)+1 FROM lane_instructions WHERE lane_id=?1",
            [&r.lane_id],
            |row| row.get(0),
        )?;
        let seq = u64::try_from(seq).map_err(|_| StorageError::OutOfRange("instruction seq"))?;
        let body = Presentation {
            seq,
            lead_thread_id: &a.thread_id,
            management_revision: current.management_revision,
            brief_revision_before: old.brief_revision,
            brief_revision_after: current.brief_revision,
            action: &r.action,
            stored_brief: &current.brief,
        }
        .render()?;
        // 4. Reserve delivery. The outbox row only references the immutable text.
        let notification_id = super::lane_delivery::enqueue(
            &tx,
            LaneNotification {
                notification_id: String::new(),
                lane_id: r.lane_id.clone(),
                management_revision: current.management_revision,
                source_thread_id: a.thread_id.clone(),
                destination_thread_id: current.thread_id.clone(),
                kind: LaneNotificationKind::Instruction {
                    instruction_seq: seq,
                },
                report_seq: None,
                created_at_ns: at,
            },
        )?;
        checkpoint(4)?;
        tx.execute(
            "INSERT INTO lane_instructions(lane_id,instruction_seq,kind,operation_id,body,lead_thread_id,lead_root_run_id,accepted_management_revision,brief_revision_before,brief_revision_after,notification_id,created_at_ns) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
            params![
                r.lane_id,
                to_i64(seq)?,
                kind.as_str(),
                r.operation_id,
                body,
                a.thread_id,
                a.root_run_id,
                to_i64(current.management_revision)?,
                to_i64(old.brief_revision)?,
                to_i64(current.brief_revision)?,
                notification_id,
                at
            ],
        )?;
        tx.execute("UPDATE lane_state SET revision=revision+1 WHERE id=1", [])?;
        result.instruction_seq = Some(seq);
        result.notification_id = Some(notification_id);
    }
    checkpoint(5)?;
    // 5. The durable result doubles as the retry record.
    tx.execute(
        "INSERT INTO lane_instruction_receipts(operation_id,lane_id,request_json,actor_json,result_json,created_at_ns) VALUES (?1,?2,?3,?4,?5,?6)",
        params![r.operation_id, r.lane_id, request, actor, json(&result)?, at],
    )?;
    checkpoint(6)?;
    tx.commit()?;
    Ok(result)
}

fn record_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<LaneInstructionRecord> {
    let kind: String = row.get(2)?;
    Ok(LaneInstructionRecord {
        lane_id: row.get(0)?,
        instruction_seq: row.get(1)?,
        kind: if kind == "brief_revision" {
            LaneInstructionKind::BriefRevision
        } else {
            LaneInstructionKind::Supplement
        },
        body: row.get(3)?,
        lead_thread_id: row.get(4)?,
        accepted_management_revision: row.get(5)?,
        brief_revision_before: row.get(6)?,
        brief_revision_after: row.get(7)?,
        notification_id: row.get(8)?,
        created_at_ns: row.get(9)?,
    })
}
const RECORD: &str = "lane_id,instruction_seq,kind,body,lead_thread_id,accepted_management_revision,brief_revision_before,brief_revision_after,notification_id,created_at_ns";

/// The immutable text for one instruction, or `None` once its lane is gone.
pub(crate) fn get(
    c: &Connection,
    lane_id: &str,
    seq: u64,
) -> Result<Option<LaneInstructionRecord>, StorageError> {
    Ok(c.query_row(
        &format!(
            "SELECT {RECORD} FROM lane_instructions WHERE lane_id=?1 AND instruction_seq=?2 AND lane_id IN (SELECT lane_id FROM lanes WHERE thread_id NOT IN (SELECT thread_id FROM thread_tombstones))"
        ),
        params![lane_id, to_i64(seq)?],
        record_from_row,
    )
    .optional()?)
}

/// Accepted instructions after `after_seq` in sequence order.
pub(crate) fn list(
    c: &Connection,
    lane_id: &str,
    after_seq: u64,
    limit: usize,
) -> Result<Vec<LaneInstructionRecord>, StorageError> {
    let mut statement = c.prepare(&format!(
        "SELECT {RECORD} FROM lane_instructions WHERE lane_id=?1 AND instruction_seq>?2 ORDER BY instruction_seq LIMIT ?3"
    ))?;
    Ok(statement
        .query_map(
            params![
                lane_id,
                to_i64(after_seq)?,
                i64::try_from(limit.clamp(1, 200)).unwrap_or(200)
            ],
            record_from_row,
        )?
        .collect::<Result<_, _>>()?)
}

pub(crate) fn delivery(
    c: &Connection,
    lane_id: &str,
    seq: u64,
) -> Result<Option<LaneInstructionDelivery>, StorageError> {
    let row: Option<(String, bool)> = c
        .query_row(
            "SELECT n.state,n.receipt_prompt_id IS NOT NULL FROM lane_instructions i JOIN lane_notifications n ON n.notification_id=i.notification_id WHERE i.lane_id=?1 AND i.instruction_seq=?2",
            params![lane_id, to_i64(seq)?],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    // The conversation snapshot and the receipt commit together; the ACK is a later
    // step that can fail or be lost, so the receipt, not the ACK, proves delivery.
    Ok(row.map(|(state, received)| match state.as_str() {
        "acked" => LaneInstructionDelivery::Delivered,
        _ if received => LaneInstructionDelivery::Delivered,
        "obsolete" => LaneInstructionDelivery::Withdrawn,
        _ => LaneInstructionDelivery::Queued,
    }))
}

#[cfg(test)]
#[path = "lane_instructions/tests.rs"]
mod tests;
