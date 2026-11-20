//! Batch owner checks and diagnostic receipt cleanup. Owner leases stay Running.
use super::*;
use crate::DisposalAuthority;
use rusqlite::{OptionalExtension, TransactionBehavior, params};
use std::collections::BTreeSet;
use storage::recovery::graph_control::GraphEffectKey;

fn check(registry: &Registry, authority: &DisposalAuthority) -> Result<(), RegistryError> {
    for (thread, permit) in &authority.permits {
        let actual = registry.attach(thread);
        match (permit, actual) {
            (None, Err(RegistryError::Absent)) => (),
            (Some(permit), Ok(owner)) => {
                owner.validate(&permit.lease)?;
                if owner.state != OwnerState::Running
                    || owner.handoff_hold.is_some()
                    || owner.lease.expires_at <= permit::now_ms()
                {
                    return Err(OwnershipError::Fenced.into());
                }
                let hold = owner
                    .planned_claim
                    .as_ref()
                    .ok_or(OwnershipError::PlannedMismatch)?;
                let members = authority
                    .expected
                    .membership
                    .iter()
                    .filter(|m| m.thread_id.as_ref() == Some(thread))
                    .collect::<Vec<_>>();
                if hold.transaction_id != authority.expected.restart_id
                    || hold.capture_id != authority.expected.capture_id
                    || hold.orphaned
                    || hold
                        .active_runs
                        .union(&hold.parked_runs)
                        .cloned()
                        .collect::<BTreeSet<_>>()
                        != members.iter().map(|m| m.run_id.clone()).collect()
                    || members.iter().any(|m| {
                        m.owner_id.as_ref() != Some(&hold.owner_id)
                            || m.owner_generation != Some(hold.generation)
                    })
                {
                    return Err(OwnershipError::PlannedMismatch.into());
                }
            }
            (_, Err(error)) => return Err(error),
            _ => return Err(OwnershipError::Fenced.into()),
        }
    }
    Ok(())
}
pub(crate) fn guard(authority: &DisposalAuthority) -> Result<Registry, RegistryError> {
    let registry = Registry::open_readonly(&authority.registry_path)?;
    registry.connection.execute_batch("BEGIN DEFERRED")?;
    check(&registry, authority)?;
    Ok(registry)
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DisposalOwnerReceipt {
    pub effect: GraphEffectKey,
    pub manifest_id: String,
    pub cleaned_threads: Vec<String>,
    pub shared_threads: Vec<String>,
}
/// Only the selected planned receipt changes, never ownership identity/state.
pub(crate) fn cleanup(
    authority: &DisposalAuthority,
    effect: GraphEffectKey,
    shared: &BTreeSet<String>,
) -> Result<DisposalOwnerReceipt, RegistryError> {
    let mut registry = Registry::open_existing(&authority.registry_path)?;
    let tx = registry
        .connection
        .transaction_with_behavior(TransactionBehavior::Immediate)?;
    let existing: Option<String> = tx
        .query_row(
            "SELECT receipt_json FROM disposal_cleanup_receipts WHERE operation_id=?1",
            [&effect.operation_id],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(json) = existing {
        let receipt: DisposalOwnerReceipt = serde_json::from_str(&json)?;
        if receipt.effect != effect
            || receipt.manifest_id != authority.expected.manifest.manifest_id
            || receipt
                .shared_threads
                .iter()
                .cloned()
                .collect::<BTreeSet<_>>()
                != *shared
        {
            return Err(OwnershipError::PlannedMismatch.into());
        }
        return Ok(receipt);
    }
    // This connection already has an IMMEDIATE reservation. Reuse the same
    // snapshot for the batch check rather than opening a second reader.
    for (thread, permit) in &authority.permits {
        let json: Option<String> = tx
            .query_row(
                "SELECT state FROM thread_owners WHERE thread_id=?1",
                [thread],
                |r| r.get(0),
            )
            .optional()?;
        match (permit, json) {
            (None, None) => (),
            (Some(permit), Some(json)) => {
                let owner: ThreadOwner = serde_json::from_str(&json)?;
                owner.validate(&permit.lease)?;
                if owner.state != OwnerState::Running
                    || owner.handoff_hold.is_some()
                    || owner.lease.expires_at <= permit::now_ms()
                    || owner.planned_claim.as_ref().is_none_or(|h| {
                        h.transaction_id != authority.expected.restart_id
                            || h.capture_id != authority.expected.capture_id
                    })
                {
                    return Err(OwnershipError::Fenced.into());
                }
            }
            _ => return Err(OwnershipError::Fenced.into()),
        }
    }
    let mut receipt = DisposalOwnerReceipt {
        effect,
        manifest_id: authority.expected.manifest.manifest_id.clone(),
        cleaned_threads: Vec::new(),
        shared_threads: Vec::new(),
    };
    for (thread, permit) in &authority.permits {
        if permit.is_none() {
            continue;
        }
        if shared.contains(thread) {
            receipt.shared_threads.push(thread.clone());
            continue;
        }
        let json: String = tx.query_row(
            "SELECT state FROM thread_owners WHERE thread_id=?1",
            [thread],
            |r| r.get(0),
        )?;
        let mut owner: ThreadOwner = serde_json::from_str(&json)?;
        owner.planned_claim = None;
        tx.execute(
            "UPDATE thread_owners SET state=?2 WHERE thread_id=?1",
            params![thread, serde_json::to_string(&owner)?],
        )?;
        receipt.cleaned_threads.push(thread.clone());
    }
    tx.execute(
        "INSERT INTO disposal_cleanup_receipts(operation_id,receipt_json) VALUES(?1,?2)",
        params![
            receipt.effect.operation_id,
            serde_json::to_string(&receipt)?
        ],
    )?;
    tx.commit()?;
    Ok(receipt)
}
