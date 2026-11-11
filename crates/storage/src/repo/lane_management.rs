//! All eight durable management steps share one commit. No scheduler/GUI side effects.
use crate::{
    StorageError,
    lane_management::*,
    lanes::{LanePolicy, LaneRecord},
};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Serialize, de::DeserializeOwned};
#[path = "lane_management/effects.rs"]
mod effects;
#[path = "lane_management/monitor_failure.rs"]
pub(crate) mod monitor_failure;
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
fn actor_key(a: &LaneManagementActor) -> Result<String, StorageError> {
    match a {
        LaneManagementActor::User => json(&("user",)),
        LaneManagementActor::Lead {
            thread_id,
            root_run_id,
        } => {
            id(thread_id)?;
            id(root_run_id)?;
            json(&("lead", thread_id, root_run_id))
        }
    }
}
/// A live, lane-external root Orchestrator. Thread and run pointers must agree.
pub(crate) fn root(c: &Connection, thread: &str, run: &str) -> Result<bool, StorageError> {
    Ok(c.query_row(
        "SELECT EXISTS(SELECT 1 FROM threads t JOIN run_contexts r ON r.run_id=t.root_run_id
        WHERE t.thread_id=?1 AND t.root_run_id=?2 AND t.chat_role='Orchestrator'
        AND r.role='Orchestrator' AND r.parent_run_id IS NULL
        AND t.thread_id NOT IN (SELECT thread_id FROM lanes)
        AND t.thread_id NOT IN (SELECT thread_id FROM thread_tombstones)
        AND r.run_id NOT IN (SELECT run_id FROM run_tombstones))",
        params![thread, run],
        |r| r.get(0),
    )?)
}
fn authorize(
    c: &Connection,
    l: &LaneRecord,
    a: &LaneManagementActor,
    action: &LaneManagementAction,
) -> Result<(), StorageError> {
    if matches!(action, LaneManagementAction::Reattach { .. })
        && !matches!(a, LaneManagementActor::User)
    {
        return Err(err(LaneManagementError::HumanActionRequired));
    }
    if let LaneManagementActor::Lead {
        thread_id,
        root_run_id,
    } = a
        && (l.lead_thread_id.as_deref() != Some(thread_id) || !root(c, thread_id, root_run_id)?)
    {
        return Err(err(LaneManagementError::NotCurrentLead));
    }
    Ok(())
}
fn next_relation(
    c: &Connection,
    l: &LaneRecord,
    r: &LaneManagementRequest,
    g: Option<LaneAttachGuard>,
) -> Result<(Option<String>, LanePolicy), StorageError> {
    match &r.action {
        LaneManagementAction::SetPolicy { policy } if l.policy.is_managed() => {
            Ok((l.lead_thread_id.clone(), (*policy).into()))
        }
        LaneManagementAction::Detach if l.policy.is_managed() => Ok((None, LanePolicy::Detached)),
        LaneManagementAction::Reattach {
            lead_thread_id,
            expected_lead_root_run_id,
        } if !l.policy.is_managed() => {
            id(lead_thread_id)?;
            id(expected_lead_root_run_id)?;
            let g = g.ok_or_else(|| err(LaneManagementError::StaleFamilyRevision))?;
            if g.pinned {
                return Err(err(LaneManagementError::LanePinned));
            }
            if g.archived {
                return Err(err(LaneManagementError::LaneArchived));
            }
            let (pinned,archived,same_project):(bool,bool,bool)=c.query_row("SELECT s.pinned,s.archived,COALESCE(s.project_id=t.project_id AND s.project_id<>'',0) FROM threads s LEFT JOIN threads t ON t.thread_id=?2 WHERE s.thread_id=?1",params![l.thread_id,lead_thread_id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
            if pinned {
                return Err(err(LaneManagementError::LanePinned));
            }
            if archived {
                return Err(err(LaneManagementError::LaneArchived));
            }
            if !same_project || !root(c, lead_thread_id, expected_lead_root_run_id)? {
                return Err(err(LaneManagementError::InvalidAttachTarget));
            }
            Ok((Some(lead_thread_id.clone()), LanePolicy::Summary))
        }
        _ => Err(err(LaneManagementError::InvalidPolicyTransition)),
    }
}
pub(crate) fn topology_revision(c: &Connection) -> Result<u64, StorageError> {
    Ok(c.query_row(
        "SELECT family_topology_revision FROM lane_state WHERE id=1",
        [],
        |r| r.get(0),
    )?)
}
pub(crate) fn apply(
    c: &Connection,
    r: &LaneManagementRequest,
    a: &LaneManagementActor,
    g: Option<LaneAttachGuard>,
    at: i64,
) -> Result<LaneManagementResult, StorageError> {
    apply_inner(c, r, a, g, at, |_| Ok(()))
}
/// The hook is private and compiled away in production; tests inject failure at
/// every durable step and immediately before commit without an alternative path.
fn apply_inner(
    c: &Connection,
    r: &LaneManagementRequest,
    a: &LaneManagementActor,
    g: Option<LaneAttachGuard>,
    at: i64,
    mut checkpoint: impl FnMut(u8) -> Result<(), StorageError>,
) -> Result<LaneManagementResult, StorageError> {
    id(&r.operation_id)?;
    id(&r.lane_id)?;
    let request = json(r)?;
    let actor = actor_key(a)?;
    crate::entity::SecretGuard::from_env().check_text("lane_management", "request", &request)?;
    let tx = Transaction::new_unchecked(c, TransactionBehavior::Immediate)?;
    // 1. Receipt lookup precedes revision checks: a retry survives subsequent edits.
    let old:Option<(String,String,String)>=tx.query_row("SELECT request_json,actor_json,result_json FROM lane_management_receipts WHERE operation_id=?1",[&r.operation_id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
    if let Some((old_request, old_actor, result)) = old {
        if old_request != request || old_actor != actor {
            return Err(err(LaneManagementError::OperationIdConflict));
        }
        let mut result: LaneManagementResult = parse(&result)?;
        result.replayed = true;
        return Ok(result);
    }
    checkpoint(1)?;
    // 2. Authenticated actor and all SQL-verifiable eligibility fences.
    let old = lane(&tx, &r.lane_id)?;
    authorize(&tx, &old, a, &r.action)?;
    if old.management_revision != r.expected_management_revision {
        return Err(err(LaneManagementError::StaleManagementRevision));
    }
    let (lead, policy) = next_relation(&tx, &old, r, g)?;
    checkpoint(2)?;
    let changed = old.lead_thread_id != lead || old.policy != policy;
    if changed {
        // 3. Management CAS. It is not progress, a run restart or a successful close.
        tx.execute("UPDATE lanes SET lead_thread_id=?2,policy=?3,management_revision=management_revision+1,
            detached_since_management_revision=CASE WHEN ?3='detached' THEN management_revision+1 ELSE NULL END,
            updated_at_ns=max(updated_at_ns,?4) WHERE lane_id=?1 AND management_revision=?5",params![old.lane_id,lead,policy.as_str(),at,old.management_revision])?;
        tx.execute("UPDATE lane_state SET revision=revision+1 WHERE id=1", [])?;
        checkpoint(3)?;
        let current = lane(&tx, &old.lane_id)?;
        // 4. Same question/requester, new responder fence. Already accepted answers stay.
        effects::questions(&tx, &old, &current)?;
        checkpoint(4)?;
        // 5. Re-stamp only retained kinds; fence and release entire touched batches.
        effects::notifications(&tx, &old, &current)?;
        effects::episodes(&tx, &old, &current, at)?;
        checkpoint(5)?;
        // 6. Historical Ready/verification remain immutable and revision-bound.
        tx.execute(
            "UPDATE lanes SET current_close_id=NULL WHERE lane_id=?1",
            [&old.lane_id],
        )?;
        checkpoint(6)?;
        // 7. Topology is distinct from ordinary ledger/report revisions.
        if old.policy.is_managed() != policy.is_managed() {
            tx.execute("UPDATE lane_state SET family_topology_revision=family_topology_revision+1 WHERE id=1",[])?;
            if policy == LanePolicy::Detached {
                tx.execute("UPDATE threads SET pinned=0,archived=0,archived_at_ns=NULL,pin_revision=pin_revision+1,archive_revision=archive_revision+1 WHERE thread_id=?1",[&old.thread_id])?;
            }
        }
        checkpoint(7)?;
    }
    let lane = lane(&tx, &r.lane_id)?;
    let projected: u64 = tx.query_row(
        "SELECT management_projection_revision FROM lanes WHERE lane_id=?1",
        [&r.lane_id],
        |r| r.get(0),
    )?;
    let result = LaneManagementResult {
        operation_id: r.operation_id.clone(),
        projection_pending: projected < lane.management_revision,
        lane,
        changed,
        replayed: false,
        family_topology_revision: topology_revision(&tx)?,
    };
    // 8. Durable result doubles as the repair/audit record; no lead notice or wake.
    tx.execute("INSERT INTO lane_management_receipts(operation_id,lane_id,request_json,actor_json,result_json,created_at_ns) VALUES (?1,?2,?3,?4,?5,?6)",params![r.operation_id,r.lane_id,request,actor,json(&result)?,at])?;
    checkpoint(8)?;
    tx.commit()?;
    Ok(result)
}
pub(crate) fn pending_projections(
    c: &Connection,
    limit: usize,
) -> Result<Vec<LaneRecord>, StorageError> {
    let mut s=c.prepare("SELECT lane_id FROM lanes WHERE management_projection_revision<management_revision AND thread_id NOT IN (SELECT thread_id FROM thread_tombstones) ORDER BY lane_id LIMIT ?1")?;
    let ids = s
        .query_map([limit.clamp(1, 200)], |r| r.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    ids.into_iter().map(|id| lane(c, &id)).collect()
}
pub(crate) fn ack_projection(c: &Connection, id: &str, expected: u64) -> Result<(), StorageError> {
    if c.execute("UPDATE lanes SET management_projection_revision=?2 WHERE lane_id=?1 AND management_revision=?2 AND thread_id NOT IN (SELECT thread_id FROM thread_tombstones)",params![id,expected])?!=1 {return Err(err(LaneManagementError::StaleManagementRevision));}
    Ok(())
}
#[cfg(test)]
#[path = "lane_management/tests.rs"]
mod tests;
