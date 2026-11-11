//! This regression also runs unmodified against origin/main to demonstrate the
//! original missing save -> mailbox connection without waiting for a timeout.
use event_bus::{AgentMessageKind, EventBus};
use runtime::{AgentRuntime, Role, RunConfig, RunStore};
use std::sync::Arc;
struct Pending;
#[async_trait::async_trait]
impl runtime::AgentModel for Pending {
    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "pending".into()
    }
    async fn complete(
        &self,
        _: &runtime::AgentInvocationContext,
        _: Role,
        _: &[providers::Message],
        _: &[providers::ToolSpec],
    ) -> Result<providers::ChatResponse, runtime::RuntimeError> {
        std::future::pending().await
    }
}
#[tokio::test]
async fn saving_a_delegated_question_notifies_parent_without_user_input() {
    let dir = tempfile::tempdir().unwrap();
    let config = storage::StorageConfig {
        db_path: dir.path().join("regression.db"),
        ..Default::default()
    };
    let storage = storage::Storage::open(config.clone()).unwrap();
    let bus = Arc::new(EventBus::new(64));
    let runtime = AgentRuntime::new(
        bus.clone(),
        Arc::new(tools::ToolExecutor::new(bus)),
        Arc::new(Pending),
    )
    .with_run_store(RunStore::open(&config, storage.handle()).unwrap());
    let parent =
        runtime.delegate_background(Role::Orchestrator, "parent".into(), RunConfig::default());
    let child = runtime
        .delegate_background_as_child(parent, Role::Worker, "child", RunConfig::default())
        .unwrap();
    let question = runtime
        .request_user_question(child, "Which task scope?".into(), vec![], false)
        .unwrap();
    // The receipt is queued synchronously after the durable save; no scheduler
    // timing, sleep, or fixed completion deadline is part of this assertion.
    let messages = runtime.take_inbox(parent).unwrap();
    assert_eq!(
        messages.len(),
        1,
        "saving a delegated question must notify its parent"
    );
    assert_eq!(messages[0].kind, AgentMessageKind::Steering);
    assert!(messages[0].content.contains(&question.id));
    runtime.cancel_subtree(parent).unwrap();
}
