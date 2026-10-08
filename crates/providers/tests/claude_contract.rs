//! Claude's native OAuth wire and cost-sensitive prompt contracts.
use futures_util::StreamExt;
use mock_openai::cache_contract::{CacheProtocol, assert_append_only};
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
        "tools": [{"name":"read", "description":"Read report", "input_schema":{"type":"object"}},
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
        let mut body: Value = serde_json::from_slice(&request.body).unwrap();
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
    Mock::given(method("POST"))
        .and(path("/messages"))
        .and(query_param("beta", "true"))
        .and(header("accept", "application/json"))
        .and(headers(
            "anthropic-beta",
            CLAUDE_INFERENCE_BETAS.split(',').collect::<Vec<_>>(),
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
            "content":[{"type":"text", "text":"Immutable report output 東京".repeat(1000)}]}]}])).unwrap());
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
    for pair in bodies.windows(2) {
        assert_append_only(CacheProtocol::Anthropic, &pair[0], &pair[1]).unwrap();
    }
    assert_eq!(bodies[0]["system"][0]["text"], CLAUDE_SYSTEM_IDENTITY);
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
    assert_eq!(bodies[1]["messages"][1]["content"][0]["name"], "_read");
    assert!(!requests[0].headers.contains_key("x-api-key"));
    assert_eq!(requests[0].headers["authorization"], "Bearer access-secret");
    assert_eq!(requests[1].headers["authorization"], "Bearer new-access");
}

#[tokio::test]
async fn claude_stream_restores_tool_names_and_refuses_unsigned_thinking_controls() {
    let server = MockServer::start().await;
    let sse = include_str!("fixtures/anthropic/stream_tool_use.sse")
        .replace("get_weather", "_get_weather");
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
    assert!(!client.capabilities().reasoning);
    let auth = ProviderAuth::new("unused");
    let events: Vec<_> = client
        .stream(&auth, &request())
        .await
        .unwrap()
        .collect()
        .await;
    let events: Vec<_> = events.into_iter().collect::<Result<_, _>>().unwrap();
    assert!(events.iter().any(
        |e| matches!(e, StreamEvent::ToolCallDelta { name:Some(name), .. } if name == "get_weather")
    ));
    assert!(
        matches!(events.last(), Some(StreamEvent::Completed { response }) if matches!(&response.message.content[0], providers::ContentBlock::ToolUse { name, .. } if name == "get_weather"))
    );
    let mut input = request();
    input.reasoning_effort = Some("medium".into());
    assert!(
        client
            .send(&auth, &input)
            .await
            .unwrap_err()
            .to_string()
            .contains("signed thinking")
    );
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
            client = client.with_claude_oauth();
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
        assert_append_only(CacheProtocol::Anthropic, &bodies[0], &bodies[1]).unwrap();
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
