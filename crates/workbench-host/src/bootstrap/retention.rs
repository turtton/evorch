//! The deletion of expired archived threads, run by the core (PR 09).
//!
//! The core is the one process that confirms a deletion: at start it takes the
//! database and ownership-root leases exclusively, runs the retention phase over
//! the sidebar it owns and then keeps both leases shared for its whole life. A
//! client (the GUI in client mode, the web adapter) never runs the phase: it has
//! no lease and no sidebar of its own, so a client that is closed, or none at
//! all, changes nothing about what is deleted. When another process holds a lease
//! (another core sharing the root), the core takes the shared leases only
//! and deletes nothing, as before.
//!
//! What a family may lose is decided in `storage`, not here: pinned, evaluation
//! sample and running families and a family with a prepared recovery claim are
//! never confirmed. A component that a committed, not superseded recovery
//! manifest depends on is never split: when the whole component can be deleted
//! it is deleted whole, otherwise none of it is (the closure of 06b). A sidebar
//! the core cannot read, or one whose families are not resolved, confirms
//! nothing.

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
    // Readiness never explores or confirms new deletion candidates. Only the
    // activation-time online engine may do that; the exclusive startup path
    // retains oversized rows, artifact GC and scratch cleanup.
    let cleanup_only = ThreadRetentionPolicy {
        archived_delete_after_days: 0,
        ..plan.policy
    };
    Ok(run_exclusive_phase(
        leases,
        &paths,
        &cleanup_only,
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
    scratch: &runtime::scratch::ThreadScratch,
) -> Result<SharedLeases, BootstrapError> {
    let started = std::time::Instant::now();
    // Sidebar absence does not prove another core is live. Root-wide scratch
    // cleanup uses the DB/root exclusive leases, independently of retention.
    let exclusive = storage::try_exclusive(&plan.db_path, &plan.ownership_root)?;
    let Some(exclusive) = exclusive else {
        return Ok(storage::shared_with_wait(
            &plan.db_path,
            &plan.ownership_root,
            wait.saturating_sub(started.elapsed()),
        )?);
    };
    sweep_scratch(plan, scratch);
    if plan.sidebar_path.exists() {
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
    }
    Ok(exclusive.downgrade(wait.saturating_sub(started.elapsed()))?)
}

/// Called only while the caller retains both exclusive maintenance leases.
fn sweep_scratch(plan: &RetentionPlan, scratch: &runtime::scratch::ThreadScratch) {
    let protected = (|| -> Result<_, BootstrapError> {
        let config = storage::StorageConfig {
            db_path: plan.db_path.clone(),
            ..Default::default()
        };
        let db = storage::Database::open(&config)?;
        if let runtime::thread_retention::BindingStatus::Mismatch(reason) =
            runtime::thread_retention::check_binding(
                &db,
                runtime::thread_retention::BindingPaths {
                    ownership_root: &plan.ownership_root,
                    sidebar_path: &plan.sidebar_path,
                },
            )?
        {
            tracing::warn!(%reason, "scratch sweep skipped: root binding mismatch");
            return Ok(None);
        }
        let mut threads = std::collections::HashSet::new();
        for graph in db.recovery_graph_headers()? {
            if graph.superseded {
                continue;
            }
            // A graph with no provable thread correspondence may still own
            // unbound run scratch. Preserve the entire root on uncertainty.
            if graph.bindings.is_empty() {
                return Ok(None);
            }
            threads.extend(graph.bindings.into_iter().map(|(thread, _)| thread));
        }
        Ok(Some(threads))
    })();
    match protected {
        Ok(Some(threads)) => scratch.sweep(&threads, false),
        Ok(None) => tracing::info!("scratch sweep skipped: unresolved recovery membership"),
        Err(error) => {
            tracing::warn!(%error, "scratch sweep skipped: recovery inventory unavailable")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};
    use std::os::unix::ffi::OsStrExt;

    fn fixture() -> (
        tempfile::TempDir,
        RetentionPlan,
        runtime::scratch::ThreadScratch,
        PathBuf,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("threads");
        std::fs::create_dir(&root).unwrap();
        let plan = RetentionPlan {
            db_path: dir.path().join("events.db"),
            ownership_root: root.clone(),
            sidebar_path: dir.path().join("sidebar.json"),
            artifacts_dir: None,
            policy: ThreadRetentionPolicy::default(),
        };
        let scratch = runtime::scratch::ThreadScratch::for_ownership_root(&root).unwrap();
        let thread = scratch.thread_dir("cold-thread");
        let canonical = root.canonicalize().unwrap();
        let digest = format!("{:x}", Sha256::digest(canonical.as_os_str().as_bytes()));
        let prepared =
            sandbox::scratch::ScratchRoot::fixed_for_owner(&digest[..8], &digest, &canonical)
                .unwrap()
                .prepare(thread.file_name().unwrap().to_str().unwrap())
                .unwrap();
        assert_eq!(prepared, thread);
        let marker = thread.join("sentinel");
        std::fs::write(&marker, b"existing scratch").unwrap();
        (dir, plan, scratch, marker)
    }

    #[test]
    fn cold_start_without_sidebar_sweeps_only_inside_exclusive_leases() {
        let (_dir, plan, scratch, marker) = fixture();
        assert!(!plan.sidebar_path.exists());
        let leases = acquire_leases(&plan, Duration::ZERO, &scratch).unwrap();
        assert!(!marker.exists());
        assert!(
            storage::try_exclusive(&plan.db_path, &plan.ownership_root)
                .unwrap()
                .is_none()
        );
        assert!(!plan.ownership_root.join("owners.db").exists());
        drop(leases);
    }

    #[test]
    fn another_config_sharing_root_prevents_root_wide_sweep() {
        let (_dir, plan, scratch, marker) = fixture();
        let other_db = plan.db_path.with_file_name("other.db");
        let other =
            storage::shared_with_wait(&other_db, &plan.ownership_root, Duration::ZERO).unwrap();
        let leases = acquire_leases(&plan, Duration::ZERO, &scratch).unwrap();
        assert_eq!(std::fs::read(marker).unwrap(), b"existing scratch");
        drop((leases, other));
    }

    #[test]
    fn binding_mismatch_retains_scratch_even_when_no_core_is_running() {
        let (_dir, plan, scratch, marker) = fixture();
        std::fs::write(
            plan.ownership_root
                .join(runtime::thread_retention::binding::ROOT_BINDING_FILE),
            b"invalid old binding",
        )
        .unwrap();
        let leases = acquire_leases(&plan, Duration::ZERO, &scratch).unwrap();
        assert_eq!(std::fs::read(marker).unwrap(), b"existing scratch");
        drop(leases);
    }
}
