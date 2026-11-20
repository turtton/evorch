//! Conditional Q2 reset under an owners reservation; never claims or releases.
use super::*;
use crate::DisposalAuthority;
use rusqlite::{OptionalExtension, TransactionBehavior};

/// Execution's own current permits, never derived from disposal authority.
pub(crate) fn guard_resume(
    expected: &storage::recovery::graph_control::GraphControlCas,
    permits: &[OwnerPermit],
) -> Result<Registry, RegistryError> {
    use std::collections::BTreeSet;
    let threads = expected
        .membership
        .iter()
        .filter_map(|m| m.thread_id.as_deref())
        .collect::<BTreeSet<_>>();
    if permits.is_empty()
        || permits.len() > 64
        || permits
            .iter()
            .map(|p| p.thread_id.as_str())
            .collect::<BTreeSet<_>>()
            != threads
        || permits.len() != threads.len()
        || permits
            .iter()
            .any(|p| p.registry_path != permits[0].registry_path)
    {
        return Err(OwnershipError::PlannedMismatch.into());
    }
    let registry = Registry::open_readonly(&permits[0].registry_path)?;
    registry.connection.execute_batch("BEGIN DEFERRED")?;
    for permit in permits {
        let owner = registry.attach(&permit.thread_id)?;
        owner.validate(&permit.lease)?;
        let hold = owner
            .planned_claim
            .as_ref()
            .ok_or(OwnershipError::PlannedMismatch)?;
        if owner.state != OwnerState::Running
            || owner.handoff_hold.is_some()
            || owner.lease.expires_at <= permit::now_ms()
            || hold.orphaned
            || hold.transaction_id != expected.restart_id
            || hold.capture_id != expected.capture_id
        {
            return Err(OwnershipError::PlannedMismatch.into());
        }
        for member in expected
            .membership
            .iter()
            .filter(|m| m.thread_id.as_deref() == Some(&permit.thread_id))
        {
            if member.owner_id.as_ref() != Some(&hold.owner_id)
                || member.owner_generation != Some(hold.generation)
                || (!hold.active_runs.contains(&member.run_id)
                    && !hold.parked_runs.contains(&member.run_id))
            {
                return Err(OwnershipError::PlannedMismatch.into());
            }
        }
    }
    Ok(registry)
}

pub(crate) fn reset_unused(
    authority: &DisposalAuthority,
    thread: &str,
    expected: &Lease,
    reset: impl FnOnce() -> std::io::Result<Option<bool>>,
) -> Result<Option<bool>, RegistryError> {
    let Some(Some(permit)) = authority.permits().get(thread) else {
        return Ok(None);
    };
    if permit.lease.owner_id != expected.owner_id || permit.lease.generation != expected.generation
    {
        return Ok(None);
    }
    let mut registry = Registry::open_existing(authority.registry_path())?;
    registry
        .connection
        .busy_timeout(std::time::Duration::ZERO)?;
    let tx = registry
        .connection
        .transaction_with_behavior(TransactionBehavior::Immediate)?;
    // No runtime/main/scratch locks retained. Validate the complete inventory
    // while this owners write reservation excludes concurrent mutations.
    let _all_owners = super::disposal_support::guard(authority)?;
    let json: Option<String> = tx
        .query_row(
            "SELECT state FROM thread_owners WHERE thread_id=?1",
            [thread],
            |r| r.get(0),
        )
        .optional()?;
    let Some(json) = json else {
        return Ok(None);
    };
    let owner: ThreadOwner = serde_json::from_str(&json)?;
    owner.validate(expected)?;
    if owner.state != OwnerState::Running
        || owner.handoff_hold.is_some()
        || owner.planned_claim.is_some()
        || owner.lease.expires_at <= permit::now_ms()
    {
        return Ok(None);
    }
    Ok(reset()?)
}
