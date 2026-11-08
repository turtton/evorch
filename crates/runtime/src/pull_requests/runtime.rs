//! Connecting the pull request service to a runtime: what it knows about threads, and where the
//! rest of the runtime reaches it.

use std::path::PathBuf;
use std::sync::Arc;

use config::PullRequestsConfig;

use super::{
    DerivedThreadKind, OwnershipView, PrNoticeDelivery, PullRequestService, PullRequestSource,
    ServiceParts, SystemWallClock, ThreadEnvironment,
};
use crate::{AgentRuntime, WeakAgentRuntime};

/// Reads thread facts from the runtime without keeping it alive.
struct RuntimeEnvironment {
    runtime: WeakAgentRuntime,
}

impl PrNoticeDelivery for RuntimeEnvironment {
    fn schedule(&self, thread: &str) {
        if let Some(runtime) = self.runtime.upgrade() {
            runtime.schedule_lane_deliveries(thread);
        }
    }
    fn subscribe_resumes(&self) -> tokio::sync::watch::Receiver<u64> {
        self.runtime.upgrade().map_or_else(
            || tokio::sync::watch::channel(0).1,
            |runtime| runtime.subscribe_harness_delivery_resumes(),
        )
    }
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
    pub(crate) fn lane_checkout_directories(&self, thread: &str) -> Vec<PathBuf> {
        RuntimeEnvironment {
            runtime: self.downgrade(),
        }
        .checkout_directories(thread)
    }
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
        self.start_pull_requests_with_log_source(
            ownership,
            source,
            settings,
            Arc::new(super::GhLogSource::new()),
        )
    }

    /// [`Self::start_pull_requests`] with the source of the CI failure logs a notice carries
    /// (the `gh` CLI in production; a script in tests that must not reach a network).
    ///
    /// # Errors
    /// As [`Self::start_pull_requests`].
    pub fn start_pull_requests_with_log_source(
        &self,
        ownership: Arc<dyn OwnershipView>,
        source: Arc<dyn PullRequestSource>,
        settings: PullRequestsConfig,
        logs: Arc<dyn super::PullRequestLogSource>,
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
        let service = PullRequestService::start_with_notice_delivery(
            ServiceParts {
                storage,
                source,
                ownership,
                environment: Arc::new(RuntimeEnvironment {
                    runtime: self.downgrade(),
                }),
                bus: Arc::clone(&self.shared.bus),
                settings,
                clock: Arc::new(SystemWallClock),
            },
            logs,
            Arc::new(RuntimeEnvironment {
                runtime: self.downgrade(),
            }),
        )
        .map_err(|error| error.to_string())?;
        Ok(self.shared.pull_requests.get_or_init(|| service).clone())
    }

    /// The pull request service, once [`Self::start_pull_requests`] ran.
    #[must_use]
    pub fn pull_requests(&self) -> Option<PullRequestService> {
        self.shared.pull_requests.get().cloned()
    }

    /// A derived thread may take its parent's pull request link (escalation only here; a fork
    /// or rewind is derived by the host, which calls [`PullRequestService::inherit`] with the
    /// same arguments). The parent's link is pinned here, in the path that creates the child.
    pub(crate) fn inherit_pull_request(&self, parent_thread: Option<String>, child_thread: &str) {
        if let (Some(service), Some(parent)) = (self.shared.pull_requests.get(), parent_thread)
            && let Err(error) =
                service.inherit_now(&parent, child_thread, DerivedThreadKind::Escalation)
        {
            tracing::warn!(%error, parent, child = child_thread, "pull request inheritance was not pinned");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use async_trait::async_trait;
    use config::PullRequestsConfig;
    use event_bus::EventBus;
    use providers::{ChatResponse, FinishReason, Message, Role as MessageRole, ToolSpec, Usage};
    use storage::pull_requests::{InheritanceState, PullRequestKey};

    use crate::ownership::OwnerHost;
    use crate::pull_requests::FakePullRequestSource;
    use crate::{AgentInvocationContext, AgentModel, AgentRuntime, Role, RunStore, RuntimeError};

    struct NoModel;

    #[async_trait]
    impl AgentModel for NoModel {
        async fn complete(
            &self,
            _: &AgentInvocationContext,
            _: Role,
            _: &[Message],
            _: &[ToolSpec],
        ) -> Result<ChatResponse, RuntimeError> {
            Ok(ChatResponse {
                usage_reported: true,
                message: Message {
                    role: MessageRole::Assistant,
                    content: Vec::new(),
                },
                finish_reason: FinishReason::Stop,
                usage: Usage::default(),
            })
        }

        fn selected_model(&self, _: Role, _: Option<&str>) -> String {
            "pull-request-hook".into()
        }
    }

    // Given: a parent linked to a pull request / When: an escalation creates a child and the
    // parent unlinks right after / Then: the child's inheritance was pinned to the parent's link
    // inside the creation path, so the unlink does not take it away
    #[tokio::test(start_paused = true)]
    async fn an_escalation_pins_the_parent_link_before_the_parent_can_change_it() {
        let directory = tempfile::tempdir().unwrap();
        let config = storage::StorageConfig {
            db_path: directory.path().join("hook.db"),
            ..Default::default()
        };
        let storage = storage::Storage::open(config.clone()).unwrap();
        let bus = Arc::new(EventBus::new(256));
        let runtime = AgentRuntime::new(
            Arc::clone(&bus),
            Arc::new(tools::ToolExecutor::new(bus)),
            Arc::new(NoModel),
        )
        .with_run_store(RunStore::open(&config, storage.handle()).unwrap());
        let host = Arc::new(
            OwnerHost::open(
                &directory.path().join("owners"),
                config::OwnershipConfig::default(),
                Arc::clone(&runtime.shared.bus),
            )
            .unwrap(),
        );
        let service = runtime
            .start_pull_requests(
                Arc::clone(&host) as Arc<dyn crate::pull_requests::OwnershipView>,
                Arc::new(FakePullRequestSource::new()),
                PullRequestsConfig {
                    enabled: true,
                    ..PullRequestsConfig::default()
                },
            )
            .unwrap();
        let permit = host.start("parent").unwrap();
        let linked = service
            .link("parent", "turtton/evorch#1", 0, &permit)
            .await
            .unwrap();

        runtime.inherit_pull_request(Some("parent".into()), "escalation-run-9");
        service
            .unlink("parent", linked.lifecycle_revision, &permit)
            .await
            .unwrap();
        for _ in 0..50 {
            tokio::task::yield_now().await;
        }

        let row = storage
            .handle()
            .pull_request_inheritance("escalation-run-9")
            .unwrap()
            .expect("the inheritance was pinned in the creation path");
        assert_eq!(row.state, InheritanceState::Pending);
        assert_eq!(row.parent_link_id, linked.link.link_id);
        assert_eq!(
            row.key,
            PullRequestKey::github("turtton/evorch", 1).unwrap()
        );
    }
}
