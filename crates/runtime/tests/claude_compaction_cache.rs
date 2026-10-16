//! Claude subscription compaction through the runtime, routing and HTTP/SSE.
//! Cache tokens are derived from captured input bytes, not production billing.
mod support;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use config::{
    ApiProtocolConfig, Config, CredentialRefConfig, ProviderProfileConfig, ProviderTypeConfig,
    RouteCandidateConfig, SummarizerKind,
};
use event_bus::{
    AgentRunPhase, CompactionEvent, CompactionReason, Event, EventBus, EventKind, EventReceiver,
    LifecycleEvent, ProviderEvent,
};
use mock_openai::cache_contract::{
    CacheProtocol, assert_append_only, assert_compaction_transition,
};
use mock_openai::claude_contract::claude_model_input;
use routing::MapEnv;
use runtime::{ModelSource, Role, RunConfig, RunId, RuntimeComposition, compose_runtime};
use sandbox::credential::{CredentialStore, FileCredentialStore, Secret};
use serde_json::{Value, json};
use tools::{Permissions, Tool, ToolError, ToolExecutor, ToolResult};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

const MODEL: &str = "claude-sonnet-4-6";

struct Read;

#[async_trait::async_trait]
impl Tool for Read {
    fn name(&self) -> &'static str {
        "read"
    }
    fn schema(&self) -> Value {
        json!({"type":"object","properties":{"index":{"type":"integer"}},"required":["index"]})
    }
    fn permissions(&self) -> Permissions {
        Permissions::read_only()
    }
    async fn execute(&self, input: Value) -> Result<ToolResult, ToolError> {
        Ok(ToolResult::success(
            format!("evidence from read {}\n", input["index"]).repeat(100),
        ))
    }
}

struct Captured {
    wire: Value,
    model_input: Value,
    prompt: Vec<u8>,
    cached: u64,
}

#[derive(Clone, Default)]
struct ClaudeEndpoint(Arc<Mutex<Vec<Captured>>>);

impl Respond for ClaudeEndpoint {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        // This independent decoder verifies the fingerprint and checksum before
        // removing only the billing envelope and normalizing the one-hour marker.
        let model_input = claude_model_input(&request.body).unwrap();
        let wire: Value = serde_json::from_slice(&request.body).unwrap();
        let mut normalized = model_input.clone();
        for message in normalized["messages"].as_array_mut().unwrap() {
            for block in message["content"].as_array_mut().unwrap() {
                if block.get("cache_control") == Some(&json!({"type":"ephemeral"})) {
                    block.as_object_mut().unwrap().remove("cache_control");
                }
            }
        }
        let messages = normalized
            .as_object_mut()
            .unwrap()
            .remove("messages")
            .unwrap();
        normalized.as_object_mut().unwrap().remove("stream");
        let mut prompt = serde_json::to_vec(&normalized).unwrap();
        for message in messages.as_array().unwrap() {
            prompt.extend(serde_json::to_vec(message).unwrap());
            prompt.push(b'\n');
        }
        let mut captured = self.0.lock().unwrap();
        let cached = captured
            .iter()
            .map(|old| {
                old.prompt
                    .iter()
                    .zip(&prompt)
                    .take_while(|(a, b)| a == b)
                    .count()
            })
            .max()
            .unwrap_or(0) as u64;
        let total = prompt.len() as u64;
        let index = captured.len();
        captured.push(Captured {
            wire,
            model_input,
            prompt,
            cached,
        });
        let (block, reason) = match index {
            0 | 2 => (
                json!({"type":"tool_use","id":format!("read-{index}"),"name":"_read","input":{"index":index}}),
                "tool_use",
            ),
            1 => (
                json!({"type":"text","text":"Older analysis that may be summarized. ".repeat(100)}),
                "end_turn",
            ),
            3 => (json!({"type":"text","text":"done"}), "end_turn"),
            _ => return ResponseTemplate::new(400).set_body_json(json!({
                "type":"error","error":{"type":"invalid_request_error","message":"unexpected extra model request"}
            })),
        };
        let events = [
            (
                "message_start",
                json!({"type":"message_start","message":{"id":format!("msg-{index}"),"type":"message","role":"assistant","model":MODEL,"content":[],"usage":{"input_tokens":total-cached,"output_tokens":0,"cache_read_input_tokens":cached}}}),
            ),
            (
                "content_block_start",
                json!({"type":"content_block_start","index":0,"content_block":block}),
            ),
            (
                "content_block_stop",
                json!({"type":"content_block_stop","index":0}),
            ),
            (
                "message_delta",
                json!({"type":"message_delta","delta":{"stop_reason":reason},"usage":{"output_tokens":1}}),
            ),
            ("message_stop", json!({"type":"message_stop"})),
        ];
        ResponseTemplate::new(200)
            .insert_header("content-type", "text/event-stream")
            .set_body_string(
                events
                    .into_iter()
                    .map(|(name, value)| format!("event: {name}\ndata: {value}\n\n"))
                    .collect::<String>(),
            )
    }
}

async fn through_phase(
    receiver: &mut EventReceiver,
    run: RunId,
    expected: AgentRunPhase,
) -> Vec<Event> {
    let mut events = Vec::new();
    loop {
        let event = receiver.recv().await.unwrap();
        let reached = match &event.kind {
            EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged { run_id, to, .. })
                if run_id == &run.to_string() =>
            {
                if matches!(
                    to,
                    AgentRunPhase::Done | AgentRunPhase::Error | AgentRunPhase::Stopped
                ) {
                    assert_eq!(*to, expected, "unexpected terminal event: {event:?}");
                }
                *to == expected
            }
            _ => false,
        };
        events.push(event);
        if reached {
            return events;
        }
    }
}

#[tokio::test]
async fn claude_subscription_compaction_replaces_history_once_then_reuses_the_prefix() {
    let review_storage = support::ReviewStorage::new();
    let server = MockServer::start().await;
    let endpoint = ClaudeEndpoint::default();
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"data":[{"id":MODEL}],"has_more":false})),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("authorization", "Bearer fixture-compaction-token"))
        .respond_with(endpoint.clone())
        .expect(4)
        .mount(&server)
        .await;
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(FileCredentialStore::open(directory.path().join("credentials")).unwrap());
    store.set("claude", &Secret::from(json!({"access_token":"fixture-compaction-token","refresh_token":"fixture-refresh","expires_at":4102444800u64}).to_string())).unwrap();
    let mut config = Config::default();
    config.providers.insert(
        "claude".into(),
        ProviderProfileConfig {
            provider_type: ProviderTypeConfig::AnthropicSubscription,
            api_protocol: ApiProtocolConfig::AnthropicMessages,
            base_url: server.uri(),
            credential: CredentialRefConfig::Keyring {
                service: "evorch".into(),
                account: "claude".into(),
            },
            models: vec![config::ModelEntryConfig::enabled(MODEL)],
            default_model: MODEL.into(),
            excluded_models: vec![],
        },
    );
    config.routing.routes.insert(
        "worker".into(),
        vec![RouteCandidateConfig {
            profile: "claude".into(),
            ..Default::default()
        }],
    );
    config.compaction.context_window_tokens = 1_000_000;
    config.compaction.keep_recent_tokens = 1;
    config.compaction.max_summary_bytes = 256;
    config.compaction.summarizer = SummarizerKind::Structural;
    let bus = Arc::new(EventBus::new(1024));
    let mut receiver = bus.subscribe();
    let mut executor = ToolExecutor::new(bus.clone());
    executor.register(Arc::new(Read)).unwrap();
    let runtime = compose_runtime(RuntimeComposition {
        user_config_dir: Some(directory.path().join("empty-user-config")),
        config: &config,
        bus,
        executor: Arc::new(executor),
        credential_store: store,
        env: Arc::new(MapEnv::default()),
        model_source: ModelSource::Configured,
        workspace: None,
    })
    .unwrap()
    .runtime
    .with_compaction(config.compaction.clone());
    runtime
        .set_project_root(directory.path().canonicalize().unwrap())
        .unwrap();
    let run = runtime.delegate_background(
        Role::Worker,
        "Preserve this original goal".into(),
        RunConfig {
            interactive: true,
            ..Default::default()
        },
    );
    let mut events = through_phase(&mut receiver, run, AgentRunPhase::Waiting).await;
    runtime.compact(run).unwrap();
    review_storage.attach(&runtime);
    runtime
        .send_message(run, "Continue after this boundary".into())
        .unwrap();
    events.extend(through_phase(&mut receiver, run, AgentRunPhase::Done).await);
    assert_eq!(runtime.wait(run).await.unwrap(), AgentRunPhase::Done);

    let mut starts = Vec::new();
    let mut completed = HashMap::new();
    let mut pending_checkpoint = None;
    let mut compactions = 0;
    for event in &events {
        match &event.kind {
            EventKind::Compaction(CompactionEvent::Compacted {
                run_id,
                checkpoint_id,
                reason,
                ..
            }) if run_id == &run.to_string() => {
                assert_eq!(*reason, CompactionReason::Manual);
                assert!(pending_checkpoint.replace(checkpoint_id.clone()).is_none());
                compactions += 1;
            }
            EventKind::Provider(ProviderEvent::RequestStarted {
                request_id,
                run_id: Some(id),
                ..
            }) if id == &run.to_string() => {
                starts.push((request_id.clone(), pending_checkpoint.take()));
            }
            EventKind::Provider(ProviderEvent::RequestCompleted {
                request_id,
                run_id: Some(id),
                input_tokens,
                cache_read_tokens,
                ..
            }) if id == &run.to_string() => {
                assert!(
                    completed
                        .insert(request_id.clone(), (*input_tokens, *cache_read_tokens))
                        .is_none()
                );
            }
            EventKind::Diagnostic(diagnostic) => {
                assert_ne!(diagnostic.code, "CacheRegression", "{diagnostic:?}")
            }
            _ => {}
        }
    }
    assert_eq!(compactions, 1);
    assert!(pending_checkpoint.is_none());
    assert_eq!(starts.len(), 4);
    assert_eq!(completed.len(), 4);
    assert_eq!(
        starts
            .iter()
            .enumerate()
            .filter_map(|(index, (_, checkpoint))| checkpoint.as_ref().map(|_| index))
            .collect::<Vec<_>>(),
        [2]
    );
    let captured = endpoint.0.lock().unwrap();
    assert_eq!(captured.len(), 4);
    assert_eq!(captured[0].cached, 0);
    for (index, current) in captured.iter().enumerate() {
        assert_eq!(
            completed[&starts[index].0],
            (current.prompt.len() as u64, current.cached)
        );
        assert_eq!(
            current.wire["system"][1]["cache_control"],
            json!({"type":"ephemeral","ttl":"1h"})
        );
        let markers = current.wire["messages"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|message| message["content"].as_array().unwrap())
            .filter_map(|block| block.get("cache_control"))
            .collect::<Vec<_>>();
        assert!(!markers.is_empty());
        assert!(
            markers
                .iter()
                .all(|marker| **marker == json!({"type":"ephemeral","ttl":"1h"}))
        );
        if index == 0 {
            continue;
        }
        let previous = &captured[index - 1];
        if let Some(checkpoint) = &starts[index].1 {
            assert_compaction_transition(
                CacheProtocol::Anthropic,
                &previous.model_input,
                &current.model_input,
                checkpoint,
            )
            .unwrap();
            assert!(
                current.cached < previous.prompt.len() as u64,
                "compaction must replace part of the old prefix"
            );
        } else {
            assert_append_only(
                CacheProtocol::Anthropic,
                &previous.model_input,
                &current.model_input,
            )
            .unwrap();
            assert!(
                current.cached >= previous.prompt.len() as u64,
                "normal request {index} must reuse the entire prior input"
            );
        }
    }
    let fingerprint = |index: usize| {
        captured[index].wire["system"][0]["text"]
            .as_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
    };
    assert_eq!(fingerprint(0), fingerprint(1));
    assert_eq!(fingerprint(2), fingerprint(3));
}
