//! The deletion of expired archived threads, run by the core (PR 09).
//!
//! The core is the one process that confirms a deletion: at start it takes the
//! database and ownership-root leases exclusively, runs the retention phase over
//! the sidebar it owns and then keeps both leases shared for its whole life. A
//! client (the GUI in client mode, the web adapter) never runs the phase: it has
//! no lease and no sidebar of its own, so a client that is closed, or none at
//! all, changes nothing about what is deleted. When another process holds a lease
//! (a legacy GUI that runs its own phase), the core takes the shared leases only
//! and deletes nothing, as before.
//!
//! What a family may lose is decided in `storage`, not here: pinned, evaluation
//! sample and running families, a family with a member of a committed and not
//! superseded recovery manifest and one with a prepared recovery claim are never
//! confirmed (the closure of 06b). A sidebar the core cannot read, or one whose
//! families are not resolved, confirms nothing.

use std::path::PathBuf;
use std::time::Duration;

use runtime::thread_retention::{
    RetentionError, StartupBudget, StartupPaths, StartupReport, run_exclusive_phase, startup_input,
};
use storage::thread_retention::ThreadRetentionPolicy;
use storage::{ExclusiveLeases, SharedLeases};
use workspace_state::SidebarStore;

use super::BootstrapError;
use crate::areas::workspace::sidebar_threads;

/// What the phase works on.
#[derive(Debug, Clone)]
pub struct RetentionPlan {
    pub db_path: PathBuf,
    pub ownership_root: PathBuf,
    /// The sidebar file the core owns. Without it nothing says what is
    /// archived, and nothing is deleted.
    pub sidebar_path: PathBuf,
    pub artifacts_dir: Option<PathBuf>,
    pub policy: ThreadRetentionPolicy,
}

fn now_nanos() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_nanos()).ok())
        .unwrap_or(0)
}

/// Runs the phase over the saved sidebar while `leases` proves the exclusivity.
///
/// # Errors
/// The sidebar cannot be read, or the phase fails before it confirmed anything
/// (see [`run_exclusive_phase`]).
pub fn run_phase(
    plan: &RetentionPlan,
    leases: &ExclusiveLeases,
) -> Result<StartupReport, PhaseError> {
    // No other evorch process runs while the leases are exclusive: the file needs
    // no advisory lock to be read.
    let document = SidebarStore::new(&plan.sidebar_path)
        .read_unlocked()
        .map_err(|error| PhaseError::Sidebar(error.to_string()))?;
    let paths = StartupPaths {
        db_path: plan.db_path.clone(),
        ownership_root: plan.ownership_root.clone(),
        sidebar_path: plan.sidebar_path.clone(),
        artifacts_dir: plan.artifacts_dir.clone(),
    };
    let now = now_nanos();
    let input = startup_input(&paths, sidebar_threads(&document.sidebar), now)?;
    Ok(run_exclusive_phase(
        leases,
        &paths,
        &plan.policy,
        &input,
        &StartupBudget::default(),
        now,
    )?)
}

#[derive(Debug, thiserror::Error)]
pub enum PhaseError {
    #[error("the sidebar cannot be read: {0}")]
    Sidebar(String),
    #[error(transparent)]
    Retention(#[from] RetentionError),
}

/// Takes the leases the core runs with: exclusively first, to run the phase,
/// then shared for the life of the process. A phase that fails is logged and
/// the core still starts (nothing it did not confirm is deleted).
///
/// # Errors
/// A lease cannot be taken or stays exclusive in another process for `wait`.
pub fn acquire_leases(
    plan: &RetentionPlan,
    wait: Duration,
) -> Result<SharedLeases, BootstrapError> {
    let exclusive = if plan.sidebar_path.exists() {
        storage::try_exclusive(&plan.db_path, &plan.ownership_root)?
    } else {
        None
    };
    let Some(exclusive) = exclusive else {
        return Ok(storage::shared_with_wait(
            &plan.db_path,
            &plan.ownership_root,
            wait,
        )?);
    };
    match run_phase(plan, &exclusive) {
        Ok(report) => tracing::info!(
            target: "thread_retention",
            elapsed_ms = u64::try_from(report.elapsed.as_millis()).unwrap_or(u64::MAX),
            "core retention phase finished"
        ),
        Err(error) => {
            tracing::warn!(target: "thread_retention", %error, "core retention phase failed");
        }
    }
    Ok(exclusive.downgrade(wait)?)
}
