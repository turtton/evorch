//! `/btw` through the real command sink: the frozen context outlives a handoff, so the
//! sink must refuse it once another host owns the thread.
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use event_bus::{AgentRunPhase, EventBus};
use gui::model::commands::{BtwStartError, CommandSink};
use gui::runtime_sink::RuntimeCommandSink;
use providers::{
    ChatRequest, ChatResponse, ContentBlock, DeltaStream, FinishReason, Message, ProviderAuth,
    ProviderCapabilities, ProviderClient, ProviderError, Role as MessageRole, ToolSpec, Usage,
};
use runtime::btw::BtwError;
use runtime::ownership::OwnerHost;
use runtime::{AgentInvocationContext, AgentModel, AgentRuntime, Role, RunConfig, RuntimeError};
use tools::ToolExecutor;

fn text(text: &str) -> ChatResponse {
    ChatResponse {
        message: Message {
            role: MessageRole::Assistant,
            content: vec![ContentBlock::Text { text: text.into() }],
        },
        usage: Usage::default(),
        usage_reported: true,
        finish_reason: FinishReason::Stop,
    }
}

#[derive(Default)]
struct SideClient {
    sent: Mutex<usize>,
}

#[async_trait]
impl ProviderClient for SideClient {
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            streaming: true,
            tool_use: true,
            reasoning: false,
        }
    }

    async fn supports_tool_choice_none(&self, _request: &ChatRequest) -> bool {
        true
    }

    async fn send(
        &self,
        _auth: &ProviderAuth,
        _request: &ChatRequest,
    ) -> Result<ChatResponse, ProviderError> {
        *self.sent.lock().unwrap() += 1;
        Ok(text("side answer"))
    }

    async fn stream(
        &self,
        _auth: &ProviderAuth,
        _request: &ChatRequest,
    ) -> Result<DeltaStream, ProviderError> {
        Err(ProviderError::Request("not used".into()))
    }
}

struct PublishingModel {
    client: Arc<SideClient>,
    script: Mutex<VecDeque<ChatResponse>>,
}

#[async_trait]
impl AgentModel for PublishingModel {
    async fn complete(
        &self,
        invocation: &AgentInvocationContext,
        _role: Role,
        messages: &[Message],
        tools: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        if let Some(btw) = &invocation.btw {
            btw.publish(
                &ChatRequest {
                    responses_replay: None,
                    model: "stub".into(),
                    messages: messages.to_vec(),
                    tools: tools.to_vec(),
                    temperature: None,
                    max_tokens: None,
                    reasoning_effort: None,
                    service_tier: None,
                    output_schema: None,
                    tool_choice: None,
                    observation: None,
                },
                Arc::clone(&self.client) as Arc<dyn ProviderClient>,
                ProviderAuth::new("key"),
                None,
            );
        }
        self.script
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| RuntimeError::Model {
                reason: "script exhausted".into(),
            })
    }

    fn selected_model(&self, _role: Role, _category: Option<&str>) -> String {
        "stub".into()
    }
}

#[test]
fn a_handed_off_thread_refuses_side_questions_even_with_its_context_still_held() {
    let tokio = tokio::runtime::Runtime::new().unwrap();
    let client = Arc::new(SideClient::default());
    let bus = Arc::new(EventBus::new(64));
    let runtime = AgentRuntime::new(
        Arc::clone(&bus),
        Arc::new(ToolExecutor::new(Arc::clone(&bus))),
        Arc::new(PublishingModel {
            client: Arc::clone(&client),
            script: Mutex::new([text("main answer")].into()),
        }),
    );
    let directory = tempfile::tempdir().unwrap();
    let owner =
        Arc::new(OwnerHost::open(directory.path(), Default::default(), Arc::clone(&bus)).unwrap());
    owner.start("thread").unwrap();
    let mut sink = RuntimeCommandSink::new(runtime.clone(), tokio.handle().clone())
        .with_ownership(Arc::clone(&owner));

    let run = tokio.block_on(async {
        let run = runtime.delegate_background(Role::Worker, "task".into(), RunConfig::default());
        assert_eq!(runtime.wait(run).await, Ok(AgentRunPhase::Done));
        run
    });
    sink.bind_goal_context("thread", "project", &run.to_string());

    // The owner can ask about its finished run.
    let job = sink.start_btw("thread", "what happened?".into()).unwrap();
    let answer = job.receiver.recv().unwrap().unwrap();
    assert_eq!(answer.text, "side answer");
    assert!(answer.source.settled);
    assert_eq!(*client.sent.lock().unwrap(), 1);

    // A thread this process holds no run for has no context.
    assert_eq!(
        sink.start_btw("other", "q".into()).err(),
        Some(BtwStartError::Refused(BtwError::NoLocalContext))
    );

    // After the thread is handed to another host the context is still in memory, but
    // this process no longer owns the thread.
    let stale = owner.owned_permit("thread").unwrap();
    let successor =
        OwnerHost::open(directory.path(), Default::default(), Arc::clone(&bus)).unwrap();
    owner.handoff(&stale, &successor).unwrap();
    assert!(
        runtime.btw_snapshot(run).is_ok(),
        "the context is still held"
    );
    assert_eq!(
        sink.start_btw("thread", "q".into()).err(),
        Some(BtwStartError::Refused(BtwError::OwnedElsewhere))
    );
    assert_eq!(
        *client.sent.lock().unwrap(),
        1,
        "nothing was sent after the handoff"
    );
}
