//! Config -> runtime admission -> native authentication -> streamed tool turns.
use config::{
    ApiProtocolConfig, Config, CredentialRefConfig, ProviderProfileConfig, ProviderTypeConfig,
};
use event_bus::EventBus;
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
    prompts: Vec<Vec<u8>>,
    cached: Vec<u64>,
}

#[derive(Clone)]
struct ClaudeEndpoint(Arc<Mutex<Oracle>>);

impl Respond for ClaudeEndpoint {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        let mut normalized = body.clone();
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
        let oauth = request.headers.contains_key("authorization");
        let tool_name = match (index, oauth) {
            (0, true) => "Read",
            (0, false) => "read",
            (_, true) => "mcp__evorch__inspect_run",
            (_, false) => "inspect_run",
        };
        let tool_input = if index == 0 {
            if oauth {
                json!({"file_path":"report.txt","offset":1,"limit":20})
            } else {
                json!({"path":"report.txt","offset":1,"limit":20})
            }
        } else {
            json!({"run_id":"run-1","path":"custom-path"})
        };
        oracle.requests.push(body);
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

async fn exercise(oauth: bool) {
    let server = MockServer::start().await;
    let oracle = Arc::new(Mutex::new(Oracle::default()));
    let token = "fixture-claude-token";
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
        .expect(3)
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
    )
    .unwrap();
    let invocation = AgentInvocationContext {
        run_id: "claude-cache-contract".into(),
        model_preference: Some(ModelPreference {
            profile: "claude".into(),
            model: Some("claude-sonnet-4-6".into()),
            reasoning_effort: None,
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
            messages.push(Message {
                role: MessageRole::User,
                content: vec![ContentBlock::ToolResult {
                    tool_call_id: format!("call-{index}"),
                    content: vec![ToolResultContent::Text {
                        text: format!("result-{index}\n").repeat(10_000),
                    }],
                    is_error: false,
                }],
            });
        }
        responses.push(response);
    }
    let oracle = oracle.lock().unwrap();
    assert_eq!(oracle.cached[0], 0);
    for (index, response) in responses.iter().enumerate().skip(1) {
        mock_openai::cache_contract::assert_append_only(
            mock_openai::cache_contract::CacheProtocol::Anthropic,
            &oracle.requests[index - 1],
            &oracle.requests[index],
        )
        .unwrap();
        assert!(oracle.cached[index] >= oracle.prompts[index - 1].len() as u64);
        assert_eq!(response.usage.cache_read_tokens, oracle.cached[index]);
    }
    let last = oracle.requests[2]["messages"].to_string();
    assert!(last.contains(&"result-0\n".repeat(10_000).replace('\n', "\\n")));
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
            (0, true) => "Read",
            (0, false) => "read",
            (_, true) => "mcp__evorch__inspect_run",
            (_, false) => "inspect_run",
        };
        assert_eq!(blocks[2]["name"], expected_name);
        if turn == 0 {
            assert_eq!(
                blocks[2]["input"],
                if oauth {
                    json!({"file_path":"report.txt","offset":1,"limit":20})
                } else {
                    json!({"path":"report.txt","offset":1,"limit":20})
                }
            );
        } else {
            assert_eq!(
                blocks[2]["input"],
                json!({"run_id":"run-1","path":"custom-path"})
            );
        }
    }
}

#[tokio::test]
async fn claude_api_key_runtime_tool_turns_preserve_prefix() {
    exercise(false).await;
}

#[tokio::test]
async fn claude_oauth_runtime_tool_turns_preserve_prefix() {
    exercise(true).await;
}
