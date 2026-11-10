//! `/btw` at the runtime boundary: a side question reads a frozen copy of the run's
//! last request, never starts or changes a run, and never executes a tool.
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use event_bus::{AgentRunPhase, EventBus, EventKind, RequestPurpose};
use providers::{
    ChatRequest, ChatResponse, ContentBlock, DeltaStream, FinishReason, Message, ProviderAuth,
    ProviderCapabilities, ProviderClient, ProviderError, Role as MessageRole, ToolChoice, ToolSpec,
};
use runtime::btw::BtwError;
use runtime::{
    AgentInvocationContext, AgentModel, AgentRuntime, Role, RunConfig, RunId, RuntimeError,
};
use sandbox::DirectSandbox;
use tokio::sync::Notify;
use tools::ToolExecutor;

mod support;
use support::{text_response, tool_response};

const SECRET: &str = "sk-btw-test-secret";
const QUESTION: &str = "zq-what did you just do?";

/// Answers every side request from a script and records what it was sent.
struct FakeClient {
    answers: Mutex<VecDeque<ChatResponse>>,
    sent: Mutex<Vec<ChatRequest>>,
    supports_none: bool,
    arrived: Notify,
    gate: Option<Notify>,
    dropped: AtomicBool,
}

impl FakeClient {
    fn new(answers: Vec<ChatResponse>) -> Arc<Self> {
        Arc::new(Self {
            answers: Mutex::new(answers.into()),
            sent: Mutex::new(Vec::new()),
            supports_none: true,
            arrived: Notify::new(),
            gate: None,
            dropped: AtomicBool::new(false),
        })
    }

    fn with(answers: Vec<ChatResponse>, supports_none: bool, gated: bool) -> Arc<Self> {
        Arc::new(Self {
            answers: Mutex::new(answers.into()),
            sent: Mutex::new(Vec::new()),
            supports_none,
            arrived: Notify::new(),
            gate: gated.then(Notify::new),
            dropped: AtomicBool::new(false),
        })
    }

    fn sent(&self) -> Vec<ChatRequest> {
        self.sent.lock().unwrap().clone()
    }
}

struct DropFlag<'a>(&'a AtomicBool);
impl Drop for DropFlag<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[async_trait]
impl ProviderClient for FakeClient {
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            streaming: true,
            tool_use: true,
            reasoning: false,
        }
    }

    async fn supports_tool_choice_none(&self, _request: &ChatRequest) -> bool {
        self.supports_none
    }

    async fn send(
        &self,
        _auth: &ProviderAuth,
        request: &ChatRequest,
    ) -> Result<ChatResponse, ProviderError> {
        let _flag = DropFlag(&self.dropped);
        self.sent.lock().unwrap().push(request.clone());
        self.arrived.notify_one();
        if let Some(gate) = &self.gate {
            gate.notified().await;
        }
        let answer = self.answers.lock().unwrap().pop_front();
        std::mem::forget(_flag);
        answer.ok_or_else(|| ProviderError::Request(format!("exhausted {SECRET}")))
    }

    async fn stream(
        &self,
        _auth: &ProviderAuth,
        _request: &ChatRequest,
    ) -> Result<DeltaStream, ProviderError> {
        Err(ProviderError::Request("side questions never stream".into()))
    }
}

/// A model that publishes the request each call would send, like the routed model.
struct PublishingModel {
    script: Mutex<VecDeque<ChatResponse>>,
    client: Arc<FakeClient>,
    publish: bool,
    window: Option<u64>,
    calls: AtomicUsize,
    turns: Mutex<Vec<AgentInvocationContext>>,
    /// Holds the call with this index after publishing it.
    hold: Option<(usize, Arc<Notify>, Arc<Notify>)>,
}

impl PublishingModel {
    fn new(script: Vec<ChatResponse>, client: &Arc<FakeClient>) -> Self {
        Self {
            script: Mutex::new(script.into()),
            client: Arc::clone(client),
            publish: true,
            window: None,
            calls: AtomicUsize::new(0),
            turns: Mutex::new(Vec::new()),
            hold: None,
        }
    }
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
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        self.turns.lock().unwrap().push(invocation.clone());
        if self.publish
            && let Some(btw) = &invocation.btw
        {
            btw.publish(
                &ChatRequest {
                    responses_replay: None,
                    model: "stub-model".into(),
                    messages: messages.to_vec(),
                    tools: tools.to_vec(),
                    temperature: None,
                    max_tokens: None,
                    reasoning_effort: None,
                    service_tier: None,
                    output_schema: None,
                    tool_choice: None,
                    observation: Some(providers::ObservationContext {
                        attempts: None,
                        turn: invocation.turn.clone(),
                        run_id: invocation.run_id.clone(),
                        purpose: invocation.purpose,
                    }),
                },
                Arc::clone(&self.client) as Arc<dyn ProviderClient>,
                ProviderAuth::new(SECRET),
                self.window,
            );
        }
        if let Some((index, arrived, release)) = &self.hold
            && *index == call
        {
            arrived.notify_one();
            release.notified().await;
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
        "stub-model".into()
    }
}

fn runtime_with(model: Arc<PublishingModel>) -> (AgentRuntime, Arc<EventBus>) {
    let bus = Arc::new(EventBus::new(256));
    let executor = Arc::new(ToolExecutor::with_standard_tools(
        Arc::clone(&bus),
        Arc::new(DirectSandbox::new_unchecked()),
    ));
    (AgentRuntime::new(Arc::clone(&bus), executor, model), bus)
}

fn tool_events(receiver: &mut event_bus::EventReceiver) -> usize {
    receiver
        .drain_pending_snapshot()
        .iter()
        .filter(|event| matches!(event.kind, EventKind::Tool(_)))
        .count()
}

#[tokio::test]
async fn a_run_this_process_never_held_has_no_context() {
    let client = FakeClient::new(vec![]);
    let (runtime, _bus) = runtime_with(Arc::new(PublishingModel::new(vec![], &client)));
    assert_eq!(
        runtime.btw_snapshot(RunId::new(4242)).unwrap_err(),
        BtwError::NoLocalContext
    );
}

#[tokio::test]
async fn before_the_first_request_nothing_is_askable_and_no_run_is_started() {
    let client = FakeClient::new(vec![text_response("never", FinishReason::Stop)]);
    let mut model = PublishingModel::new(vec![text_response("done", FinishReason::Stop)], &client);
    model.publish = false;
    let model = Arc::new(model);
    let (runtime, _bus) = runtime_with(Arc::clone(&model));
    let run = runtime.delegate_background(Role::Worker, "task".into(), RunConfig::default());
    assert_eq!(runtime.wait(run).await, Ok(AgentRunPhase::Done));
    assert_eq!(
        runtime.btw_snapshot(run).unwrap_err(),
        BtwError::ContextNotReady
    );
    assert_eq!(model.calls.load(Ordering::SeqCst), 1);
    assert!(client.sent().is_empty());
}

#[tokio::test]
async fn a_finished_run_answers_from_its_last_request_plus_final_answer() {
    let client = FakeClient::new(vec![text_response("side answer", FinishReason::Stop)]);
    let model = Arc::new(PublishingModel::new(
        vec![text_response("main final answer", FinishReason::Stop)],
        &client,
    ));
    let (runtime, bus) = runtime_with(Arc::clone(&model));
    let mut events = bus.subscribe();
    let run = runtime.delegate_background(Role::Worker, "task".into(), RunConfig::default());
    assert_eq!(runtime.wait(run).await, Ok(AgentRunPhase::Done));
    let _ = tool_events(&mut events);

    let snapshot = runtime.btw_snapshot(run).unwrap();
    assert!(snapshot.source().settled);
    let debug = format!("{snapshot:?}");
    assert!(!debug.contains(SECRET) && !debug.contains("main final answer"));
    let answer = snapshot.ask(QUESTION).await.unwrap();
    assert_eq!(answer.text, "side answer");
    assert!(!format!("{answer:?}").contains("side answer"));

    let sent = client.sent();
    assert_eq!(sent.len(), 1);
    let request = &sent[0];
    assert_eq!(request.tool_choice, Some(ToolChoice::None));
    assert!(
        !request.tools.is_empty(),
        "tool definitions stay for the cache"
    );
    let observation = request.observation.as_ref().unwrap();
    assert_eq!(observation.purpose, RequestPurpose::Btw);
    assert_eq!(observation.run_id, run.to_string());
    // The main turn's own context is the prefix; the final answer and question follow.
    let main = model.turns.lock().unwrap()[0].clone();
    assert_ne!(
        observation.turn.as_ref().unwrap().prompt_id,
        main.turn.unwrap().prompt_id
    );
    assert_eq!(observation.turn.as_ref().unwrap().turn_index, None);
    let [.., answer_message, question] = request.messages.as_slice() else {
        panic!("tail missing");
    };
    assert_eq!(answer_message.role, MessageRole::Assistant);
    assert!(matches!(&answer_message.content[0],
        ContentBlock::Text { text } if text == "main final answer"));
    assert!(matches!(&question.content[0],
        ContentBlock::Text { text } if text.contains(QUESTION)));

    // No run, model call or tool ran for it.
    assert_eq!(model.calls.load(Ordering::SeqCst), 1);
    assert_eq!(tool_events(&mut events), 0);
}

#[tokio::test]
async fn a_stopped_run_keeps_its_context() {
    let client = FakeClient::new(vec![text_response("side answer", FinishReason::Stop)]);
    let (arrived, release) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
    let mut model = PublishingModel::new(vec![], &client);
    model.hold = Some((0, Arc::clone(&arrived), release));
    let (runtime, _bus) = runtime_with(Arc::new(model));
    let run = runtime.delegate_background(Role::Worker, "task".into(), RunConfig::default());
    arrived.notified().await;
    runtime.stop(run, runtime::StopScope::SelfOnly).unwrap();
    assert_eq!(runtime.wait(run).await, Ok(AgentRunPhase::Stopped));
    let snapshot = runtime.btw_snapshot(run).unwrap();
    assert!(!snapshot.source().settled);
    assert_eq!(snapshot.ask(QUESTION).await.unwrap().text, "side answer");
}

#[tokio::test]
async fn mid_turn_snapshot_holds_only_complete_tool_rounds() {
    let client = FakeClient::new(vec![text_response("side answer", FinishReason::Stop)]);
    let (arrived, release) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
    let mut model = PublishingModel::new(
        vec![
            tool_response("call-1", "no_such_tool", serde_json::json!({})),
            text_response("final", FinishReason::Stop),
        ],
        &client,
    );
    model.hold = Some((1, Arc::clone(&arrived), Arc::clone(&release)));
    let (runtime, _bus) = runtime_with(Arc::new(model));
    let run = runtime.delegate_background(Role::Worker, "task".into(), RunConfig::default());
    // The second request is published once the first round's tool result is in.
    arrived.notified().await;
    let snapshot = runtime.btw_snapshot(run).unwrap();
    assert!(!snapshot.source().settled);
    snapshot.ask(QUESTION).await.unwrap();
    release.notify_one();
    assert_eq!(runtime.wait(run).await, Ok(AgentRunPhase::Done));

    let messages = &client.sent()[0].messages;
    let uses: Vec<_> = messages
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|block| match block {
            ContentBlock::ToolUse { id, .. } => Some(("use", id.as_str())),
            ContentBlock::ToolResult { tool_call_id, .. } => {
                Some(("result", tool_call_id.as_str()))
            }
            _ => None,
        })
        .collect();
    assert_eq!(uses, [("use", "call-1"), ("result", "call-1")]);
}

#[tokio::test]
async fn a_tool_call_in_the_reply_is_refused_and_nothing_runs() {
    let client = FakeClient::new(vec![tool_response(
        "side-call",
        "shell",
        serde_json::json!({"command": "echo hi"}),
    )]);
    let (runtime, bus) = runtime_with(Arc::new(PublishingModel::new(
        vec![text_response("done", FinishReason::Stop)],
        &client,
    )));
    let mut events = bus.subscribe();
    let run = runtime.delegate_background(Role::Worker, "task".into(), RunConfig::default());
    assert_eq!(runtime.wait(run).await, Ok(AgentRunPhase::Done));
    let _ = tool_events(&mut events);
    assert_eq!(
        runtime
            .btw_snapshot(run)
            .unwrap()
            .ask(QUESTION)
            .await
            .unwrap_err(),
        BtwError::UnexpectedToolCall
    );
    assert_eq!(tool_events(&mut events), 0);
}

#[tokio::test]
async fn a_client_that_cannot_forbid_tools_never_receives_the_question() {
    let client = FakeClient::with(vec![], false, false);
    let (runtime, _bus) = runtime_with(Arc::new(PublishingModel::new(
        vec![text_response("done", FinishReason::Stop)],
        &client,
    )));
    let run = runtime.delegate_background(Role::Worker, "task".into(), RunConfig::default());
    assert_eq!(runtime.wait(run).await, Ok(AgentRunPhase::Done));
    assert_eq!(
        runtime
            .btw_snapshot(run)
            .unwrap()
            .ask(QUESTION)
            .await
            .unwrap_err(),
        BtwError::UnsupportedProvider
    );
    assert!(client.sent().is_empty());
}

#[tokio::test]
async fn a_context_over_the_window_is_refused_before_sending() {
    let client = FakeClient::new(vec![]);
    let mut model = PublishingModel::new(vec![text_response("done", FinishReason::Stop)], &client);
    model.window = Some(1);
    let (runtime, _bus) = runtime_with(Arc::new(model));
    let run = runtime.delegate_background(Role::Worker, "task".into(), RunConfig::default());
    assert_eq!(runtime.wait(run).await, Ok(AgentRunPhase::Done));
    assert_eq!(
        runtime
            .btw_snapshot(run)
            .unwrap()
            .ask(QUESTION)
            .await
            .unwrap_err(),
        BtwError::ContextTooLarge
    );
    assert!(client.sent().is_empty());
}

#[tokio::test]
async fn provider_failures_hide_the_key_and_the_question() {
    let client = FakeClient::new(vec![]);
    let (runtime, _bus) = runtime_with(Arc::new(PublishingModel::new(
        vec![text_response("done", FinishReason::Stop)],
        &client,
    )));
    let run = runtime.delegate_background(Role::Worker, "task".into(), RunConfig::default());
    assert_eq!(runtime.wait(run).await, Ok(AgentRunPhase::Done));
    let error = runtime
        .btw_snapshot(run)
        .unwrap()
        .ask(QUESTION)
        .await
        .unwrap_err();
    let BtwError::ProviderFailed(message) = &error else {
        panic!("{error:?}");
    };
    assert!(
        !message.contains(SECRET) && !message.contains(QUESTION),
        "{message}"
    );
}

#[tokio::test]
async fn dropping_a_pending_question_cancels_the_provider_request() {
    let client = FakeClient::with(vec![text_response("late", FinishReason::Stop)], true, true);
    let (runtime, _bus) = runtime_with(Arc::new(PublishingModel::new(
        vec![text_response("done", FinishReason::Stop)],
        &client,
    )));
    let run = runtime.delegate_background(Role::Worker, "task".into(), RunConfig::default());
    assert_eq!(runtime.wait(run).await, Ok(AgentRunPhase::Done));
    let snapshot = runtime.btw_snapshot(run).unwrap();
    let pending = tokio::spawn(async move { snapshot.ask(QUESTION).await });
    client.arrived.notified().await;
    pending.abort();
    assert!(pending.await.unwrap_err().is_cancelled());
    assert!(client.dropped.load(Ordering::SeqCst));
}

#[tokio::test]
async fn forgetting_a_run_releases_its_context() {
    let client = FakeClient::new(vec![]);
    let (runtime, _bus) = runtime_with(Arc::new(PublishingModel::new(
        vec![text_response("done", FinishReason::Stop)],
        &client,
    )));
    let run = runtime.delegate_background(Role::Worker, "task".into(), RunConfig::default());
    assert_eq!(runtime.wait(run).await, Ok(AgentRunPhase::Done));
    assert!(runtime.btw_snapshot(run).is_ok());
    runtime.forget_btw(run);
    assert_eq!(
        runtime.btw_snapshot(run).unwrap_err(),
        BtwError::NoLocalContext
    );
}
