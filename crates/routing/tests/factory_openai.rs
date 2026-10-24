//! factory の OpenAI (Responses API) client 構築契約を検証します。

use std::sync::Arc;
use std::time::Duration;

use providers::{ChatRequest, ContentBlock, FinishReason, Message, ProviderAuth, Role, Usage};
use routing::factory::{FactoryOptions, build_provider_client};
use routing::{CredentialRef, ProviderProfile, RoutingError};
use sandbox::credential::{CredentialStore, FileCredentialStore};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const PROFILE: &str = "openai-main";
const MODEL: &str = "gpt-test";

/// Responses API 成功応答の最小 SSE 本文。
const SSE_SUCCESS: &str = r#"event: response.created
data: {"type":"response.created","response":{"id":"resp-1","status":"in_progress"}}

event: response.output_text.delta
data: {"type":"response.output_text.delta","item_id":"msg-1","output_index":0,"content_index":0,"delta":"Hello"}

event: response.output_text.delta
data: {"type":"response.output_text.delta","item_id":"msg-1","output_index":0,"content_index":0,"delta":" world"}

event: response.completed
data: {"type":"response.completed","response":{"id":"resp-1","status":"completed","usage":{"input_tokens":12,"output_tokens":2,"total_tokens":14}}}

"#;

fn profile(api_protocol: model::ApiProtocol, credential: CredentialRef) -> ProviderProfile {
    ProviderProfile {
        name: PROFILE.to_string(),
        provider_type: model::ProviderType::OpenAi,
        api_protocol,
        base_url: "https://api.openai.com/v1".to_string(),
        credential,
        models: vec![MODEL.to_string()],
        default_model: MODEL.to_string(),
    }
}

fn env_credential() -> CredentialRef {
    CredentialRef::Env {
        var: "OPENAI_API_KEY".to_string(),
    }
}

fn keyring_credential() -> CredentialRef {
    CredentialRef::Keyring {
        service: "evorch".to_string(),
        account: PROFILE.to_string(),
    }
}

fn credential_store() -> (tempfile::TempDir, Arc<dyn CredentialStore>) {
    let directory = tempfile::tempdir().expect("一時ディレクトリを作成できる");
    let store = FileCredentialStore::open(directory.path()).expect("資格情報ストアを開ける");
    (directory, Arc::new(store))
}

fn request() -> ChatRequest {
    ChatRequest {
        model: MODEL.to_string(),
        messages: vec![Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: "Hello".to_string(),
            }],
        }],
        tools: Vec::new(),
        temperature: None,
        max_tokens: Some(123),
        reasoning_effort: None,
        service_tier: None,
        output_schema: None,
        observation: None,
    }
}

/// `POST /v1/responses` へ Bearer 認証付きで届いた要求だけに SSE 成功応答を返す。
async fn mount_responses(server: &MockServer, api_key: &str) {
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .and(header(
            "authorization",
            format!("Bearer {api_key}").as_str(),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_raw(SSE_SUCCESS, "text/event-stream"))
        .expect(1)
        .mount(server)
        .await;
}

// Given: openai プロファイル (openai-responses) と API キー
// When: factory で構築した client から send する
// Then: /v1/responses へ Bearer 認証で Responses 形式の body を送り、SSE を canonical 応答へ集約し、
//       観測イベントに provider・profile・protocol を記録する
#[tokio::test]
async fn factory_builds_openai_responses_client() {
    let server = MockServer::start().await;
    mount_responses(&server, "test-api-key").await;
    let (_directory, store) = credential_store();
    let mut profile = profile(model::ApiProtocol::OpenAiResponses, env_credential());
    profile.base_url = format!("{}/v1", server.uri());
    let bus = Arc::new(event_bus::EventBus::new(16));
    let mut events = bus.subscribe();

    let client = build_provider_client(&profile, store, Some(bus), &FactoryOptions::default())
        .expect("OpenAI Responses client を構築できる");
    let response = client
        .send(&ProviderAuth::new("test-api-key"), &request())
        .await
        .expect("送信に成功する");

    assert_eq!(
        response.message.content,
        vec![ContentBlock::Text {
            text: "Hello world".to_string()
        }]
    );
    assert_eq!(
        response.usage,
        Usage {
            input_tokens: 12,
            output_tokens: 2,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            reasoning_tokens: None,
        }
    );
    assert_eq!(response.finish_reason, FinishReason::Stop);
    let received = server
        .received_requests()
        .await
        .expect("wiremock が要求を記録する");
    assert_eq!(received.len(), 1);
    let body: serde_json::Value = received[0].body_json().expect("要求 body は JSON");
    assert_eq!(body["model"], MODEL);
    assert!(
        body.get("input").is_some(),
        "Responses は input を送る: {body}"
    );
    assert!(
        body.get("messages").is_none(),
        "Chat Completions の messages は送らない: {body}"
    );
    assert_eq!(
        body["max_output_tokens"], 123,
        "出力上限は max_output_tokens として送る: {body}"
    );
    let started = loop {
        let event = tokio::time::timeout(Duration::from_secs(1), events.recv())
            .await
            .expect("provider 観測イベントが届く")
            .expect("event bus は開いている");
        if let event_bus::EventKind::Provider(event_bus::ProviderEvent::RequestStarted {
            provider,
            profile,
            protocol,
            ..
        }) = event.kind
        {
            break (provider, profile, protocol);
        }
    };
    assert_eq!(
        started,
        (
            "openai".to_string(),
            Some(PROFILE.to_string()),
            "openai-responses".to_string()
        )
    );
}

// Given: /v1 を含まない base URL、または末尾 / 付きの base URL
// When: factory で構築した client から send する
// Then: /v1 を補い、末尾 / を落として /v1/responses へ送る (/v1/v1 にはならない)
#[tokio::test]
async fn factory_normalizes_openai_base_url_to_v1_responses() {
    for suffix in ["", "/", "/v1", "/v1/"] {
        let server = MockServer::start().await;
        mount_responses(&server, "test-api-key").await;
        let (_directory, store) = credential_store();
        let mut profile = profile(model::ApiProtocol::OpenAiResponses, env_credential());
        profile.base_url = format!("{}{suffix}", server.uri());

        let client = build_provider_client(&profile, store, None, &FactoryOptions::default())
            .expect("OpenAI Responses client を構築できる");
        client
            .send(&ProviderAuth::new("test-api-key"), &request())
            .await
            .unwrap_or_else(|error| panic!("base_url suffix {suffix:?} で送信できる: {error}"));
    }
}

// Given: keyring 認証参照を持つ openai プロファイル
// When: factory で構築した client から send する
// Then: 構築でき、解決済み API キーを Bearer 認証として送る
#[tokio::test]
async fn factory_builds_openai_client_from_keyring_profile() {
    let server = MockServer::start().await;
    mount_responses(&server, "keyring-api-key").await;
    let (_directory, store) = credential_store();
    let mut profile = profile(model::ApiProtocol::OpenAiResponses, keyring_credential());
    profile.base_url = server.uri();

    let client = build_provider_client(&profile, store, None, &FactoryOptions::default())
        .expect("keyring 参照の OpenAI Responses client を構築できる");
    client
        .send(&ProviderAuth::new("keyring-api-key"), &request())
        .await
        .expect("keyring プロファイルの client が送信できる");
}

// Given: openai type と openai-responses 以外の protocol
// When: factory で構築する
// Then: openai-responses への是正を案内する InvalidProfile を返す
#[test]
fn factory_rejects_non_responses_protocol_for_openai() {
    for protocol in [
        model::ApiProtocol::OpenAiCompletions,
        model::ApiProtocol::AnthropicMessages,
        model::ApiProtocol::OpenAiCodexResponses,
        model::ApiProtocol::CursorAgent,
    ] {
        let (_directory, store) = credential_store();
        let profile = profile(protocol, env_credential());

        let error = build_provider_client(&profile, store, None, &FactoryOptions::default())
            .err()
            .expect("openai は openai-responses 以外を拒否する");

        assert!(
            matches!(error, RoutingError::InvalidProfile { .. }),
            "actual: {error:?}"
        );
        assert!(
            error.to_string().contains("openai-responses"),
            "actual: {error}"
        );
    }
}
