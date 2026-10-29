//! Config -> runtime admission -> native authentication -> streamed tool turns.
use config::{
    ApiProtocolConfig, Config, CredentialRefConfig, ProviderProfileConfig, ProviderTypeConfig,
};
use event_bus::{
    CacheBaselineMissing, CacheComparison, EventBus, EventKind, ProviderEvent, RequestPurpose,
};
use mock_openai::claude_contract::claude_model_input;
use providers::{ContentBlock, Message, Role as MessageRole, ToolResultContent, ToolSpec};
use routing::{ComposeDeps, MapEnv};
use runtime::{AgentInvocationContext, AgentModel, ModelPreference, Role};
use sandbox::credential::{CredentialStore, FileCredentialStore, Secret};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

#[derive(Default)]
struct Oracle {
    requests: Vec<Value>,
    model_inputs: Vec<Value>,
    prompts: Vec<Vec<u8>>,
    cached: Vec<u64>,
    searches: Vec<Value>,
    /// API-key requests must never carry subscription-only transport headers.
    api_key_header_leaks: Vec<String>,
}

#[derive(Clone)]
struct ClaudeEndpoint(Arc<Mutex<Oracle>>);

impl Respond for ClaudeEndpoint {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        let oauth = request.headers.contains_key("authorization");
        if !oauth {
            let leaks = request
                .headers
                .keys()
                .map(|name| name.as_str().to_owned())
                .filter(|name| name == "anthropic-beta" || name.starts_with("x-stainless-"))
                .collect::<Vec<_>>();
            self.0.lock().unwrap().api_key_header_leaks.extend(leaks);
        }
        let model_input = if oauth {
            claude_model_input(&request.body).unwrap()
        } else {
            body.clone()
        };
        if body["tools"].as_array().is_some_and(|tools| {
            tools
                .iter()
                .any(|tool| tool["type"] == "web_search_20250305")
        }) {
            assert_eq!(body["model"], "claude-sonnet-4-6");
            assert_eq!(body["messages"].as_array().unwrap().len(), 1);
            assert_eq!(body["tools"][0]["name"], "web_search");
            assert!(body["tools"][0].get("input_schema").is_none());
            assert!(!request.body.windows(13).any(|s| s == b"cache_control"));
            self.0.lock().unwrap().searches.push(model_input);
            return ResponseTemplate::new(200).set_body_json(json!({
                "id":"msg_search", "type":"message", "role":"assistant", "stop_reason":"end_turn",
                "content":[
                    {"type":"server_tool_use", "id":"srvtoolu_search", "name":"web_search", "input":{"query":"source"}},
                    {"type":"web_search_tool_result", "tool_use_id":"srvtoolu_search", "content":[
                        {"type":"web_search_result", "title":"Source", "url":"https://example.com/source", "encrypted_content":"opaque-search-result"}
                    ]},
                    {"type":"text", "text":"A sourced answer.", "citations":[
                        {"type":"web_search_result_location", "title":"Source", "url":"https://example.com/source", "encrypted_index":"opaque-index", "cited_text":"A sourced answer."}
                    ]}
                ],
                "usage":{"input_tokens":31,"output_tokens":7,"server_tool_use":{"web_search_requests":1}}
            }));
        }
        assert_eq!(body["max_tokens"], 128_000);
        let mut normalized = model_input.clone();
        // Normalize only the moving Anthropic breakpoint, not nested arguments.
        for message in normalized["messages"].as_array_mut().unwrap() {
            for block in message["content"].as_array_mut().unwrap() {
                block.as_object_mut().unwrap().remove("cache_control");
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
        let mut oracle = self.0.lock().unwrap();
        let cached = oracle
            .prompts
            .iter()
            .map(|old| old.iter().zip(&prompt).take_while(|(a, b)| a == b).count())
            .max()
            .unwrap_or(0) as u64;
        let total = prompt.len() as u64;
        let index = oracle.requests.len();
        let tool_name = match (index, oauth) {
            (0, true) => "_read",
            (0, false) => "read",
            (_, true) => "_mcp__evorch__inspect_run",
            (_, false) => "inspect_run",
        };
        let tool_input = if index == 0 {
            json!({"path":"report.txt","offset":1,"limit":20})
        } else {
            json!({"run_id":"run-1","path":"custom-path"})
        };
        oracle.requests.push(body);
        oracle.model_inputs.push(model_input);
        oracle.prompts.push(prompt);
        oracle.cached.push(cached);
        let block = if index < 2 {
            json!({"type":"tool_use", "id":format!("call-{index}"), "name":tool_name, "input":tool_input})
        } else {
            json!({"type":"text", "text":"done"})
        };
        let reason = if index < 2 { "tool_use" } else { "end_turn" };
        let events = [
            (
                "message_start",
                json!({"type":"message_start", "message":{"id":"msg", "type":"message", "role":"assistant", "content":[], "usage":{"input_tokens":total-cached,"output_tokens":0,"cache_read_input_tokens":cached}}}),
            ),
            (
                "content_block_start",
                json!({"type":"content_block_start", "index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}),
            ),
            (
                "content_block_delta",
                json!({"type":"content_block_delta", "index":0,"delta":{"type":"thinking_delta","thinking":format!("reasoning-{index}")}}),
            ),
            (
                "content_block_delta",
                json!({"type":"content_block_delta", "index":0,"delta":{"type":"signature_delta","signature":"replaced-signature"}}),
            ),
            (
                "content_block_delta",
                json!({"type":"content_block_delta", "index":0,"delta":{"type":"signature_delta","signature":format!("signature-{index}")}}),
            ),
            (
                "content_block_stop",
                json!({"type":"content_block_stop", "index":0}),
            ),
            (
                "content_block_start",
                json!({"type":"content_block_start", "index":1,"content_block":{"type":"redacted_thinking","data":format!("opaque-{index}")}}),
            ),
            (
                "content_block_stop",
                json!({"type":"content_block_stop", "index":1}),
            ),
            (
                "content_block_start",
                json!({"type":"content_block_start", "index":2,"content_block":block}),
            ),
            (
                "content_block_stop",
                json!({"type":"content_block_stop", "index":2}),
            ),
            (
                "message_delta",
                json!({"type":"message_delta", "delta":{"stop_reason":reason},"usage":{"output_tokens":1}}),
            ),
            ("message_stop", json!({"type":"message_stop"})),
        ];
        let sse = events
            .into_iter()
            .map(|(name, value)| format!("event: {name}\ndata: {value}\n\n"))
            .collect::<String>();
        ResponseTemplate::new(200)
            .insert_header("content-type", "text/event-stream")
            .set_body_string(sse)
    }
}

const TOKEN: &str = "fixture-claude-token";

struct Harness {
    server: MockServer,
    oracle: Arc<Mutex<Oracle>>,
    model: Arc<runtime::compose::RoutedModel>,
    bus: Arc<EventBus>,
    _credentials: tempfile::TempDir,
}

/// Mounts the Claude endpoint and composes the production routed model against it.
/// `effort_levels` is the model's explicit `effort_levels` configuration, if any.
async fn harness(oauth: bool, effort_levels: Option<Vec<String>>, posts: u64) -> Harness {
    let server = MockServer::start().await;
    let oracle = Arc::new(Mutex::new(Oracle::default()));
    let token = TOKEN;
    let auth_header = if oauth { "authorization" } else { "x-api-key" };
    let auth_value = if oauth {
        format!("Bearer {token}")
    } else {
        token.to_owned()
    };
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(header(auth_header, &auth_value))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"data":[{"id":"claude-sonnet-4-6"}],"has_more":false})),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header(auth_header, &auth_value))
        .respond_with(ClaudeEndpoint(oracle.clone()))
        .expect(posts)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let store: Arc<dyn CredentialStore> = Arc::new(FileCredentialStore::open(dir.path()).unwrap());
    store.set("claude", &Secret::from(json!({"access_token":token,"refresh_token":"fixture-refresh","expires_at":4102444800u64}).to_string())).unwrap();
    let mut config = Config::default();
    config.providers.insert(
        "claude".into(),
        ProviderProfileConfig {
            provider_type: if oauth {
                ProviderTypeConfig::AnthropicSubscription
            } else {
                ProviderTypeConfig::Anthropic
            },
            api_protocol: ApiProtocolConfig::AnthropicMessages,
            base_url: server.uri(),
            credential: if oauth {
                CredentialRefConfig::Keyring {
                    service: "evorch".into(),
                    account: "claude".into(),
                }
            } else {
                CredentialRefConfig::Env {
                    var: "CLAUDE_KEY".into(),
                }
            },
            models: vec![config::ModelEntryConfig::enabled("claude-sonnet-4-6")],
            default_model: "claude-sonnet-4-6".into(),
            excluded_models: vec![],
            edit_tool: None,
        },
    );
    config.providers.get_mut("claude").unwrap().models[0].preset = Some("output-limit".into());
    config.providers.get_mut("claude").unwrap().models[0].effort_levels = effort_levels;
    config.model_presets.insert(
        "output-limit".into(),
        config::ModelPresetConfig {
            max_output_tokens: Some(128_000),
            ..Default::default()
        },
    );
    let bus = Arc::new(EventBus::new(128));
    let model = runtime::compose::compose_routed_model(
        &config,
        ComposeDeps {
            credential_store: store,
            event_bus: Some(bus.clone()),
            env: Arc::new(MapEnv::from_iter([("CLAUDE_KEY", token)])),
            catalog: model::ModelCatalog::new(),
            factory: routing::factory::FactoryOptions::default(),
        },
        Some(test_support::model_catalog_dir()),
    )
    .unwrap();
    Harness {
        server,
        oracle,
        model,
        bus,
        _credentials: dir,
    }
}

async fn exercise(oauth: bool, effort: Option<&str>) {
    // The model lists its efforts explicitly, as the GUI picker requires for API profiles.
    let levels = effort.map(|_| ["low", "medium", "high"].map(str::to_owned).to_vec());
    let Harness {
        server: _server,
        oracle,
        model,
        bus,
        _credentials,
    } = harness(oauth, levels, 5).await;
    let mut events = bus.subscribe();
    let invocation = AgentInvocationContext {
        attempts: None,
        run_id: "claude-cache-contract".into(),
        model_preference: Some(ModelPreference {
            profile: "claude".into(),
            model: Some("claude-sonnet-4-6".into()),
            reasoning_effort: effort.map(str::to_owned),
        }),
        ..Default::default()
    };
    let tools = vec![
        ToolSpec {
            name: "read".into(),
            description: "Read a bounded UTF-8 file range".into(),
            input_schema: json!({"type":"object","properties":{"path":{"type":"string"},"offset":{"type":"integer","minimum":1},"limit":{"type":"integer","minimum":1}},"required":["path"]}),
        },
        ToolSpec {
            name: "inspect_run".into(),
            description: "Inspect a run".into(),
            input_schema: json!({"type":"object","properties":{"run_id":{"type":"string"},"path":{"type":"string"}},"required":["run_id"]}),
        },
    ];
    let mut messages = vec![
        Message {
            role: MessageRole::System,
            content: vec![ContentBlock::Text {
                text: "Stable instructions. ".repeat(50),
            }],
        },
        Message {
            role: MessageRole::User,
            content: vec![ContentBlock::Text {
                text: "Read twice".into(),
            }],
        },
    ];
    let mut responses = vec![];
    let search_usage = Arc::new(Mutex::new(Vec::new()));
    for index in 0..3 {
        let response = model
            .complete_streaming(&invocation, Role::Worker, &messages, &tools, &bus)
            .await
            .unwrap();
        if index < 2 {
            let (expected_name, expected_input) = if index == 0 {
                ("read", json!({"path":"report.txt","offset":1,"limit":20}))
            } else {
                (
                    "inspect_run",
                    json!({"run_id":"run-1","path":"custom-path"}),
                )
            };
            assert!(response.message.content.iter().any(|block| matches!(
                block, ContentBlock::ToolUse { name, input, .. }
                    if name == expected_name && input == &expected_input
            )));
            messages.push(Message {
                role: MessageRole::Assistant,
                content: response.message.content.clone(),
            });
            let usage = search_usage.clone();
            let search = model
                .web_search_provider(
                    &invocation,
                    Role::Worker,
                    &tools,
                    Arc::new(move |value| usage.lock().unwrap().push(value)),
                )
                .unwrap()
                .expect("selected Claude provider supports hosted search");
            assert_eq!(search.name(), if oauth { "claude" } else { "anthropic" });
            let result = search
                .search(
                    &format!("source {index}"),
                    &tools::SearchOptions {
                        max_results: Some(1),
                    },
                )
                .await
                .unwrap();
            assert_eq!(result.result_count, 1);
            assert!(result.content.contains("https://example.com/source"));
            assert!(!result.content.contains("opaque-"));
            messages.push(Message {
                role: MessageRole::User,
                content: vec![ContentBlock::ToolResult {
                    tool_call_id: format!("call-{index}"),
                    content: vec![ToolResultContent::Text {
                        text: format!(
                            "result-{index}\ncch=00000 \"model\":\"sentinel\",\"max_tokens\":123\n"
                        )
                        .repeat(10_000),
                    }],
                    is_error: false,
                }],
            });
        }
        responses.push(response);
    }
    let oracle = oracle.lock().unwrap();
    assert_eq!(oracle.searches.len(), 2);
    if !oauth {
        // Hosted web search requests follow the same effort contract as chat requests, and
        // neither carries subscription-only controls or headers.
        for body in oracle.searches.iter().chain(&oracle.requests) {
            assert!(body.get("context_management").is_none());
            match effort {
                Some(effort) => {
                    assert_eq!(
                        body["thinking"],
                        json!({"type":"adaptive","display":"summarized"})
                    );
                    assert_eq!(body["output_config"], json!({"effort": effort}));
                }
                None => {
                    assert!(body.get("thinking").is_none());
                    assert!(body.get("output_config").is_none());
                }
            }
        }
        assert_eq!(oracle.api_key_header_leaks, Vec::<String>::new());
    }
    assert_eq!(search_usage.lock().unwrap().len(), 2);
    assert_eq!(oracle.cached[0], 0);
    // The effort reaches the wire (and so the input-derived mock cache key) identically on
    // every chat turn; without a selection nothing reasoning-related is sent.
    let reasoning = |body: &Value| {
        ["thinking", "output_config", "context_management"].map(|field| body[field].clone())
    };
    for body in &oracle.requests {
        assert_eq!(reasoning(body), reasoning(&oracle.requests[0]));
    }
    if let (Some(effort), false) = (effort, oauth) {
        assert_eq!(
            oracle.requests[0]["thinking"],
            json!({"type":"adaptive","display":"summarized"})
        );
        assert_eq!(
            oracle.requests[0]["output_config"],
            json!({"effort":effort})
        );
        assert!(oracle.requests[0].get("context_management").is_none());
        assert!(
            oracle
                .model_inputs
                .iter()
                .all(|input| input["thinking"].is_object())
        );
    } else if effort.is_none() {
        assert_eq!(
            reasoning(&oracle.requests[0]),
            [Value::Null, Value::Null, Value::Null]
        );
    }
    for (index, response) in responses.iter().enumerate().skip(1) {
        mock_openai::cache_contract::assert_append_only(
            mock_openai::cache_contract::CacheProtocol::Anthropic,
            &oracle.model_inputs[index - 1],
            &oracle.model_inputs[index],
        )
        .unwrap();
        assert!(oracle.cached[index] >= oracle.prompts[index - 1].len() as u64);
        assert_eq!(response.usage.cache_read_tokens, oracle.cached[index]);
    }
    assert_eq!(
        oracle.requests[2]["messages"][2]["content"][0]["content"][0]["text"],
        "result-0\ncch=00000 \"model\":\"sentinel\",\"max_tokens\":123\n".repeat(10_000)
    );
    assert_eq!(
        oracle.requests[1]["messages"][2]["content"][0]["content"],
        oracle.requests[2]["messages"][2]["content"][0]["content"]
    );
    for turn in 0..2 {
        let blocks = oracle.requests[2]["messages"][1 + turn * 2]["content"]
            .as_array()
            .unwrap();
        assert_eq!(blocks.len(), 3);
        assert_eq!(
            blocks[0],
            json!({"type":"thinking","thinking":format!("reasoning-{turn}"),"signature":format!("signature-{turn}")})
        );
        assert_eq!(
            blocks[1],
            json!({"type":"redacted_thinking","data":format!("opaque-{turn}")})
        );
        assert_eq!(blocks[2]["type"], "tool_use");
        assert_eq!(blocks[2]["id"], format!("call-{turn}"));
        let expected_name = match (turn, oauth) {
            (0, true) => "_read",
            (0, false) => "read",
            (_, true) => "_mcp__evorch__inspect_run",
            (_, false) => "inspect_run",
        };
        assert_eq!(blocks[2]["name"], expected_name);
        if turn == 0 {
            assert_eq!(
                blocks[2]["input"],
                json!({"path":"report.txt","offset":1,"limit":20})
            );
        } else {
            assert_eq!(
                blocks[2]["input"],
                json!({"run_id":"run-1","path":"custom-path"})
            );
        }
    }
    let events = events.drain_pending_snapshot();
    let completed: Vec<_> = events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::Provider(ProviderEvent::RequestCompleted {
                request_id,
                purpose,
                ..
            }) => Some((request_id, purpose)),
            _ => None,
        })
        .collect();
    assert_eq!(completed.len(), 5);
    assert_eq!(
        completed
            .iter()
            .filter(|(_, purpose)| **purpose == Some(RequestPurpose::WebSearch))
            .count(),
        2
    );
    let chats: Vec<_> = completed
        .iter()
        .filter(|(_, purpose)| **purpose == Some(RequestPurpose::Agent))
        .collect();
    assert_eq!(chats.len(), 3);
    let cache: Vec<_> = events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::Provider(ProviderEvent::CacheReuseObserved { comparison, .. }) => {
                Some(comparison)
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        cache.len(),
        3,
        "hosted search must preserve the chat cache baseline"
    );
    assert!(
        matches!(cache[2], CacheComparison::Compared { previous_request_id, .. } if previous_request_id == chats[1].0)
    );
    assert!(events.iter().all(
        |event| !matches!(&event.kind, EventKind::Diagnostic(d) if d.code == "CacheRegression")
    ));
}

#[tokio::test]
async fn claude_api_key_runtime_tool_turns_and_hosted_search_preserve_prefix() {
    exercise(false, None).await;
}

#[tokio::test]
async fn claude_api_key_runtime_with_reasoning_effort_preserves_prefix() {
    exercise(false, Some("medium")).await;
}

#[tokio::test]
async fn claude_oauth_runtime_tool_turns_and_hosted_search_preserve_prefix() {
    exercise(true, None).await;
}

// Given: one run on an API-key profile whose model lists its efforts, as selected from the picker
// When: the selected effort goes medium -> medium -> high -> high through the runtime
// Then: each wire carries the selected effort, the change is an explicit boundary that reuses
// none of the old history, and the new setting is reused again on the next turn. The mock cache
// is derived from the received bytes, and `assert_append_only` is an independent oracle.
#[tokio::test]
async fn claude_api_key_effort_change_is_a_boundary_and_the_new_setting_is_reused() {
    let levels = ["low", "medium", "high"].map(str::to_owned).to_vec();
    let Harness {
        server: _server,
        oracle,
        model,
        bus,
        _credentials,
    } = harness(false, Some(levels), 4).await;
    let mut events = bus.subscribe();
    let mut messages = vec![
        Message {
            role: MessageRole::System,
            content: vec![ContentBlock::Text {
                text: "Stable instructions. ".repeat(50),
            }],
        },
        Message {
            role: MessageRole::User,
            content: vec![ContentBlock::Text {
                text: "Read twice".into(),
            }],
        },
    ];
    let tools = vec![
        ToolSpec {
            name: "read".into(),
            description: "Read a bounded UTF-8 file range".into(),
            input_schema: json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}),
        },
        ToolSpec {
            name: "inspect_run".into(),
            description: "Inspect a run".into(),
            input_schema: json!({"type":"object","properties":{"run_id":{"type":"string"}},"required":["run_id"]}),
        },
    ];
    for (index, effort) in ["medium", "medium", "high", "high"].into_iter().enumerate() {
        let invocation = AgentInvocationContext {
            run_id: "claude-effort-boundary".into(),
            model_preference: Some(ModelPreference {
                profile: "claude".into(),
                model: Some("claude-sonnet-4-6".into()),
                reasoning_effort: Some(effort.into()),
            }),
            ..Default::default()
        };
        let response = model
            .complete_streaming(&invocation, Role::Worker, &messages, &tools, &bus)
            .await
            .unwrap();
        messages.push(Message {
            role: MessageRole::Assistant,
            content: response.message.content.clone(),
        });
        let call = response
            .message
            .content
            .iter()
            .find_map(|block| match block {
                ContentBlock::ToolUse { id, .. } => Some(id.clone()),
                _ => None,
            });
        messages.push(Message {
            role: MessageRole::User,
            content: vec![match call {
                Some(tool_call_id) => ContentBlock::ToolResult {
                    tool_call_id,
                    content: vec![ToolResultContent::Text {
                        text: format!("result-{index}\n").repeat(2_000),
                    }],
                    is_error: false,
                },
                None => ContentBlock::Text {
                    text: format!("continue-{index}"),
                },
            }],
        });
    }
    let oracle = oracle.lock().unwrap();
    assert_eq!(oracle.requests.len(), 4);
    for (body, effort) in oracle
        .requests
        .iter()
        .zip(["medium", "medium", "high", "high"])
    {
        assert_eq!(
            body["thinking"],
            json!({"type":"adaptive","display":"summarized"})
        );
        assert_eq!(body["output_config"], json!({"effort": effort}));
    }
    let append_only = |from: usize| {
        mock_openai::cache_contract::assert_append_only(
            mock_openai::cache_contract::CacheProtocol::Anthropic,
            &oracle.model_inputs[from],
            &oracle.model_inputs[from + 1],
        )
    };
    // Same settings: the whole previous prompt is reused.
    append_only(0).unwrap();
    assert!(oracle.cached[1] >= oracle.prompts[0].len() as u64);
    // The change: not append-only, and none of the old history counts as reused.
    assert!(append_only(1).is_err());
    assert!(
        oracle.cached[2] < oracle.prompts[1].len() as u64 / 2,
        "the previous history must not be reused across an effort change"
    );
    // After the boundary, the new settings continue the new prefix.
    append_only(2).unwrap();
    assert!(oracle.cached[3] >= oracle.prompts[2].len() as u64);
    assert_eq!(oracle.api_key_header_leaks, Vec::<String>::new());
    // The production observer agrees: the boundary has no baseline (it is not a regression),
    // and the first turn on the new setting is compared with the boundary request.
    let events = events.drain_pending_snapshot();
    let completed: Vec<_> = events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::Provider(ProviderEvent::RequestCompleted { request_id, .. }) => {
                Some(request_id)
            }
            _ => None,
        })
        .collect();
    let cache: Vec<_> = events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::Provider(ProviderEvent::CacheReuseObserved { comparison, .. }) => {
                Some(comparison)
            }
            _ => None,
        })
        .collect();
    assert_eq!(completed.len(), 4);
    assert_eq!(cache.len(), 4);
    // The mock reports no cache write on the first request, so turn 1 has nothing to compare.
    assert!(matches!(
        cache[1],
        CacheComparison::NoBaseline {
            reason: CacheBaselineMissing::PreviousUncached
        }
    ));
    assert!(matches!(
        cache[2],
        CacheComparison::NoBaseline {
            reason: CacheBaselineMissing::PrefixChanged
        }
    ));
    assert!(
        matches!(cache[3], CacheComparison::Compared { previous_request_id, .. } if previous_request_id == completed[2])
    );
    assert!(events.iter().all(
        |event| !matches!(&event.kind, EventKind::Diagnostic(d) if d.code == "CacheRegression")
    ));
}
