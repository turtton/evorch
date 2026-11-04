//! What the pull request service needs from the rest of the harness: who owns a thread
//! (ADR 0024) and where a thread's checkouts are.

use std::path::PathBuf;
use std::time::Duration;

use super::checkout::{CheckoutResolution, resolve_blocking};
use crate::ownership::{OwnerHost, OwnerPermit};

/// Who may act for a thread. Polling, refreshing and resolving an inheritance are done only
/// by the process that holds the thread's lease; a process that does not hold it acquires
/// nothing by itself.
pub trait OwnershipView: Send + Sync {
    /// The permit this process holds for `thread_id` right now (its lease is running and
    /// belongs to this process), or `None`. Blocks briefly on the ownership registry.
    fn owned_permit(&self, thread_id: &str) -> Option<OwnerPermit>;
}

impl OwnershipView for OwnerHost {
    fn owned_permit(&self, thread_id: &str) -> Option<OwnerPermit> {
        OwnerHost::owned_permit(self, thread_id).ok()
    }
}

/// Whether `permit`'s generation still holds `permit.thread_id`. Contention that outlasts the
/// retry budget is not an answer: it counts as lost, so nothing is done on a guess. Blocks.
pub(crate) fn permit_holds(permit: &OwnerPermit) -> bool {
    permit.validate_generation().is_ok()
}

/// Facts about threads the service reads from the runtime.
pub trait ThreadEnvironment: Send + Sync {
    /// The thread a run belongs to, if it is known. May read the database.
    fn thread_of_run(&self, run_id: &str) -> Option<String>;

    /// The directories a thread's work can be in: the project it is bound to, the allowed
    /// directories and the read-only directories declared for it. Never the project the
    /// window happens to have selected.
    fn checkout_directories(&self, thread_id: &str) -> Vec<PathBuf>;

    /// Finds the checkout of `head_branch` of `head_repository` among `directories`: the only
    /// place the service runs `git`. The default reads the repositories; a test replaces it to
    /// see whether it was asked.
    fn resolve_checkout(
        &self,
        directories: &[PathBuf],
        head_repository: Option<&str>,
        head_branch: &str,
        budget: Duration,
    ) -> CheckoutResolution {
        resolve_blocking(directories, head_repository, head_branch, budget)
    }
}
