//! Generations this process has seen lose authority (ADR 0024, addendum 2026-10-23).
//!
//! Run event fences read this record instead of opening a registry read
//! transaction for every event. A generation never regains authority: claims
//! and handoffs always increment it, and a release is final. So the record only
//! grows, and a contended registry can neither revoke a live generation nor
//! drop its events. Every registry access that observes a fence records it
//! here: permit operations, the heartbeat scan, releases and handoffs.
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, PoisonError, RwLock};

use super::Lease;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct Generation {
    registry: PathBuf,
    thread_id: String,
    owner_id: String,
    generation: u64,
}

impl Generation {
    fn new(registry: &Path, thread_id: &str, lease: &Lease) -> Self {
        Self {
            registry: registry.to_path_buf(),
            thread_id: thread_id.to_owned(),
            owner_id: lease.owner_id.clone(),
            generation: lease.generation,
        }
    }
}

#[derive(Default)]
struct Authority {
    fenced: HashSet<Generation>,
    approvals: HashMap<Generation, Vec<std::sync::Weak<sandbox::approval::ApprovalOwner>>>,
}
static FENCED: LazyLock<RwLock<Authority>> = LazyLock::new(RwLock::default);

/// Subscribe to observed ownership loss, including a fence that predates this
/// execution. Never invoke registry code while holding the authority record.
pub(crate) fn watch_approvals(
    permit: &super::OwnerPermit,
    owner: &std::sync::Arc<sandbox::approval::ApprovalOwner>,
) {
    let generation = Generation::new(&permit.registry_path, &permit.thread_id, &permit.lease);
    let fenced = {
        let mut authority = FENCED.write().unwrap_or_else(PoisonError::into_inner);
        if authority.fenced.contains(&generation) {
            true
        } else {
            let owners = authority.approvals.entry(generation).or_default();
            owners.retain(|o| o.strong_count() > 0);
            owners.push(std::sync::Arc::downgrade(owner));
            false
        }
    };
    if fenced {
        owner.close();
    }
}

/// Record that `lease` no longer holds `thread_id` in `registry`.
pub(crate) fn record_fenced(registry: &Path, thread_id: &str, lease: &Lease) {
    let generation = Generation::new(registry, thread_id, lease);
    let (inserted, owners) = {
        let mut authority = FENCED.write().unwrap_or_else(PoisonError::into_inner);
        let inserted = authority.fenced.insert(generation.clone());
        (
            inserted,
            authority.approvals.remove(&generation).unwrap_or_default(),
        )
    };
    for owner in owners.into_iter().filter_map(|o| o.upgrade()) {
        owner.close();
    }
    if inserted {
        tracing::info!(
            target: super::DB_TRACE_TARGET,
            thread_id,
            owner_id = %lease.owner_id,
            generation = lease.generation,
            "ownership generation fenced; its events are rejected from now on"
        );
    }
}

/// Whether this process has observed that `lease` lost `thread_id`.
pub(crate) fn is_fenced(registry: &Path, thread_id: &str, lease: &Lease) -> bool {
    FENCED
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .fenced
        .contains(&Generation::new(registry, thread_id, lease))
}

/// Record `permit`'s generation as fenced when `result` proves it lost authority.
pub(super) fn observe<T>(
    permit: &super::OwnerPermit,
    result: Result<T, super::RegistryError>,
) -> Result<T, super::RegistryError> {
    if let Err(error) = &result
        && error.is_fenced()
    {
        record_fenced(&permit.registry_path, &permit.thread_id, &permit.lease);
    }
    result
}
