//! Connecting the pull request service to a runtime: what it knows about threads, and where the
//! rest of the runtime reaches it.

use std::path::PathBuf;
use std::sync::Arc;

use config::PullRequestsConfig;

use super::{
    DerivedThreadKind, OwnershipView, PullRequestService, PullRequestSource, ServiceParts,
    SystemWallClock, ThreadEnvironment,
};
use crate::{AgentRuntime, WeakAgentRuntime};

/// Reads thread facts from the runtime without keeping it alive.
struct RuntimeEnvironment {
    runtime: WeakAgentRuntime,
}

impl ThreadEnvironment for RuntimeEnvironment {
    fn thread_of_run(&self, run_id: &str) -> Option<String> {
        let runtime = self.runtime.upgrade()?;
        if let Some(store) = runtime.shared.run_store.get() {
            match store.thread_of_run(run_id) {
                Ok(Some(binding)) => return Some(binding.thread_id),
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!(%error, run_id, "the thread of a run could not be read");
                }
            }
        }
        runtime.goal_thread(run_id.parse().ok()?)
    }

    /// The thread's project, the project's allowed directories and the thread's read-only
    /// directories. The project is the one the thread is bound to, never the one a window
    /// has selected.
    fn checkout_directories(&self, thread_id: &str) -> Vec<PathBuf> {
        let Some(runtime) = self.runtime.upgrade() else {
            return Vec::new();
        };
        let mut directories = Vec::new();
        let project_root = runtime
            .shared
            .run_store
            .get()
            .and_then(|store| match store.thread(thread_id) {
                Ok(row) => row.and_then(|row| row.project_root),
                Err(error) => {
                    tracing::warn!(%error, thread_id, "the project of a thread could not be read");
                    None
                }
            })
            .map(PathBuf::from);
        if let Some(root) = project_root {
            directories.extend(
                runtime
                    .shared
                    .project_allowed_directories(&root)
                    .get()
                    .iter()
                    .cloned(),
            );
            directories.insert(0, root);
        }
        directories.extend(runtime.thread_read_only_directories(thread_id));
        directories
    }
}

impl AgentRuntime {
    /// Starts pull request tracking for this runtime (ADR 0043): the scheduler, the output
    /// observer and the inheritance of derived threads. Call once, inside a Tokio runtime,
    /// after the run store is connected. A repeated call keeps the first service.
    ///
    /// `ownership` decides which threads this process may act for; `source` fetches the
    /// facts (the `gh` CLI in production). While `settings.enabled` is false nothing is
    /// fetched or observed, and a later [`PullRequestService::update_settings`] enables it.
    ///
    /// # Errors
    /// The settings are outside their documented ranges, or the runtime has no run store.
    pub fn start_pull_requests(
        &self,
        ownership: Arc<dyn OwnershipView>,
        source: Arc<dyn PullRequestSource>,
        settings: PullRequestsConfig,
    ) -> Result<PullRequestService, String> {
        if let Some(existing) = self.shared.pull_requests.get() {
            return Ok(existing.clone());
        }
        let storage = self
            .shared
            .run_store
            .get()
            .ok_or_else(|| "pull request tracking needs the run store".to_owned())?
            .handle
            .clone();
        let service = PullRequestService::start(ServiceParts {
            storage,
            source,
            ownership,
            environment: Arc::new(RuntimeEnvironment {
                runtime: self.downgrade(),
            }),
            bus: Arc::clone(&self.shared.bus),
            settings,
            clock: Arc::new(SystemWallClock),
        })
        .map_err(|error| error.to_string())?;
        Ok(self.shared.pull_requests.get_or_init(|| service).clone())
    }

    /// The pull request service, once [`Self::start_pull_requests`] ran.
    #[must_use]
    pub fn pull_requests(&self) -> Option<PullRequestService> {
        self.shared.pull_requests.get().cloned()
    }

    /// A derived thread may take its parent's pull request link (escalation only here; a fork
    /// or rewind is derived by the host, which calls
    /// [`PullRequestService::inherit`] with the same arguments).
    pub(crate) fn inherit_pull_request(&self, parent_thread: Option<String>, child_thread: String) {
        if let (Some(service), Some(parent)) = (self.shared.pull_requests.get(), parent_thread) {
            service.inherit_in_background(parent, child_thread, DerivedThreadKind::Escalation);
        }
    }
}
