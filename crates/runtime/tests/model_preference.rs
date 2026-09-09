use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use event_bus::EventBus;
use providers::{ChatResponse, FinishReason, Message, ToolSpec, Usage};
use runtime::{
    AgentInvocationContext, AgentModel, AgentRuntime, ModelPreference, Role, RunConfig, RunId,
    RuntimeError,
};
use tokio::sync::{Semaphore, mpsc};
use tools::ToolExecutor;

struct RecordingModel {
    records: mpsc::Sender<Option<ModelPreference>>,
    proceed: Semaphore,
}

#[async_trait]
impl AgentModel for RecordingModel {
    async fn complete(
        &self,
        invocation: &AgentInvocationContext,
        _role: Role,
        _messages: &[Message],
        _tools: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        self.records
            .send(invocation.model_preference.clone())
            .await
            .unwrap();
        self.proceed.acquire().await.unwrap().forget();
        Ok(ChatResponse {
            message: Message {
                role: providers::Role::Assistant,
                content: vec![],
            },
            usage: Usage::default(),
            finish_reason: FinishReason::Stop,
        })
    }

    fn selected_model(&self, _role: Role) -> String {
        "recording".into()
    }
}

#[tokio::test]
async fn set_model_preference_applies_to_next_completion() {
    // Given: a keep-alive run with a blocked first completion and an initial preference.
    let bus = Arc::new(EventBus::new(64));
    let (tx, mut rx) = mpsc::channel(4);
    let model = Arc::new(RecordingModel {
        records: tx,
        proceed: Semaphore::new(0),
    });
    let runtime = AgentRuntime::new(bus.clone(), Arc::new(ToolExecutor::new(bus)), model.clone());
    let initial = ModelPreference {
        profile: "initial".into(),
        model: None,
    };
    let run = runtime.delegate_background(
        Role::Worker,
        "start".into(),
        RunConfig {
            interactive: true,
            keep_alive: true,
            model_preference: Some(initial.clone()),
            ..RunConfig::default()
        },
    );
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .unwrap(),
        Some(Some(initial))
    );
    // When: change the preference while a completion is in flight, then clear it.
    let preference = ModelPreference {
        profile: "profile-b".into(),
        model: Some("model-b".into()),
    };
    for next in [Some(preference), None] {
        runtime.set_model_preference(run, next.clone()).unwrap();
        runtime.send_message(run, "continue".into()).unwrap();
        model.proceed.add_permits(1);
        // Then: the next invocation receives the new snapshot.
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .unwrap(),
            Some(next)
        );
    }
    runtime.cancel(run).unwrap();
    tokio::time::timeout(Duration::from_secs(5), runtime.wait(run))
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn set_model_preference_rejects_unknown_run() {
    // Given
    let bus = Arc::new(EventBus::new(16));
    let runtime = AgentRuntime::new(
        bus.clone(),
        Arc::new(ToolExecutor::new(bus)),
        Arc::new(runtime::compose::UnconfiguredModel),
    );
    // When
    let result = runtime.set_model_preference(RunId::new(999), None);
    // Then
    assert!(matches!(result, Err(RuntimeError::UnknownRun { .. })));
}
