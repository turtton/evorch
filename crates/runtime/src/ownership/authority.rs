//! Generations this process has seen lose authority (ADR 0024, addendum 2026-10-23).
//!
//! Run event fences read this record instead of opening a registry read
//! transaction for every event. A generation never regains authority: claims
//! and handoffs always increment it, and a release is final. So the record only
//! grows, and a contended registry can neither revoke a live generation nor
//! drop its events. Every registry access that observes a fence records it
//! here: permit operations, the heartbeat scan, releases and handoffs.
use std::collections::HashSet;
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

static FENCED: LazyLock<RwLock<HashSet<Generation>>> = LazyLock::new(RwLock::default);

/// Record that `lease` no longer holds `thread_id` in `registry`.
pub(crate) fn record_fenced(registry: &Path, thread_id: &str, lease: &Lease) {
    let inserted = FENCED
        .write()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(Generation::new(registry, thread_id, lease));
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
