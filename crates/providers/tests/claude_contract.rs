//! Claude's native OAuth wire and cost-sensitive prompt contracts.
use futures_util::StreamExt;
use mock_openai::cache_contract::{CacheProtocol, assert_append_only};
use mock_openai::claude_contract::claude_model_input;
use providers::provider::anthropic::{AnthropicClient, AnthropicConfig};
use providers::provider::claude::*;
use providers::{ChatRequest, ProviderAuth, ProviderClient, StreamEvent};
use serde_json::{Value, json};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use wiremock::matchers::{body_partial_json, header, headers, method, path, query_param};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

fn bundle(expires_at: u64) -> ClaudeTokenBundle {
    ClaudeTokenBundle {
        access_token: "access-secret".into(),
        refresh_token: "refresh-secret".into(),
        expires_at,
        account_id: Some("account-secret".into()),
        email: None,
        org_id: Some("original-org".into()),
        org_name: None,
    }
}
fn oauth(server: &MockServer) -> ClaudeOAuthClient {
    ClaudeOAuthClient::new(ClaudeOAuthConfig {
        authorize_url: format!("{}/authorize", server.uri()),
        token_url: format!("{}/token", server.uri()),
        ..Default::default()
    })
    .unwrap()
}
fn request() -> ChatRequest {
    serde_json::from_value(json!({ "model": "claude-test", "max_tokens": 1024,
        "messages": [{"role":"system", "content":[{"type":"text", "text":"stable instructions".repeat(100)}]},
            {"role":"user", "content":[{"type":"text", "text":"Read 東京 report"}]}],
        "observation": {"run_id":"claude-cache-run"},
        "tools": [{"name":"read", "description":"Read report", "input_schema":{"type":"object",
            "properties":{"path":{"type":"string"},"byte_offset":{"type":"integer","minimum":0}},
            "required":["path"],"additionalProperties":false}},
            {"name":"search", "description":"Search", "input_schema":{"type":"object"}}] })).unwrap()
}
fn config(server: &MockServer) -> AnthropicConfig {
    AnthropicConfig {
        base_url: server.uri(),
        ..Default::default()
    }
}
fn token_response() -> Value {
    json!({"access_token":"rotated-access", "refresh_token":"rotated-refresh", "expires_in":3600,
    "account":{"uuid":"account-a", "email_address":"a@example.test"}, "organization":{"uuid":"new-org"} })
}
fn send_response() -> Value {
    json!({"role":"assistant", "content":[{"type":"text", "text":"ok"}],
    "stop_reason":"end_turn", "usage":{"input_tokens":10,"output_tokens":1} })
}

#[tokio::test]
async fn claude_pkce_exchange_and_refresh_preserve_account_workspace() {
    let server = MockServer::start().await;
    let client = oauth(&server);
    let login = client.begin().unwrap();
    let url = reqwest::Url::parse(&login.authorize_url).unwrap();
    let params: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
    assert_eq!(
        params["code_challenge"],
        providers::provider::codex::oauth::PkcePair::challenge_for(login.code_verifier())
    );
    assert_eq!(params["scope"], CLAUDE_SCOPE);
    assert_eq!(params["code_challenge_method"], "S256");
    let state = login.state.clone();
    Mock::given(method("POST"))
        .and(path("/token"))
        .and(body_partial_json(json!({
        "grant_type":"authorization_code", "client_id":CLAUDE_CLIENT_ID, "code":"manual-code",
        "state":state, "code_verifier":login.code_verifier() })))
        .respond_with(ResponseTemplate::new(200).set_body_json(token_response()))
        .expect(1)
        .mount(&server)
        .await;
    let first = client
        .complete_at(login, &format!("manual-code#{state}"), 1000)
        .await
        .unwrap();
    assert_eq!(first.expires_at, 4600);
    assert_eq!(first.account_id.as_deref(), Some("account-a"));
    Mock::given(method("POST"))
        .and(path("/token"))
        .and(header("anthropic-beta", "oauth-2025-04-20"))
        .and(body_partial_json(
            json!({"grant_type":"refresh_token", "refresh_token":"refresh-secret"}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(token_response()))
        .expect(1)
        .mount(&server)
        .await;
    let rotated = client.refresh_at(&bundle(0), 2000).await.unwrap();
    assert_eq!(rotated.expires_at, 5600);
    assert_eq!(rotated.org_id.as_deref(), Some("original-org"));
    let encoded = serde_json::to_string(&rotated).unwrap();
    assert_eq!(
        serde_json::from_str::<ClaudeTokenBundle>(&encoded).unwrap(),
        rotated
    );
    let debug = format!("{rotated:?}");
    assert!(
        !debug.contains("rotated-access")
            && !debug.contains("rotated-refresh")
            && !debug.contains("original-org")
    );
}

#[tokio::test]
async fn claude_rejects_callback_state_and_sanitizes_auth_errors() {
    let server = MockServer::start().await;
    let client = oauth(&server);
    assert_eq!(
        client
            .complete_at(client.begin().unwrap(), "code#wrong-state", 1000)
            .await
            .unwrap_err(),
        ClaudeOAuthError::Callback
    );
    assert!(server.received_requests().await.unwrap().is_empty());
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(401).set_body_string("access-secret refresh-secret"))
        .expect(1)
        .mount(&server)
        .await;
    let error = client.refresh_at(&bundle(0), 1000).await.unwrap_err();
    assert_eq!(error, ClaudeOAuthError::ReauthenticationRequired);
    assert!(!format!("{error:?} {error}").contains("secret"));
}

#[tokio::test]
async fn claude_shared_store_refreshes_once_across_independent_clients() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(token_response()))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/messages"))
        .and(header("authorization", "Bearer rotated-access"))
        .respond_with(ResponseTemplate::new(200).set_body_json(send_response()))
        .expect(2)
        .mount(&server)
        .await;
    let store = Arc::new(InMemoryClaudeTokenStore::new());
    store.save(&bundle(0)).unwrap();
    let oauth_config = ClaudeOAuthConfig {
        token_url: format!("{}/token", server.uri()),
        ..Default::default()
    };
    let first = ClaudeClient::new(config(&server), store.clone())
        .unwrap()
        .with_oauth_config(oauth_config.clone())
        .unwrap();
    let second = ClaudeClient::new(config(&server), store.clone())
        .unwrap()
        .with_oauth_config(oauth_config)
        .unwrap();
    let input = request();
    let auth = ProviderAuth::new("unused");
    let (a, b) = tokio::join!(first.send(&auth, &input), second.send(&auth, &input));
    a.unwrap();
    b.unwrap();
    assert_eq!(
        store.load().unwrap().unwrap().refresh_token,
        "rotated-refresh"
    );
}

/// Synthetic cached tokens derive from captured input. No response fixture can fake a warm prefix.
#[derive(Default)]
struct PromptCache {
    previous: Mutex<Option<Value>>,
}
impl Respond for PromptCache {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let mut body = claude_model_input(&request.body).unwrap();
        let streaming = body["stream"] == true;
        body.as_object_mut().unwrap().remove("stream");
        for message in body["messages"].as_array_mut().unwrap() {
            for block in message["content"].as_array_mut().unwrap() {
                block.as_object_mut().unwrap().remove("cache_control");
            }
        }
        let mut previous = self.previous.lock().unwrap();
        let cached = previous
            .as_ref()
            .filter(|old| {
                let mut old_settings = (*old).clone();
                old_settings.as_object_mut().unwrap().remove("messages");
                let mut new_settings = body.clone();
                new_settings.as_object_mut().unwrap().remove("messages");
                old_settings == new_settings
                    && old["messages"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .zip(body["messages"].as_array().unwrap())
                        .all(|(a, b)| a == b)
                    && body["messages"].as_array().unwrap().len()
                        >= old["messages"].as_array().unwrap().len()
            })
            .map_or(0, |old| serde_json::to_vec(old).unwrap().len());
        *previous = Some(body);
        let mut response = send_response();
        response["usage"]["cache_read_input_tokens"] = json!(cached);
        if streaming {
            let start = json!({"type":"message_start", "message":response});
            let sse = format!(
                "event: message_start\ndata: {start}\n\nevent: content_block_start\ndata: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{{\"type\":\"text\",\"text\":\"\"}}}}\n\nevent: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":\"ok\"}}}}\n\nevent: message_delta\ndata: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"end_turn\"}},\"usage\":{{\"output_tokens\":1}}}}\n\nevent: message_stop\ndata: {{\"type\":\"message_stop\"}}\n\n"
            );
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse)
        } else {
            ResponseTemplate::new(200).set_body_json(response)
        }
    }
}

#[tokio::test]
async fn claude_cache_normal_turns_keep_identity_tools_and_sent_prefix_after_rotation() {
    let server = MockServer::start().await;
    let betas = format!("{CLAUDE_INFERENCE_BETAS},cache-diagnosis-2026-04-07");
    Mock::given(method("POST"))
        .and(path("/messages"))
        .and(query_param("beta", "true"))
        .and(header("accept", "application/json"))
        .and(headers(
            "anthropic-beta",
            betas.split(',').collect::<Vec<_>>(),
        ))
        .respond_with(PromptCache::default())
        .expect(3)
        .mount(&server)
        .await;
    let store = Arc::new(InMemoryClaudeTokenStore::new());
    store.save(&bundle(u64::MAX)).unwrap();
    let client = ClaudeClient::new(config(&server), store.clone())
        .unwrap()
        .with_profile("claude-profile");
    let mut input = request();
    let auth = ProviderAuth::new("unused");
    assert_eq!(
        client
            .send(&auth, &input)
            .await
            .unwrap()
            .usage
            .cache_read_tokens,
        0
    );
    input.messages.extend(serde_json::from_value::<Vec<providers::Message>>(json!([
        {"role":"assistant", "content":[{"type":"tool_use", "id":"call", "name":"read", "input":{"path":"report"}}]},
        {"role":"user", "content":[{"type":"tool_result", "tool_call_id":"call", "is_error":false,
            "content":[{"type":"text", "text":"Immutable report output 東京 cch=00000 \"model\":\"sentinel\",\"max_tokens\":123 ".repeat(1000)}]}]}])).unwrap());
    input.tools.reverse();
    let mut rotated = bundle(u64::MAX);
    rotated.access_token = "new-access".into();
    store.save(&rotated).unwrap();
    let events: Vec<_> = client.stream(&auth, &input).await.unwrap().collect().await;
    let events = events.into_iter().collect::<Result<Vec<_>, _>>().unwrap();
    assert!(
        matches!(events.last(), Some(StreamEvent::Completed { response }) if response.usage.cache_read_tokens > 0)
    );
    input.messages.push(
        serde_json::from_value(
            json!({"role":"user", "content":[{"type":"text", "text":"Continue"}]}),
        )
        .unwrap(),
    );
    let client = ClaudeClient::new(config(&server), store.clone())
        .unwrap()
        .with_profile("claude-profile");
    assert!(
        client
            .send(&auth, &input)
            .await
            .unwrap()
            .usage
            .cache_read_tokens
            > 0
    );
    let requests = server.received_requests().await.unwrap();
    let bodies: Vec<Value> = requests
        .iter()
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect();
    let model_inputs: Vec<_> = requests
        .iter()
        .map(|request| claude_model_input(&request.body).unwrap())
        .collect();
    for pair in model_inputs.windows(2) {
        assert_append_only(CacheProtocol::Anthropic, &pair[0], &pair[1]).unwrap();
    }
    assert_ne!(bodies[0]["system"][0], bodies[1]["system"][0]);
    for (body, captured) in bodies.iter().zip(&requests) {
        assert_eq!(body["system"][1]["text"], CLAUDE_SYSTEM_IDENTITY);
        assert_eq!(body["system"][2]["text"], "stable instructions".repeat(100));
        for index in [1, 2] {
            assert_eq!(
                body["system"][index]["cache_control"],
                json!({"type":"ephemeral","ttl":"1h"})
            );
        }
        let identity: Value =
            serde_json::from_str(body["metadata"]["user_id"].as_str().unwrap()).unwrap();
        assert_eq!(
            captured.headers["x-claude-code-session-id"]
                .to_str()
                .unwrap(),
            identity["session_id"]
        );
        for (name, value) in [
            ("user-agent", CLAUDE_USER_AGENT),
            ("x-stainless-lang", "js"),
            ("x-stainless-package-version", "0.128.0"),
            ("x-stainless-retry-count", "0"),
            ("x-stainless-runtime", "node"),
            ("x-stainless-runtime-version", "v26.3.0"),
            ("x-stainless-timeout", "600"),
            ("x-app", "cli"),
            ("anthropic-dangerous-direct-browser-access", "true"),
        ] {
            assert_eq!(captured.headers[name], value);
        }
        assert!(captured.headers.contains_key("x-stainless-arch"));
        assert!(captured.headers.contains_key("x-stainless-os"));
        assert_eq!(
            body["tools"].as_array().unwrap().last().unwrap()["cache_control"],
            json!({"type":"ephemeral","ttl":"1h"})
        );
        assert_eq!(
            body["messages"].as_array().unwrap().last().unwrap()["content"]
                .as_array()
                .unwrap()
                .last()
                .unwrap()["cache_control"],
            json!({"type":"ephemeral","ttl":"1h"})
        );
    }
    let identity: Value =
        serde_json::from_str(bodies[0]["metadata"]["user_id"].as_str().unwrap()).unwrap();
    assert_eq!(identity["device_id"].as_str().unwrap().len(), 64);
    assert!(uuid::Uuid::parse_str(identity["session_id"].as_str().unwrap()).is_ok());
    assert_eq!(identity["account_uuid"], "account-secret");
    assert!(
        bodies
            .iter()
            .all(|body| !body.to_string().contains("access-secret")
                && !body.to_string().contains("new-access"))
    );
    assert_eq!(bodies[0]["tools"][0]["name"], "_read");
    assert_eq!(
        bodies[0]["tools"][0]["input_schema"]["required"],
        json!(["path"])
    );
    assert_eq!(bodies[0]["tools"][1]["name"], "_mcp__evorch__search");
    assert_eq!(bodies[1]["messages"][1]["content"][0]["name"], "_read");
    assert_eq!(
        bodies[1]["messages"][1]["content"][0]["input"],
        json!({"path":"report"})
    );
    assert_eq!(
        bodies[1]["messages"][2]["content"][0]["content"],
        bodies[2]["messages"][2]["content"][0]["content"]
    );
    assert!(!requests[0].headers.contains_key("x-api-key"));
    assert_eq!(requests[0].headers["authorization"], "Bearer access-secret");
    assert_eq!(requests[1].headers["authorization"], "Bearer new-access");
}

#[tokio::test]
async fn claude_stream_restores_tool_names_and_rejects_unsupported_reasoning_controls() {
    let server = MockServer::start().await;
    let sse = include_str!("fixtures/anthropic/stream_tool_use.sse")
        .replace("get_weather", "_mcp__evorch__get_weather");
    Mock::given(method("POST"))
        .and(path("/messages"))
        .and(header("authorization", "Bearer access-secret"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse),
        )
        .expect(1)
        .mount(&server)
        .await;
    let store = Arc::new(InMemoryClaudeTokenStore::new());
    store.save(&bundle(u64::MAX)).unwrap();
    let client = ClaudeClient::new(config(&server), store).unwrap();
    assert!(client.capabilities().reasoning);
    let auth = ProviderAuth::new("unused");
    let mut input = request();
    input.tools.push(
        serde_json::from_value(json!({
            "name":"get_weather", "description":"Weather", "input_schema":{"type":"object"}
        }))
        .unwrap(),
    );
    let events: Vec<_> = client.stream(&auth, &input).await.unwrap().collect().await;
    let events: Vec<_> = events.into_iter().collect::<Result<_, _>>().unwrap();
    assert!(events.iter().any(
        |e| matches!(e, StreamEvent::ToolCallDelta { name:Some(name), .. } if name == "get_weather")
    ));
    assert!(
        matches!(events.last(), Some(StreamEvent::Completed { response }) if matches!(&response.message.content[0], providers::ContentBlock::ToolUse { name, .. } if name == "get_weather"))
    );
    let mut input = request();
    input.reasoning_effort = Some("medium".into());
    assert!(matches!(
        client.send(&auth, &input).await,
        Err(providers::ProviderError::Request(_))
    ));
}

#[tokio::test]
async fn subscription_adaptive_reasoning_is_model_validated_and_api_requests_stay_unchanged() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(send_response()))
        .expect(2)
        .mount(&server)
        .await;
    let mut input = request();
    input.model = "claude-sonnet-4-6".into();
    input.reasoning_effort = Some("high".into());
    let auth = ProviderAuth::new("fixture-token");
    let oauth = AnthropicClient::new(config(&server))
        .unwrap()
        .with_claude_oauth()
        .unwrap();
    let api = AnthropicClient::new(config(&server)).unwrap();
    oauth.send(&auth, &input).await.unwrap();
    api.send(&auth, &input).await.unwrap();
    let captured = server.received_requests().await.unwrap();
    let subscription = claude_model_input(&captured[0].body).unwrap();
    assert_eq!(
        subscription["thinking"],
        json!({"type":"adaptive","display":"updates"})
    );
    assert_eq!(subscription["output_config"], json!({"effort":"high"}));
    assert_eq!(
        subscription["context_management"],
        json!({"edits":[{"type":"clear_thinking_20251015","keep":"all"}]})
    );
    assert!(
        captured[0].headers["anthropic-beta"]
            .to_str()
            .unwrap()
            .split(',')
            .any(|beta| beta == "effort-2025-11-24")
    );
    let api_body: Value = serde_json::from_slice(&captured[1].body).unwrap();
    for control in ["thinking", "output_config", "context_management"] {
        assert!(api_body.get(control).is_none());
    }
    assert!(
        !captured[1]
            .headers
            .contains_key("x-stainless-package-version")
    );
}

#[tokio::test]
async fn subscription_tool_surface_roundtrips_without_changing_api_key_requests() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/messages"))
        .respond_with(|request: &Request| {
            let oauth = request.headers.contains_key("authorization");
            ResponseTemplate::new(200).set_body_json(json!({
                "role":"assistant", "stop_reason":"tool_use",
                "content":[
                    {"type":"tool_use","id":"read-call", "name":if oauth {"_read"} else {"read"},
                        "input":json!({"path":"report","byte_offset":7})},
                    {"type":"tool_use","id":"write-call", "name":if oauth {"_write"} else {"write"},
                        "input":json!({"path":"output","content":"file_path stays in text"})},
                    {"type":"tool_use","id":"custom-call", "name":if oauth {"_mcp__evorch__inspect_run"} else {"inspect_run"},
                        "input":{"path":"canonical-custom-path","nested":{"file_path":"unchanged"}}}
                ], "usage":{"input_tokens":10,"output_tokens":5}
            }))
        })
        .expect(2)
        .mount(&server)
        .await;
    let mut input = request();
    input.tools.extend(serde_json::from_value::<Vec<providers::ToolSpec>>(json!([
        {"name":"write","description":"Write content","input_schema":{"type":"object","properties":{"path":{"type":"string"},"content":{"type":"string"}},"required":["path","content"],"additionalProperties":false}},
        {"name":"inspect_run","description":"Inspect run","input_schema":{"type":"object","properties":{"path":{"type":"string"},"nested":{"type":"object","properties":{"file_path":{"type":"string"}}}}}},
        {"name":"Read","description":"A custom tool named Read","input_schema":{"type":"object"}},
        {"name":"mcp__evorch__inspect_run","description":"An existing MCP-looking tool","input_schema":{"type":"object"}}
    ])).unwrap());
    let original = serde_json::to_value(&input).unwrap();
    let auth = ProviderAuth::new("fixture-key");
    let oauth = AnthropicClient::new(config(&server))
        .unwrap()
        .with_claude_oauth()
        .unwrap();
    let api = AnthropicClient::new(config(&server)).unwrap();
    let oauth_response = oauth.send(&auth, &input).await.unwrap();
    let api_response = api.send(&auth, &input).await.unwrap();
    assert_eq!(oauth_response.message, api_response.message);
    assert_eq!(serde_json::to_value(&input).unwrap(), original);
    let captured = server.received_requests().await.unwrap();
    let oauth_body = claude_model_input(&captured[0].body).unwrap();
    let api_body: Value = serde_json::from_slice(&captured[1].body).unwrap();
    for tool in &input.tools {
        let api_tool = api_body["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|candidate| candidate["name"] == tool.name)
            .unwrap();
        assert_eq!(api_tool["input_schema"], tool.input_schema);
        assert_eq!(api_tool["description"], tool.description);
    }
    for (canonical, wire) in [
        ("read", "_read"),
        ("write", "_write"),
        ("search", "_mcp__evorch__search"),
        ("inspect_run", "_mcp__evorch__inspect_run"),
        ("Read", "_mcp__evorch__Read"),
        (
            "mcp__evorch__inspect_run",
            "_mcp__evorch__mcp__evorch__inspect_run",
        ),
    ] {
        let tool = oauth_body["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|tool| tool["name"] == wire)
            .unwrap();
        let source = input
            .tools
            .iter()
            .find(|tool| tool.name == canonical)
            .unwrap();
        assert_eq!(tool["input_schema"], source.input_schema);
    }
    assert_eq!(oauth_body["messages"], api_body["messages"]);
    assert_eq!(
        api_body["system"][0]["text"],
        "stable instructions".repeat(100)
    );
    assert!(api_body.get("metadata").is_none());
    assert!(!captured[1].headers.contains_key("authorization"));
    assert_eq!(captured[1].headers["x-api-key"], "fixture-key");
}

#[tokio::test]
async fn subscription_stream_deltas_and_completion_agree_on_canonical_arguments() {
    let server = MockServer::start().await;
    let events = [
        json!({"type":"message_start","message":{"role":"assistant","content":[],"usage":{"input_tokens":10,"output_tokens":0}}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"read-call","name":"_read","input":{}}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"pa"}}),
        json!({"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"write-call","name":"_write","input":{"path":"output","content":"file_path stays in text 東京"}}}),
        json!({"type":"content_block_stop","index":1}),
        json!({"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"custom-call","name":"_mcp__evorch__inspect_run","input":{}}}),
        json!({"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"path\":\"custom-path\","}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"th\":\"report\",\"byte_offset\":0}"}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"\"nested\":{\"file_path\":\"unchanged\"}}"}}),
        json!({"type":"content_block_stop","index":2}),
        json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":5}}),
        json!({"type":"message_stop"}),
    ];
    let sse = events
        .into_iter()
        .map(|event| {
            format!(
                "event: {}\ndata: {event}\n\n",
                event["type"].as_str().unwrap()
            )
        })
        .collect::<String>();
    Mock::given(method("POST"))
        .and(path("/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse),
        )
        .expect(1)
        .mount(&server)
        .await;
    let mut input = request();
    input.tools.extend(
        serde_json::from_value::<Vec<providers::ToolSpec>>(json!([
            {"name":"write","description":"Write","input_schema":{"type":"object"}},
            {"name":"inspect_run","description":"Inspect","input_schema":{"type":"object"}}
        ]))
        .unwrap(),
    );
    let client = AnthropicClient::new(config(&server))
        .unwrap()
        .with_claude_oauth()
        .unwrap();
    let events = client
        .stream(&ProviderAuth::new("fixture-token"), &input)
        .await
        .unwrap()
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let mut accumulator = providers::StreamAccumulator::default();
    for event in &events {
        accumulator.feed(event);
    }
    assert!(accumulator.malformed_tool_calls().is_empty());
    let Some(StreamEvent::Completed { response }) = events.last() else {
        panic!("missing completion");
    };
    let deltas = accumulator.finish(response.usage, response.finish_reason.clone());
    let calls = |message: &providers::Message| {
        message
            .content
            .iter()
            .filter_map(|block| {
                if let providers::ContentBlock::ToolUse { id, name, input } = block {
                    Some((id.clone(), name.clone(), input.clone()))
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
    };
    let expected = vec![
        (
            "read-call".into(),
            "read".into(),
            json!({"path":"report","byte_offset":0}),
        ),
        (
            "write-call".into(),
            "write".into(),
            json!({"path":"output","content":"file_path stays in text 東京"}),
        ),
        (
            "custom-call".into(),
            "inspect_run".into(),
            json!({"path":"custom-path","nested":{"file_path":"unchanged"}}),
        ),
    ];
    assert_eq!(calls(&deltas.message), expected);
    assert_eq!(calls(&response.message), expected);
    let captured = server.received_requests().await.unwrap();
    claude_model_input(&captured[0].body).unwrap();
}

#[tokio::test]
async fn claude_quota_includes_shared_legacy_and_inactive_scoped_windows() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/proxy/api/oauth/usage"))
        .and(header("authorization", "Bearer access-secret"))
        .and(header("anthropic-beta", "oauth-2025-04-20"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "five_hour":{"utilization":12.5,"resets_at":"2026-10-09T00:00:00Z"},
            "seven_day":{"utilization":25}, "seven_day_opus":{"utilization":35},
            "limits":[{"kind":"weekly_scoped", "percent":"80", "is_active":false,
                "scope":{"model":{"display_name":"Sonnet"}}, "resets_at":"2026-10-12T00:00:00Z"},
                {"kind":"weekly_all", "percent":50}],
            "extra_usage":{"is_enabled":true, "used_credits":2500, "monthly_limit":10000} })))
        .expect(1)
        .mount(&server)
        .await;
    let client = ClaudeQuotaClient::new(
        &format!("{}/proxy/v1", server.uri()),
        Duration::from_secs(30),
    )
    .unwrap();
    let quota = client.fetch_quota(&bundle(u64::MAX)).await.unwrap();
    assert_eq!(quota.windows.len(), 4);
    assert_eq!(quota.windows[0].remaining_percent, 87.5);
    assert_eq!(quota.windows[1].label, "7d");
    assert_eq!(quota.windows[1].used_percent, 25.0);
    assert_eq!(
        quota.extra_usage,
        Some(ClaudeExtraUsage {
            used_usd: 25.0,
            limit_usd: Some(100.0)
        })
    );
    assert_eq!(quota.windows[3].label, "Sonnet 7d");
    assert_eq!(quota.windows[3].used_percent, 80.0);
    assert!(quota.windows[3].resets_at.is_some());
    server.reset().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(403).set_body_string("access-secret"))
        .mount(&server)
        .await;
    assert!(
        !client
            .fetch_quota(&bundle(0))
            .await
            .unwrap_err()
            .to_string()
            .contains("access-secret")
    );
    let store = InMemoryClaudeTokenStore::new();
    store.save(&bundle(0)).unwrap();
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(401).set_body_string("refresh-secret"))
        .expect(1)
        .mount(&server)
        .await;
    assert!(matches!(
        client
            .fetch_quota_for_store(&store, &oauth(&server))
            .await
            .unwrap_err(),
        providers::provider::codex::quota::QuotaError::ReauthenticationRequired
    ));
}

#[tokio::test]
async fn claude_native_model_discovery_uses_key_or_oauth_and_pagination() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .and(header("x-api-key", "api-secret"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"data":[{"id":"claude-key"},{"id":"claude-haiku-4-5-20251001"},{"id":"claude-unrecognized-4-5-20251001"}]})),
        )
        .expect(1)
        .mount(&server)
        .await;
    let client = AnthropicClient::new(config(&server)).unwrap();
    assert_eq!(
        client
            .list_models(&ProviderAuth::new("api-secret"))
            .await
            .unwrap()
            .unwrap(),
        [
            "claude-haiku-4-5",
            "claude-haiku-4-5-20251001",
            "claude-key",
            "claude-unrecognized-4-5-20251001"
        ]
    );
    server.reset().await;
    Mock::given(method("GET"))
        .and(header("authorization", "Bearer access-secret"))
        .and(headers(
            "anthropic-beta",
            CLAUDE_INFERENCE_BETAS.split(',').collect::<Vec<_>>(),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"data":[{"id":"claude-a"}], "has_more":true,"last_id":"claude-a"}),
        ))
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(query_param("after_id", "claude-a"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data":[{"id":"claude-b"}]})))
        .expect(1)
        .mount(&server)
        .await;
    let store = Arc::new(InMemoryClaudeTokenStore::new());
    store.save(&bundle(u64::MAX)).unwrap();
    let client = ClaudeClient::new(config(&server), store).unwrap();
    assert_eq!(
        client
            .list_models(&ProviderAuth::new("unused"))
            .await
            .unwrap()
            .unwrap(),
        ["claude-a", "claude-b"]
    );
}

#[tokio::test]
async fn api_and_oauth_replay_other_provider_reasoning_without_unsigned_thinking() {
    for oauth in [false, true] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(send_response()))
            .expect(2)
            .mount(&server)
            .await;
        let mut client = AnthropicClient::new(config(&server)).unwrap();
        if oauth {
            client = client.with_claude_oauth().unwrap();
        }
        let mut history = request();
        history.messages.extend(serde_json::from_value::<Vec<providers::Message>>(json!([
            {"role":"assistant","content":[{"type":"reasoning","text":"foreign private reasoning"}]},
            {"role":"user","content":[{"type":"text","text":"continue"}]},
            {"role":"assistant","content":[{"type":"reasoning","text":"foreign mixed reasoning"},{"type":"text","text":"answer"}]},
            {"role":"user","content":[{"type":"text","text":"follow up"}]}
        ])).unwrap());
        client
            .send(&ProviderAuth::new("test-secret"), &history)
            .await
            .unwrap();
        history.messages.extend(
            serde_json::from_value::<Vec<providers::Message>>(json!([
                {"role":"assistant","content":[{"type":"text","text":"ok"}]},
                {"role":"user","content":[{"type":"text","text":"next turn"}]}
            ]))
            .unwrap(),
        );
        client
            .send(&ProviderAuth::new("test-secret"), &history)
            .await
            .unwrap();
        let requests = server.received_requests().await.unwrap();
        let bodies: Vec<Value> = requests
            .iter()
            .map(|request| serde_json::from_slice(&request.body).unwrap())
            .collect();
        for body in &bodies {
            assert!(!body.to_string().contains("foreign private reasoning"));
            assert!(!body.to_string().contains("foreign mixed reasoning"));
            assert_eq!(
                body["messages"][1]["content"][0]["text"],
                "[Reasoning omitted from replay]"
            );
            assert_eq!(body["messages"][3]["content"][0]["text"], "answer");
            assert!(body["messages"].as_array().unwrap().iter().all(|message| {
                message["content"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|block| block["type"] != "thinking")
            }));
        }
        let model_inputs: Vec<Value> = requests
            .iter()
            .map(|request| {
                if oauth {
                    claude_model_input(&request.body).unwrap()
                } else {
                    serde_json::from_slice(&request.body).unwrap()
                }
            })
            .collect();
        assert_append_only(CacheProtocol::Anthropic, &model_inputs[0], &model_inputs[1]).unwrap();
    }
}

#[tokio::test]
async fn claude_api_credentials_are_not_forwarded_to_redirect_destinations() {
    let server = MockServer::start().await;
    let destination = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(307).insert_header("Location", destination.uri()))
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(307).insert_header("Location", destination.uri()))
        .expect(1)
        .mount(&server)
        .await;
    let client = AnthropicClient::new(config(&server)).unwrap();
    let auth = ProviderAuth::new("credential-sentinel");
    assert!(matches!(
        client.send(&auth, &request()).await,
        Err(providers::ProviderError::Http { status: 307, .. })
    ));
    assert!(matches!(
        client.stream(&auth, &request()).await,
        Err(providers::ProviderError::Http { status: 307, .. })
    ));
    assert!(matches!(
        client.list_models(&auth).await,
        Err(providers::ProviderError::Http { status: 307, .. })
    ));
    assert!(destination.received_requests().await.unwrap().is_empty());
}
