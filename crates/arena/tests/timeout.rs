use arena::{ArenaConfig, ArenaSpec, Attribution, FailureAttribution, Runner, TaskSpec};
use providers::{
    ChatRequest, ChatResponse, DeltaStream, ProviderAuth, ProviderClient, ProviderError,
};

struct PendingClient;

#[async_trait::async_trait]
impl ProviderClient for PendingClient {
    fn capabilities(&self) -> providers::ProviderCapabilities {
        providers::ProviderCapabilities {
            streaming: true,
            tool_use: false,
            reasoning: false,
        }
    }
    async fn send(&self, _: &ProviderAuth, _: &ChatRequest) -> Result<ChatResponse, ProviderError> {
        std::future::pending().await
    }
    async fn stream(
        &self,
        _: &ProviderAuth,
        _: &ChatRequest,
    ) -> Result<DeltaStream, ProviderError> {
        std::future::pending().await
    }
}

#[tokio::test(start_paused = true)]
async fn deadline_cancels_pending_provider_and_records_every_config() {
    // Given: a provider that never answers and a virtual-clock arena deadline.
    let dir = tempfile::tempdir().expect("tempdir");
    let storage = storage::Storage::open(storage::StorageConfig {
        db_path: dir.path().join("timeout.db"),
        ..Default::default()
    })
    .expect("storage");
    let spec = ArenaSpec {
        id: "timeout".into(),
        project: "p".into(),
        task: TaskSpec {
            id: "t".into(),
            prompt: "task".into(),
            expected_output: "ok".into(),
        },
        configs: ["a", "b"]
            .into_iter()
            .map(|id| ArenaConfig {
                id: id.into(),
                profile: "local".into(),
                model: id.into(),
                attribution: Attribution::Worker,
                variant: Default::default(),
            })
            .collect(),
        timeout_ms: 10,
        max_output_tokens: 10,
        total_token_budget: 1000,
    };
    let runner = Runner {
        client: &PendingClient,
        auth: &ProviderAuth::new("test"),
        storage: storage.handle(),
    };
    // When: virtual time advances to the deadline (no sleeps).
    let report = arena::run(&spec, &runner).await.expect("run");
    // Then: both attempts are terminal failures and cannot be promoted.
    assert_eq!(report.traces().len(), 2);
    assert!(
        report
            .traces()
            .iter()
            .all(|t| t.failure == Some(FailureAttribution::Timeout))
    );
    assert!(report.selected().is_empty());
    assert!(report.traces().iter().all(|trace| trace.elapsed_ms >= 5));
    assert!(report.traces().iter().all(|trace| {
        trace.execution.as_ref().is_some_and(|execution| {
            execution.steps.len() == 1 && execution.steps[0].model == trace.model
        })
    }));
}
