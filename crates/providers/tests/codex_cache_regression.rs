use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use event_bus::{DiagnosticEvent, DiagnosticSeverity, EventBus, EventKind, ProviderEvent};
use futures_util::StreamExt;
use providers::provider::codex::tokens::{CodexTokenStore, InMemoryTokenStore, TokenBundle};
use providers::provider::codex::{CodexClient, CodexConfig};
use providers::{
    ChatRequest, ContentBlock, Message, ProviderAuth, ProviderClient, Role, StreamEvent,
    ToolResultContent,
};
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn request() -> ChatRequest {
    serde_json::from_value(json!({
        "model": "gpt-6-astra",
        "messages": [
            {"role": "system", "content": [{"type": "text", "text": "a".repeat(32_768)}]},
            {"role": "user", "content": [{"type": "text", "text": "Inspect the output."}]},
            {"role": "assistant", "content": [{"type": "text", "text": "I will inspect it."}]},
            {"role": "user", "content": [{"type": "text", "text": "Continue."}]}
        ],
        "observation": {"run_id": "codex-cache-contract"}
    }))
    .unwrap()
}

struct Scenario {
    server: MockServer,
    bus: Arc<EventBus>,
    client: CodexClient,
}

impl Scenario {
    async fn new() -> Self {
        let server = MockServer::start().await;
        let bus = Arc::new(EventBus::new(32));
        let store = Arc::new(InMemoryTokenStore::new());
        let payload = json!({
            "exp": u64::MAX,
            "https://api.openai.com/auth": {"chatgpt_account_id": "cache-contract"}
        });
        store
            .save(&TokenBundle {
                access_token: "cache-test-token".into(),
                refresh_token: "cache-test-refresh".into(),
                id_token: format!("e30.{}.sig", URL_SAFE_NO_PAD.encode(payload.to_string())),
            })
            .unwrap();
        let client = CodexClient::with_config(
            CodexConfig {
                base_url: server.uri(),
                auth_base_url: server.uri(),
                timeout: Duration::from_secs(2),
                event_bus: Some(bus.clone()),
                client_version: providers::CodexClientVersion::Fixed(
                    providers::CodexCatalogVersion {
                        version: providers::CODEX_MODELS_FALLBACK_VERSION.into(),
                        warning: None,
                    },
                ),
            },
            store,
        )
        .unwrap();
        Self {
            server,
            bus,
            client,
        }
    }

    async fn complete(
        &self,
        request: &ChatRequest,
        input_tokens: u64,
        cached_tokens: u64,
    ) -> (Value, Vec<DiagnosticEvent>) {
        self.server.reset().await;
        let completed = json!({
            "type": "response.completed",
            "response": {
                "id": "cache-response", "status": "completed",
                "usage": {
                    "input_tokens": input_tokens,
                    "output_tokens": 1,
                    "total_tokens": input_tokens + 1,
                    "input_tokens_details": {"cached_tokens": cached_tokens},
                    "output_tokens_details": {}
                }
            }
        });
        Mock::given(method("POST"))
            .and(path("/backend-api/codex/responses"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                format!("event: response.completed\ndata: {completed}\n\ndata: [DONE]\n\n"),
                "text/event-stream",
            ))
            .expect(1)
            .mount(&self.server)
            .await;
        let mut receiver = self.bus.subscribe();
        let events = self
            .client
            .stream(&ProviderAuth::new(""), request)
            .await
            .unwrap()
            .collect::<Vec<_>>()
            .await;
        let StreamEvent::Completed { response } = events.last().unwrap().as_ref().unwrap() else {
            panic!("the SSE stream must complete successfully")
        };
        assert_eq!(response.usage.input_tokens, input_tokens);
        assert_eq!(response.usage.cache_read_tokens, cached_tokens);
        let mut diagnostics = Vec::new();
        loop {
            let event = tokio::time::timeout(Duration::from_secs(1), receiver.recv())
                .await
                .unwrap()
                .unwrap();
            match event.kind {
                EventKind::Diagnostic(diagnostic) => diagnostics.push(diagnostic),
                EventKind::Provider(ProviderEvent::RequestCompleted { .. }) => break,
                _ => {}
            }
        }
        let requests = self.server.received_requests().await.unwrap();
        let body = serde_json::from_slice(&requests[0].body).unwrap();
        (body, diagnostics)
    }
}

fn detail_number(diagnostic: &DiagnosticEvent, field: &str) -> f64 {
    diagnostic
        .detail
        .split_whitespace()
        .find_map(|part| part.split_once('=').filter(|(key, _)| *key == field))
        .unwrap_or_else(|| panic!("missing {field} in {}", diagnostic.detail))
        .1
        .parse()
        .unwrap()
}

#[tokio::test]
async fn codex_warning_separates_observed_hit_ratio_from_cache_retention() {
    let scenario = Scenario::new().await;
    let request = request();
    let (previous, diagnostics) = scenario.complete(&request, 9_000, 8_192).await;
    assert!(diagnostics.is_empty(), "the first request has no baseline");
    let (current, diagnostics) = scenario.complete(&request, 11_702, 1_792).await;
    assert_eq!(previous, current);
    assert_eq!(diagnostics.len(), 1);
    let warning = &diagnostics[0];
    assert_eq!(warning.code, "CacheRegression");
    assert_eq!(warning.severity, DiagnosticSeverity::Warning);
    assert_eq!(warning.run_id.as_deref(), Some("codex-cache-contract"));
    assert_eq!(detail_number(warning, "previous_cache_tokens"), 8_192.0);
    assert_eq!(detail_number(warning, "cache_read_tokens"), 1_792.0);
    assert_eq!(
        detail_number(warning, "cache_hit_ratio"),
        1_792.0 / 11_702.0
    );
    assert_eq!(
        detail_number(warning, "cache_retention_ratio"),
        1_792.0 / 8_192.0
    );
    assert!(!warning.detail.contains("expected_cacheable_tokens"));
}

#[tokio::test]
async fn codex_omitted_reasoning_cannot_inflate_the_cache_denominator() {
    let scenario = Scenario::new().await;
    let mut request = request();
    let (previous, _) = scenario.complete(&request, 9_000, 8_192).await;
    request.messages[2].content.insert(
        0,
        ContentBlock::Reasoning {
            text: "private reasoning ".repeat(8_000),
        },
    );
    let (current, diagnostics) = scenario.complete(&request, 9_000, 8_192).await;
    assert_eq!(
        previous, current,
        "canonical reasoning is omitted on the wire"
    );
    assert!(diagnostics.is_empty(), "cached usage is unchanged");

    // Ignored reasoning must not invalidate a real regression's baseline either.
    request.messages[2].content[0] = ContentBlock::Reasoning {
        text: "different private reasoning ".repeat(10_000),
    };
    let (regressed, diagnostics) = scenario.complete(&request, 11_702, 1_792).await;
    assert_eq!(current, regressed);
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(
        detail_number(&diagnostics[0], "previous_cache_tokens"),
        8_192.0
    );
    assert_eq!(
        detail_number(&diagnostics[0], "cache_hit_ratio"),
        1_792.0 / 11_702.0
    );
}

#[tokio::test]
async fn replacing_old_tool_output_invalidates_the_previous_cache_baseline() {
    let scenario = Scenario::new().await;
    let mut request = request();
    request.messages.insert(
        3,
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "call-1".into(),
                name: "read_file".into(),
                input: json!({"path": "output.txt"}),
            }],
        },
    );
    request.messages.insert(
        4,
        Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_call_id: "call-1".into(),
                content: vec![ToolResultContent::Text {
                    text: "original output\n".repeat(1_000),
                }],
                is_error: false,
            }],
        },
    );
    let (previous, _) = scenario.complete(&request, 14_000, 8_192).await;
    let ContentBlock::ToolResult { content, .. } = &mut request.messages[4].content[0] else {
        unreachable!()
    };
    *content = vec![ToolResultContent::Text {
        text: "Output saved to /tmp/tool-output.txt".into(),
    }];
    let (current, diagnostics) = scenario.complete(&request, 11_702, 1_792).await;
    assert_ne!(previous["input"], current["input"]);
    assert!(
        diagnostics.is_empty(),
        "a changed historical prefix cannot reuse the previous baseline"
    );
}

#[tokio::test]
async fn a_long_new_suffix_does_not_make_retained_cache_a_regression() {
    let scenario = Scenario::new().await;
    let mut request = request();
    let (previous, _) = scenario.complete(&request, 9_000, 8_192).await;
    request.messages.extend([
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Text {
                text: "new analysis ".repeat(12_000),
            }],
        },
        Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: "Continue with the new analysis.".into(),
            }],
        },
    ]);
    let (current, diagnostics) = scenario.complete(&request, 40_000, 8_192).await;
    assert!(
        current["input"]
            .as_array()
            .unwrap()
            .starts_with(previous["input"].as_array().unwrap())
    );
    assert!(
        diagnostics.is_empty(),
        "the input grew, but all previously cached tokens remain cached"
    );
}
