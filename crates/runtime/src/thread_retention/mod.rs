//! Deletion of archived threads after their retention period (runtime side).
//!
//! `storage` decides and records (tombstones, closure search, reclaim); this module
//! supplies what only the runtime knows and drives the work:
//!
//! * [`startup`]: the exclusive phase at process start, the only place a deletion
//!   is confirmed (both the database lease and the ownership-root lease held
//!   exclusively prove that no other evorch process exists);
//! * [`binding`]: the two-way pairing of a database with its ownership root;
//! * [`executor`]: the background reclaim of confirmed families while running;
//! * [`guard`]: refusal of new work on deleted threads and runs;
//! * [`ThreadRetention::schedule`]: per-thread display status for the host.

pub mod binding;
pub mod executor;
pub mod guard;
pub mod startup;

use std::collections::BTreeMap;

pub use binding::{BindingPaths, BindingStatus, check_binding, write_root_binding};
pub use executor::{ExecutorConfig, ExecutorReport, ReclaimExecutor};
pub use guard::{ThreadDeleted, TombstoneGuard};
pub use startup::{
    DbMaintenance, ScratchReport, SkipReason, Stage, StartupBudget, StartupOutcome, StartupPaths,
    StartupProgress, StartupReport, run_exclusive_phase, run_exclusive_phase_with_progress,
    startup_input,
};
use storage::Database;
use storage::thread_retention::{
    BlockReason, LegacyArchivedPolicy, RetentionInput, RetentionStatus, ThreadRetentionPolicy,
};

use crate::ownership::{OwnerVerdict, Registry, RegistryError};

const MS_PER_DAY: u64 = 86_400_000;

#[derive(Debug, thiserror::Error)]
pub enum RetentionError {
    #[error(transparent)]
    Storage(#[from] storage::StorageError),
    #[error(transparent)]
    Registry(#[from] RegistryError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("retention binding: {0}")]
    Binding(String),
    #[error("retention input: {0}")]
    Input(String),
}

/// The storage policy for a configured retention period. A thread archived by a
/// version that recorded no time is due at once (user decision); its protections
/// still apply.
#[must_use]
pub fn policy_from_config(config: &config::StorageConfig) -> ThreadRetentionPolicy {
    ThreadRetentionPolicy {
        archived_delete_after_days: config.thread_retention.archived_delete_after_days,
        legacy: LegacyArchivedPolicy::Immediate,
    }
}

/// The reason an owner row keeps its thread from being deleted, per thread.
///
/// A row that still claims a turn or run blocks until its lease expired for the
/// whole retention period: a crashed owner needs no help, one that may still come
/// back is given the benefit of the doubt. Idle and released rows never block.
/// With `days == 0` nothing is ever confirmed, so the grace is moot.
#[must_use]
pub fn owner_blocks(
    verdicts: &BTreeMap<String, OwnerVerdict>,
    now_ms: u64,
    days: u32,
) -> BTreeMap<String, String> {
    let grace_ms = u64::from(days).saturating_mul(MS_PER_DAY);
    verdicts
        .iter()
        .filter_map(|(thread, verdict)| match verdict {
            OwnerVerdict::ActiveRemnant { expires_at_ms }
                if now_ms.saturating_sub(*expires_at_ms) < grace_ms =>
            {
                Some((
                    thread.clone(),
                    format!(
                        "owner active remnant until {}",
                        expires_at_ms.saturating_add(grace_ms)
                    ),
                ))
            }
            _ => None,
        })
        .collect()
}

/// The status shown for every thread while the binding does not match.
pub const BINDING_MISMATCH_BLOCK: &str = "retention disabled: binding mismatch";

pub struct ThreadRetention;

impl ThreadRetention {
    /// The status of every sidebar thread for display: `DueIn` / `DueNextStartup`
    /// become "deleted in N days" / "deleted at next startup", `Blocked` carries the
    /// reason. With a mismatching binding every archived thread reads as blocked by it.
    /// Owner rows (`registry`) are classified the way the startup phase will.
    ///
    /// # Errors
    /// Fails when the database or the registry cannot be read.
    pub fn schedule(
        database: &Database,
        input: &RetentionInput,
        policy: &ThreadRetentionPolicy,
        binding: &BindingStatus,
        registry: Option<&Registry>,
    ) -> Result<Vec<(String, RetentionStatus)>, RetentionError> {
        let mut overrides = BTreeMap::new();
        if let Some(registry) = registry {
            let archived = input
                .threads
                .iter()
                .filter(|thread| thread.archived)
                .map(|thread| thread.thread_id.clone())
                .collect::<Vec<_>>();
            let now_ms = u64::try_from(input.now_ns / 1_000_000)
                .map_err(|_| RetentionError::Input("now_ns is negative".into()))?;
            let verdicts = registry.classify_for_retention(&archived)?;
            for (thread, reason) in
                owner_blocks(&verdicts, now_ms, policy.archived_delete_after_days)
            {
                overrides.insert(thread, BlockReason::Owner(reason));
            }
        }
        let mut statuses = database.retention_schedule(input, policy, &overrides)?;
        if matches!(binding, BindingStatus::Mismatch(_)) {
            for (_, status) in &mut statuses {
                if *status != RetentionStatus::NotArchived {
                    *status = RetentionStatus::Blocked(BlockReason::Owner(
                        BINDING_MISMATCH_BLOCK.to_owned(),
                    ));
                }
            }
        }
        Ok(statuses)
    }
}

#[cfg(test)]
pub(crate) mod tests;
#[cfg(test)]
pub(crate) use tests::guard::tombstones as tombstones_for_tests;
