//! factory の OpenAI 互換 client 構築契約を検証します。

use std::sync::Arc;

use providers::{ChatRequest, ContentBlock, Message, ProviderAuth, Role};
use routing::factory::{FactoryOptions, build_provider_client};
use routing::{CredentialRef, ProviderProfile, RoutingError};
use sandbox::credential::{CredentialStore, FileCredentialStore};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const PROFILE: &str = "local";
const MODEL: &str = "local-model";

fn profile(
    provider_type: model::ProviderType,
    api_protocol: model::ApiProtocol,
    credential: CredentialRef,
) -> ProviderProfile {
    ProviderProfile {
        name: PROFILE.to_string(),
        provider_type,
        api_protocol,
        base_url: "https://example.test".to_string(),
        credential,
        models: vec![MODEL.to_string()],
        default_model: MODEL.to_string(),
    }
}

fn env_credential() -> CredentialRef {
    CredentialRef::Env {
        var: "LOCAL_API_KEY".to_string(),
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
        max_tokens: None,
        observation: None,
    }
}

// Given: OpenAI互換プロファイルとAPIキー / When: factoryで構築したclientからsend / Then: Chat Completions endpointへBearer認証で送信する
#[tokio::test]
async fn factory_builds_openai_compatible_client() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(header("authorization", "Bearer test-api-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "chatcmpl-1",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "Hello"},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
        })))
        .expect(1)
        .mount(&server)
        .await;
    let (_directory, store) = credential_store();
    let mut profile = profile(
        model::ProviderType::OpenAiCompatible,
        model::ApiProtocol::OpenAiCompletions,
        env_credential(),
    );
    profile.base_url = server.uri();

    let client = build_provider_client(&profile, store, None, &FactoryOptions::default())
        .expect("OpenAI互換clientを構築できる");
    let response = client
        .send(&ProviderAuth::new("test-api-key"), &request())
        .await
        .expect("送信に成功する");

    assert_eq!(response.message.content.len(), 1);
}

// Given: OpenAI互換typeと非対応protocol / When: factoryで構築 / Then: InvalidProfileを返す
#[test]
fn factory_rejects_wrong_protocol_for_openai_compatible() {
    let (_directory, store) = credential_store();
    let profile = profile(
        model::ProviderType::OpenAiCompatible,
        model::ApiProtocol::OpenAiResponses,
        env_credential(),
    );

    let error = build_provider_client(&profile, store, None, &FactoryOptions::default())
        .err()
        .expect("非対応protocolを拒否する");

    assert!(matches!(error, RoutingError::InvalidProfile { .. }));
}

// Given: OpenAI互換typeとkeyring認証 / When: factoryで構築 / Then: fail-closedでInvalidProfileを返す
#[test]
fn factory_rejects_keyring_for_openai_compatible() {
    let (_directory, store) = credential_store();
    let profile = profile(
        model::ProviderType::OpenAiCompatible,
        model::ApiProtocol::OpenAiCompletions,
        CredentialRef::Keyring {
            service: "evorch".to_string(),
            account: PROFILE.to_string(),
        },
    );

    let error = build_provider_client(&profile, store, None, &FactoryOptions::default())
        .err()
        .expect("keyring認証を拒否する");

    assert!(matches!(error, RoutingError::InvalidProfile { .. }));
}

// Given: anthropic type / When: factoryで構築 / Then: UnsupportedProviderTypeを維持する
#[test]
fn factory_keeps_anthropic_unsupported() {
    let (_directory, store) = credential_store();
    let profile = profile(
        model::ProviderType::Anthropic,
        model::ApiProtocol::AnthropicMessages,
        env_credential(),
    );

    let error = build_provider_client(&profile, store, None, &FactoryOptions::default())
        .err()
        .expect("anthropicは未対応");

    assert_eq!(
        error,
        RoutingError::UnsupportedProviderType {
            provider_type: "anthropic".to_string()
        }
    );
}
