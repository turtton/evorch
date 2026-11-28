//! What the pull request service needs from the rest of the harness: where a thread's
//! checkouts are, and which thread a run belongs to.

use std::path::PathBuf;
use std::time::Duration;

use super::checkout::{CheckoutResolution, resolve_blocking};

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
        resolve_blocking(
            directories,
            head_repository,
            head_branch,
            budget,
            self.spawn_ledger(),
        )
    }

    /// The core's ledger, when this environment is the core's: the `git` of
    /// [`resolve_checkout`](Self::resolve_checkout) is registered in it and
    /// does not start once its spawn fence is closed.
    fn spawn_ledger(&self) -> Option<sandbox::SpawnLedger> {
        None
    }
}
