//! Batch owner checks and diagnostic receipt cleanup. Owner leases stay Running.
use super::*;
use crate::DisposalAuthority;
use rusqlite::{OptionalExtension, TransactionBehavior, params};
use std::collections::{BTreeMap, BTreeSet};
use storage::recovery::graph_control::GraphEffectKey;

fn was_cleaned(
    conn: &rusqlite::Connection,
    authority: &DisposalAuthority,
    thread: &str,
    permit: &OwnerPermit,
) -> Result<bool, RegistryError> {
    Ok(conn.query_row("SELECT EXISTS(SELECT 1 FROM disposal_cleanup_receipt_members WHERE restart_id=?1 AND capture_id=?2 AND thread_id=?3 AND owner_id=?4 AND owner_generation=?5 AND cleaned=1)",
        params![authority.expected.restart_id,authority.expected.capture_id,thread,permit.lease.owner_id,permit.lease.generation], |r|r.get(0))?)
}
fn check_owner(
    conn: &rusqlite::Connection,
    authority: &DisposalAuthority,
    thread: &str,
    permit: &Option<OwnerPermit>,
) -> Result<(), RegistryError> {
    let json: Option<String> = conn
        .query_row(
            "SELECT state FROM thread_owners WHERE thread_id=?1",
            [thread],
            |r| r.get(0),
        )
        .optional()?;
    match (permit, json) {
        (None, None) => Ok(()),
        (Some(permit), Some(json)) => {
            let owner: ThreadOwner = serde_json::from_str(&json)?;
            owner.validate(&permit.lease)?;
            if owner.state != OwnerState::Running
                || owner.handoff_hold.is_some()
                || owner.lease.expires_at <= permit::now_ms()
            {
                return Err(OwnershipError::Fenced.into());
            }
            match &owner.planned_claim {
                Some(hold) => {
                    let members = authority
                        .expected
                        .membership
                        .iter()
                        .filter(|m| m.thread_id.as_deref() == Some(thread))
                        .collect::<Vec<_>>();
                    let runs = hold
                        .active_runs
                        .union(&hold.parked_runs)
                        .collect::<BTreeSet<_>>();
                    if hold.transaction_id != authority.expected.restart_id
                        || hold.capture_id != authority.expected.capture_id
                        || hold.orphaned
                        || members.iter().any(|m| {
                            !runs.contains(&m.run_id)
                                || m.owner_id.as_ref() != Some(&hold.owner_id)
                                || m.owner_generation != Some(hold.generation)
                        })
                    {
                        return Err(OwnershipError::PlannedMismatch.into());
                    }
                    Ok(())
                }
                None if was_cleaned(conn, authority, thread, permit)? => Ok(()),
                None => Err(OwnershipError::PlannedMismatch.into()),
            }
        }
        _ => Err(OwnershipError::Fenced.into()),
    }
}
pub(crate) fn guard(authority: &DisposalAuthority) -> Result<Registry, RegistryError> {
    let registry = Registry::open_readonly(&authority.registry_path)?;
    registry.connection.execute_batch("BEGIN DEFERRED")?;
    for (thread, permit) in &authority.permits {
        check_owner(&registry.connection, authority, thread, permit)?;
    }
    Ok(registry)
}
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DisposalOwnerReceipt {
    pub effect: GraphEffectKey,
    pub manifest_id: String,
    pub restart_id: String,
    pub capture_id: String,
    pub owner_generations: BTreeMap<String, (String, u64)>,
    pub cleaned_threads: Vec<String>,
    pub shared_threads: Vec<String>,
}
fn read_on(
    conn: &rusqlite::Connection,
    operation: &str,
) -> Result<Option<DisposalOwnerReceipt>, RegistryError> {
    let json: Option<String> = conn
        .query_row(
            "SELECT receipt_json FROM disposal_cleanup_receipts WHERE operation_id=?1",
            [operation],
            |r| r.get(0),
        )
        .optional()?;
    json.map(|j| serde_json::from_str(&j).map_err(RegistryError::from))
        .transpose()
}
/// A diagnostic read; even a later generation cannot make an old receipt authority.
pub(crate) fn read(
    path: &std::path::Path,
    operation: &str,
) -> Result<Option<DisposalOwnerReceipt>, RegistryError> {
    read_on(&Registry::open_readonly(path)?.connection, operation)
}
pub(crate) fn cleanup(
    authority: &DisposalAuthority,
    effect: GraphEffectKey,
    shared: &BTreeSet<String>,
) -> Result<DisposalOwnerReceipt, RegistryError> {
    if shared
        .iter()
        .any(|thread| !authority.permits.contains_key(thread))
    {
        return Err(OwnershipError::PlannedMismatch.into());
    }
    let mut registry = Registry::open_existing(&authority.registry_path)?;
    let tx = registry
        .connection
        .transaction_with_behavior(TransactionBehavior::Immediate)?;
    let generations: BTreeMap<_, _> = authority
        .permits
        .iter()
        .filter_map(|(thread, p)| {
            p.as_ref().map(|p| {
                (
                    thread.clone(),
                    (p.lease.owner_id.clone(), p.lease.generation),
                )
            })
        })
        .collect();
    if let Some(receipt) = read_on(&tx, &effect.operation_id)? {
        if receipt.effect != effect
            || receipt.manifest_id != authority.expected.manifest.manifest_id
            || receipt.restart_id != authority.expected.restart_id
            || receipt.capture_id != authority.expected.capture_id
            || receipt.owner_generations != generations
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
    // Validate every expected owner, including absence, before the first update.
    for (thread, permit) in &authority.permits {
        check_owner(&tx, authority, thread, permit)?;
    }
    let mut receipt = DisposalOwnerReceipt {
        effect,
        manifest_id: authority.expected.manifest.manifest_id.clone(),
        restart_id: authority.expected.restart_id.clone(),
        capture_id: authority.expected.capture_id.clone(),
        owner_generations: generations,
        cleaned_threads: Vec::new(),
        shared_threads: Vec::new(),
    };
    for (thread, permit) in &authority.permits {
        let Some(permit) = permit else {
            continue;
        };
        let cleaned = !shared.contains(thread);
        if cleaned {
            let json: String = tx.query_row(
                "SELECT state FROM thread_owners WHERE thread_id=?1",
                [thread],
                |r| r.get(0),
            )?;
            let mut owner: ThreadOwner = serde_json::from_str(&json)?;
            if owner.planned_claim.take().is_some() {
                tx.execute(
                    "UPDATE thread_owners SET state=?2 WHERE thread_id=?1",
                    params![thread, serde_json::to_string(&owner)?],
                )?;
            }
            receipt.cleaned_threads.push(thread.clone());
        } else {
            receipt.shared_threads.push(thread.clone());
        }
        tx.execute("INSERT INTO disposal_cleanup_receipt_members(operation_id,thread_id,restart_id,capture_id,owner_id,owner_generation,cleaned) VALUES(?1,?2,?3,?4,?5,?6,?7)",params![receipt.effect.operation_id,thread,receipt.restart_id,receipt.capture_id,permit.lease.owner_id,permit.lease.generation,cleaned])?;
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
