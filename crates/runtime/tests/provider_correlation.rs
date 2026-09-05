//! agent-loop → モデル → provider wire を貫く run_id 相関 threading の契約。
//!
//! ローカル mock OpenAI 互換サーバと実 `OpenAiCompatibleClient` を使い、
//! wire 経路を通した状態で attempt 観測イベントと ToolEvent の run_id stamp
//! を検証する。

mod support;

use std::sync::Arc;
use std::time::Duration;

use agents::Role;
use async_trait::async_trait;
use event_bus::{AgentRunPhase, Event, EventBus, EventKind, ProviderEvent, ToolEvent};
use providers::{
    ChatRequest, ChatResponse, Message, ObservationContext, ProviderAuth, ProviderClient, ToolSpec,
    provider::openai_compatible::OpenAiCompatibleClient,
};
use runtime::{AgentInvocationContext, AgentModel, AgentRuntime, RunConfig, RuntimeError};
use sandbox::DirectSandbox;
use serde_json::json;
use tools::ToolExecutor;

use support::mock_openai::RecordingMockOpenAi;

fn runtime_with(bus: &Arc<EventBus>, model: Arc<dyn AgentModel>) -> AgentRuntime {
    let executor = Arc::new(ToolExecutor::with_standard_tools(
        Arc::clone(bus),
        Arc::new(DirectSandbox::new_unchecked()),
    ));
    AgentRuntime::new(Arc::clone(bus), executor, model)
}

async fn drain_events(receiver: &mut event_bus::EventReceiver) -> Vec<Event> {
    let mut events = Vec::new();
    while let Ok(event) = tokio::time::timeout(Duration::from_millis(200), receiver.recv()).await {
        match event {
            Ok(event) => events.push(event),
            Err(_) => break,
        }
    }
    events
}

/// production 実装と同じ「invocation → ChatRequest.observation」変換を行い、
/// 実 wire で attempt 観測への run_id stamp を検証できるようにするテスト用モデル。
struct ProviderCorrelatedModel {
    client: OpenAiCompatibleClient,
    auth: ProviderAuth,
}

impl ProviderCorrelatedModel {
    fn new(base_url: String, bus: Arc<EventBus>) -> Self {
        Self {
            client: OpenAiCompatibleClient::new(
                base_url,
                "mock-provider",
                Duration::from_secs(5),
                Some(bus),
            )
            .expect("mock provider クライアントを構築できる"),
            auth: ProviderAuth::new("test-key"),
        }
    }
}

#[async_trait]
impl AgentModel for ProviderCorrelatedModel {
    async fn complete(
        &self,
        invocation: &AgentInvocationContext,
        _role: Role,
        messages: &[Message],
        tools: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        let request = ChatRequest {
            model: "mock-model".to_string(),
            messages: messages.to_vec(),
            tools: tools.to_vec(),
            temperature: None,
            max_tokens: None,
            observation: Some(ObservationContext {
                run_id: invocation.run_id.clone(),
            }),
        };
        self.client
            .send(&self.auth, &request)
            .await
            .map_err(|error| RuntimeError::Model {
                reason: error.to_string(),
            })
    }

    fn selected_model(&self, _role: Role) -> String {
        "mock-observation".to_string()
    }
}

// Given: tool_call → stop の 2 応答を返す mock OpenAI 互換サーバと observation を wire に載せるモデル
// When: agent-loop を Worker run として完了まで実行する
// Then: bus 観測される全 ProviderEvent attempt (RequestStarted/RequestCompleted) と
//       ToolEvent (ToolStarted/ToolCompleted) の run_id はその run の ID
#[tokio::test]
async fn agent_loop_stamps_run_id_on_provider_attempts_and_tool_events() {
    let directory = tempfile::tempdir().expect("temp directory");
    let path = directory.path().join("out.txt");
    let tool_response = json!({
        "choices": [{
            "message": {
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "call-1",
                    "type": "function",
                    "function": {
                        "name": "edit",
                        "arguments": json!({ "path": path, "new_string": "written" }).to_string()
                    }
                }]
            },
            "finish_reason": "tool_calls"
        }],
        "usage": { "prompt_tokens": 1, "completion_tokens": 1 }
    })
    .to_string();
    let stop_response = json!({
        "choices": [{
            "message": { "role": "assistant", "content": "done" },
            "finish_reason": "stop"
        }],
        "usage": { "prompt_tokens": 1, "completion_tokens": 1 }
    })
    .to_string();

    let bus = Arc::new(EventBus::new(64));
    let mock_openai = RecordingMockOpenAi::spawn(vec![tool_response, stop_response]);
    let base_url = mock_openai.base_url();
    let runtime = runtime_with(
        &bus,
        Arc::new(ProviderCorrelatedModel::new(base_url, Arc::clone(&bus))),
    );
    let mut events = bus.subscribe();

    let run_id =
        runtime.delegate_background(Role::Worker, "write".to_string(), RunConfig::default());
    assert_eq!(runtime.wait(run_id).await, Ok(AgentRunPhase::Done));

    let events = drain_events(&mut events).await;
    let expected_run_id = run_id.to_string();

    let attempt_run_ids = events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::Provider(
                ProviderEvent::RequestStarted { run_id, .. }
                | ProviderEvent::RequestCompleted { run_id, .. },
            ) => Some(run_id),
            _ => None,
        })
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(
        attempt_run_ids.len(),
        4,
        "tool_call と stop の 2 attempt 分 (started + completed) が観測される: {attempt_run_ids:?}"
    );
    for run_id in &attempt_run_ids {
        assert_eq!(
            run_id,
            &Some(expected_run_id.clone()),
            "attempt 観測イベントに run 相関が載る"
        );
    }

    let tool_run_ids = events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::Tool(
                ToolEvent::ToolStarted { run_id, .. } | ToolEvent::ToolCompleted { run_id, .. },
            ) => Some(run_id),
            _ => None,
        })
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(
        tool_run_ids.len(),
        2,
        "edit ツールの started + completed が観測される: {tool_run_ids:?}"
    );
    for run_id in &tool_run_ids {
        assert_eq!(
            run_id,
            &Some(expected_run_id.clone()),
            "ToolEvent に run 相関が載る"
        );
    }
}
