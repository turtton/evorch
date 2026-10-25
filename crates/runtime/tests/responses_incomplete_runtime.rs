//! Responses terminal policy through the real HTTP client and agent loop.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use event_bus::{AgentRunPhase, EventBus};
use providers::provider::codex::tokens::{CodexTokenStore, InMemoryTokenStore, TokenBundle};
use providers::provider::codex::{CodexClient, CodexConfig};
use providers::provider::openai::{OpenAiClient, OpenAiConfig};
use providers::provider::openai_compatible::OpenAiCompatibleClient;
use providers::{
    ChatRequest, ChatResponse, ContentBlock, FinishReason, ProviderAuth, ProviderClient, Usage,
};
use runtime::{AgentInvocationContext, AgentModel, AgentRuntime, Role, RunConfig, RuntimeError};
use serde_json::{Value, json};
use tokio::sync::Mutex;
use tools::{Permissions, Tool, ToolError, ToolExecutor, ToolResult};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

struct CountedRead(AtomicUsize);

#[async_trait::async_trait]
impl Tool for CountedRead {
    fn name(&self) -> &'static str {
        "read"
    }
    fn schema(&self) -> Value {
        json!({"type":"object", "required":["path"], "properties":{"path":{"type":"string"}}})
    }
    fn permissions(&self) -> Permissions {
        Permissions::read_only()
    }
    async fn execute(&self, input: Value) -> Result<ToolResult, ToolError> {
        assert_eq!(input, json!({"path":"a"}));
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ToolResult::success("read a"))
    }
}

/// Keep the actual client's final responses observable without replacing SSE or loop logic.
struct HttpModel {
    client: Box<dyn ProviderClient>,
    responses: Mutex<Vec<ChatResponse>>,
}

#[async_trait::async_trait]
impl AgentModel for HttpModel {
    async fn complete(
        &self,
        _: &AgentInvocationContext,
        _: Role,
        _: &[providers::Message],
        _: &[providers::ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        panic!("runtime must exercise Responses SSE")
    }

    async fn complete_streaming(
        &self,
        invocation: &AgentInvocationContext,
        _: Role,
        messages: &[providers::Message],
        tools: &[providers::ToolSpec],
        bus: &EventBus,
    ) -> Result<ChatResponse, RuntimeError> {
        let response = self
            .client
            .send_streaming(
                &ProviderAuth::new("offline-key"),
                &ChatRequest {
                    responses_replay: None,
                    model: "gpt-contract".into(),
                    messages: messages.to_vec(),
                    tools: tools.to_vec(),
                    temperature: None,
                    max_tokens: None,
                    reasoning_effort: None,
                    service_tier: None,
                    output_schema: None,
                    observation: Some(providers::ObservationContext {
                        turn: None,
                        run_id: invocation.run_id.clone(),
                        purpose: Default::default(),
                    }),
                },
                bus,
            )
            .await
            .map_err(|error| RuntimeError::Model {
                reason: error.to_string(),
            })?;
        self.responses.lock().await.push(response.clone());
        Ok(response)
    }

    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "gpt-contract".into()
    }
}

fn event(value: Value) -> String {
    format!(
        "event: {}\ndata: {value}\n\n",
        value["type"].as_str().unwrap()
    )
}

fn responses_body(name: &str, terminal: &str, details: Value) -> String {
    let arguments = if name == "finish" {
        json!({"result":"finished"})
    } else {
        json!({"path":"a"})
    };
    [
        event(json!({"type":"response.output_text.delta", "delta":"partial"})),
        event(json!({"type":"response.reasoning_summary_text.delta", "delta":"thinking"})),
        event(
            json!({"type":"response.output_item.added", "output_index":0,
            "item":{"type":"function_call", "id":"fc_1", "call_id":"call_1",
                "name":name, "arguments":""}}),
        ),
        event(
            json!({"type":"response.function_call_arguments.delta", "output_index":0,
            "item_id":"fc_1", "delta":arguments.to_string()}),
        ),
        event(json!({"type":"response.output_item.done", "output_index":0,
            "item":{"type":"function_call", "id":"fc_1", "call_id":"call_1",
                "name":name, "arguments":arguments.to_string()}})),
        event(json!({"type":terminal, "response":{
            "incomplete_details":details,
            "usage":{"input_tokens":9, "output_tokens":4,
                "input_tokens_details":{"cached_tokens":2},
                "output_tokens_details":{"reasoning_tokens":1}}}})),
    ]
    .concat()
}

fn chat_body(name: &str, reason: &str) -> String {
    let arguments = if name == "finish" {
        json!({"result":"finished"})
    } else {
        json!({"path":"a"})
    };
    format!(
        "data: {}\n\ndata: [DONE]\n\n",
        json!({"choices":[{"index":0, "delta":{"tool_calls":[{"index":0,
            "id":"call_1", "type":"function", "function":{"name":name,
            "arguments":arguments.to_string()}}]}, "finish_reason":reason}],
            "usage":{"prompt_tokens":9,"completion_tokens":4}})
    )
}

/// Real clients whose final response feeds the shared agent loop.
#[derive(Clone, Copy, Debug)]
enum Backend {
    OpenAi,
    /// Codex shares the Responses interpreter but owns its own SSE pump wiring.
    Codex,
    Compatible,
}

/// Both Responses clients must apply the same terminal policy end to end.
const RESPONSES_BACKENDS: [Backend; 2] = [Backend::OpenAi, Backend::Codex];

/// Unexpired (2100-01-01) ID token carrying the account claim Codex requires.
const CODEX_ID_TOKEN: &str = "e30.eyJleHAiOjQxMDI0NDQ4MDAsImh0dHBzOi8vYXBpLm9wZW5haS5jb20vYXV0aCI6eyJjaGF0Z3B0X2FjY291bnRfaWQiOiJhY2MifX0.signature";

fn codex_client(server: &MockServer, bus: Arc<EventBus>) -> CodexClient {
    let store = Arc::new(InMemoryTokenStore::new());
    store
        .save(&TokenBundle {
            access_token: "access".into(),
            refresh_token: "refresh".into(),
            id_token: CODEX_ID_TOKEN.into(),
        })
        .unwrap();
    CodexClient::with_config(
        CodexConfig {
            base_url: format!("{}/backend-api/codex", server.uri()),
            auth_base_url: server.uri(),
            event_bus: Some(bus),
            client_version: providers::CodexClientVersion::Fixed(providers::CodexCatalogVersion {
                version: providers::CODEX_MODELS_FALLBACK_VERSION.into(),
                warning: None,
            }),
            ..CodexConfig::default()
        },
        store,
    )
    .unwrap()
}

async fn run_case(
    body: String,
    backend: Backend,
    phase: AgentRunPhase,
    executions: usize,
    requests: usize,
) -> ChatResponse {
    let server = MockServer::start().await;
    let compatible = matches!(backend, Backend::Compatible);
    let endpoint = match backend {
        Backend::OpenAi => "/responses",
        Backend::Codex => "/backend-api/codex/responses",
        Backend::Compatible => "/chat/completions",
    };
    let template = |body: String| {
        ResponseTemplate::new(200)
            .insert_header("content-type", "text/event-stream")
            .set_body_string(body)
    };
    Mock::given(method("POST"))
        .and(path(endpoint))
        .respond_with(template(body))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    // A buggy continuation must terminate rather than hang or hide the extra request.
    Mock::given(method("POST"))
        .and(path(endpoint))
        .respond_with(template(if compatible {
            chat_body("finish", "tool_calls")
        } else {
            responses_body("finish", "response.completed", Value::Null)
        }))
        .with_priority(2)
        .mount(&server)
        .await;
    let bus = Arc::new(EventBus::new(256));
    let mut receiver = bus.subscribe();
    let tool = Arc::new(CountedRead(AtomicUsize::new(0)));
    let mut executor = ToolExecutor::new(bus.clone());
    executor.register(tool.clone()).unwrap();
    let client: Box<dyn ProviderClient> = match backend {
        Backend::Compatible => Box::new(
            OpenAiCompatibleClient::new(
                server.uri(),
                "openai-compatible",
                Duration::from_secs(10),
                Some(bus.clone()),
            )
            .unwrap(),
        ),
        Backend::OpenAi => Box::new(
            OpenAiClient::new(OpenAiConfig {
                base_url: server.uri(),
                event_bus: Some(bus.clone()),
                ..OpenAiConfig::default()
            })
            .unwrap(),
        ),
        Backend::Codex => Box::new(codex_client(&server, bus.clone())),
    };
    let model = Arc::new(HttpModel {
        client,
        responses: Mutex::new(Vec::new()),
    });
    let runtime = AgentRuntime::new(bus, Arc::new(executor), model.clone());
    let run =
        runtime.delegate_background(Role::Orchestrator, "read a".into(), RunConfig::default());
    let actual = tokio::time::timeout(Duration::from_secs(5), runtime.wait(run))
        .await
        .expect("run terminates")
        .unwrap();
    let diagnostics: Vec<_> = receiver
        .drain_pending_snapshot()
        .into_iter()
        .map(|event| event.kind)
        .collect();
    assert_eq!(
        tool.0.load(Ordering::SeqCst),
        executions,
        "{backend:?} tool execution count: {diagnostics:?}"
    );
    assert_eq!(
        actual, phase,
        "{backend:?} terminal run phase: {diagnostics:?}"
    );
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        requests,
        "{backend:?} model requests"
    );
    let responses = model.responses.lock().await;
    responses[0].clone()
}

fn assert_partial(backend: Backend, response: &ChatResponse, reason: FinishReason) {
    assert_eq!(response.finish_reason, reason, "{backend:?}");
    assert_eq!(
        response.message.content,
        vec![
            ContentBlock::Reasoning {
                text: "thinking".into()
            },
            ContentBlock::Text {
                text: "partial".into()
            },
        ],
        "{backend:?}"
    );
    assert_eq!(
        response.usage,
        Usage {
            input_tokens: 9,
            output_tokens: 4,
            cache_read_tokens: 2,
            cache_write_tokens: 0,
            reasoning_tokens: Some(1),
        },
        "{backend:?}"
    );
}

// Given: valid tool deltas followed by an unknown incomplete reason
// When: OpenAiClient or CodexClient completes through runtime / Then: no tool or
// continuation, Error; partial text, reasoning, usage and Other survive final assembly.
#[tokio::test]
async fn unknown_incomplete_does_not_execute_a_valid_function() {
    for backend in RESPONSES_BACKENDS {
        let response = run_case(
            responses_body(
                "read",
                "response.incomplete",
                json!({"reason":"future_reason"}),
            ),
            backend,
            AgentRunPhase::Error,
            0,
            1,
        )
        .await;
        assert_partial(
            backend,
            &response,
            FinishReason::Other("incomplete: future_reason".into()),
        );
    }
}

// Given: a valid finish call and no incomplete reason / Then: it cannot turn the run Done.
#[tokio::test]
async fn missing_incomplete_reason_does_not_execute_finish() {
    for backend in RESPONSES_BACKENDS {
        for details in [json!({}), Value::Null] {
            let response = run_case(
                responses_body("finish", "response.incomplete", details),
                backend,
                AgentRunPhase::Error,
                0,
                1,
            )
            .await;
            assert_partial(backend, &response, FinishReason::Other("incomplete".into()));
        }
    }
}

// Given: the same function deltas with a normal completed terminal / Then: execution continues.
// This also proves each backend's harness can execute tools, so the negative cases are not vacuous.
#[tokio::test]
async fn completed_executes_the_same_function() {
    for backend in RESPONSES_BACKENDS {
        let response = run_case(
            responses_body("read", "response.completed", Value::Null),
            backend,
            AgentRunPhase::Done,
            1,
            2,
        )
        .await;
        assert_eq!(response.finish_reason, FinishReason::ToolUse, "{backend:?}");
        assert!(
            response
                .message
                .content
                .iter()
                .any(|block| matches!(block, ContentBlock::ToolUse { name, .. } if name == "read")),
            "{backend:?}"
        );
    }
}

// Given: documented incomplete reasons / Then: their existing tool behavior is unchanged.
#[tokio::test]
async fn known_incomplete_reasons_keep_tool_execution() {
    for backend in RESPONSES_BACKENDS {
        for (reason, finish) in [
            ("max_output_tokens", FinishReason::Length),
            ("content_filter", FinishReason::ContentFilter),
        ] {
            let response = run_case(
                responses_body("read", "response.incomplete", json!({"reason":reason})),
                backend,
                AgentRunPhase::Done,
                1,
                2,
            )
            .await;
            assert_eq!(response.finish_reason, finish, "{backend:?}");
        }
    }
}

// Given: openai-compatible Length plus valid tool / Then: the shared loop still executes it.
#[tokio::test]
async fn compatible_length_keeps_tool_execution() {
    let response = run_case(
        chat_body("read", "length"),
        Backend::Compatible,
        AgentRunPhase::Done,
        1,
        2,
    )
    .await;
    assert_eq!(response.finish_reason, FinishReason::Length);
}
