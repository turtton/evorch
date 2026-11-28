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
pub mod gate;
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

#[derive(Debug, thiserror::Error)]
pub enum RetentionError {
    #[error(transparent)]
    Storage(#[from] storage::StorageError),
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

/// The status shown for every thread while the binding does not match.
pub const BINDING_MISMATCH_BLOCK: &str = "retention disabled: binding mismatch";

pub struct ThreadRetention;

impl ThreadRetention {
    /// The status of every sidebar thread for display: `DueIn` / `DueNextStartup`
    /// become "deleted in N days" / "deleted at next startup", `Blocked` carries the
    /// reason. With a mismatching binding every archived thread reads as blocked by it.
    ///
    /// # Errors
    /// Fails when the database cannot be read.
    pub fn schedule(
        database: &Database,
        input: &RetentionInput,
        policy: &ThreadRetentionPolicy,
        binding: &BindingStatus,
    ) -> Result<Vec<(String, RetentionStatus)>, RetentionError> {
        let overrides = BTreeMap::new();
        let mut statuses = database.retention_schedule(input, policy, &overrides)?;
        if matches!(binding, BindingStatus::Mismatch(_)) {
            for (_, status) in &mut statuses {
                if *status != RetentionStatus::NotArchived {
                    *status = RetentionStatus::Blocked(BlockReason::SafetyHold(
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
