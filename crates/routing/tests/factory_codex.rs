//! factory の codex subscription client 構築契約を検証します。

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use providers::provider::codex::tokens::{CodexTokenStore, TokenBundle};
use providers::{
    ChatRequest, ContentBlock, FinishReason, Message, ProviderAuth, ProviderCapabilities,
    ProviderError, Role, Usage,
};
use routing::factory::{
    CredentialStoreTokenStore, DEFAULT_AUTH_BASE_URL, FactoryOptions, build_provider_client,
};
use routing::{CredentialRef, ProviderProfile, RoutingError};
use sandbox::credential::{CredentialStore, FileCredentialStore, Secret};
use serde_json::json;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const MODEL: &str = "gpt-5.1-codex";
const ACCOUNT: &str = "codex-personal";

/// codex 成功応答の最小 SSE 本文。
const SSE_SUCCESS: &str = r#"event: response.created
data: {"type":"response.created","response":{"id":"resp-1","status":"in_progress"}}

event: response.output_text.delta
data: {"type":"response.output_text.delta","item_id":"msg-1","content_index":0,"delta":"Hello"}

event: response.output_text.delta
data: {"type":"response.output_text.delta","item_id":"msg-1","content_index":0,"delta":" world"}

event: response.completed
data: {"type":"response.completed","response":{"id":"resp-1","status":"completed","usage":{"input_tokens":12,"output_tokens":2,"total_tokens":14}}}

data: [DONE]
"#;

/// codex 必須クレームを持つダミー JWT を生成する。
fn dummy_jwt(exp: u64, account_id: &str) -> String {
    let payload = json!({
        "exp": exp,
        "https://api.openai.com/auth": {"chatgpt_account_id": account_id}
    });
    format!(
        "e30.{}.signature",
        URL_SAFE_NO_PAD.encode(payload.to_string())
    )
}

/// 検証対象のトークン一式を単一 JSON として表現する。
fn token_bundle_json(id_token: &str) -> String {
    json!({
        "access_token": "access-tok-1",
        "refresh_token": "refresh-1",
        "id_token": id_token,
    })
    .to_string()
}

/// 指定した種別・プロトコル・認証参照を持つプロファイルを生成する。
fn profile(
    provider_type: model::ProviderType,
    api_protocol: model::ApiProtocol,
    credential: CredentialRef,
) -> ProviderProfile {
    ProviderProfile {
        name: ACCOUNT.to_string(),
        provider_type,
        api_protocol,
        base_url: "https://chatgpt.com".to_string(),
        credential,
        models: vec![MODEL.to_string()],
        default_model: MODEL.to_string(),
    }
}

fn keyring_credential() -> CredentialRef {
    CredentialRef::Keyring {
        service: "evorch".to_string(),
        account: ACCOUNT.to_string(),
    }
}

/// 空のファイル資格情報ストアと、その寿命を担保する一時ディレクトリを返す。
fn temp_file_store() -> (tempfile::TempDir, Arc<dyn CredentialStore>) {
    let dir = tempfile::tempdir().expect("一時ディレクトリを作成できる");
    let store: Arc<dyn CredentialStore> =
        Arc::new(FileCredentialStore::open(dir.path()).expect("ストアを開ける"));
    (dir, store)
}

/// codex 契約テストで使用する最小リクエスト。
fn chat_request() -> ChatRequest {
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
        observation: None,
    }
}

// Given: codex プロファイルと keyring 認証情報が保存された CredentialStore
// When: build_provider_client で構築して送信する
// Then: codex 機能フラグの client が SSE 応答を canonical 応答へ集約する
#[tokio::test]
async fn factory_builds_codex_client_from_profile() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/backend-api/codex/responses"))
        .and(header("authorization", "Bearer access-tok-1"))
        .and(header("chatgpt-account-id", "acc-123"))
        .and(header("originator", "evorch"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(SSE_SUCCESS, "text/event-stream"))
        .expect(1)
        .mount(&server)
        .await;

    let (_dir, store) = temp_file_store();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time is after epoch")
        .as_secs();
    store
        .set(
            ACCOUNT,
            &Secret::from(token_bundle_json(&dummy_jwt(now + 3_600, "acc-123"))),
        )
        .expect("トークン一式を保存できる");

    let mut profile = profile(
        model::ProviderType::OpenAiCodex,
        model::ApiProtocol::OpenAiCodexResponses,
        keyring_credential(),
    );
    profile.base_url = server.uri();
    let options = FactoryOptions {
        auth_base_url_override: Some(server.uri()),
    };

    let client = build_provider_client(&profile, store, None, &options)
        .expect("factory は codex client を構築できる");

    assert_eq!(
        client.capabilities(),
        ProviderCapabilities {
            streaming: true,
            tool_use: true,
            reasoning: true
        }
    );

    let response = client
        .send(&ProviderAuth::new(""), &chat_request())
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
            cache_write_tokens: 0
        }
    );
    assert_eq!(response.finish_reason, FinishReason::Stop);
}

// Given: 環境変数参照の認証情報を持つ codex プロファイル
// When: build_provider_client を呼び出す
// Then: keyring 参照への是正を案内する InvalidProfile
#[test]
fn factory_rejects_env_credential_for_codex() {
    let (_dir, store) = temp_file_store();
    let profile = profile(
        model::ProviderType::OpenAiCodex,
        model::ApiProtocol::OpenAiCodexResponses,
        CredentialRef::Env {
            var: "EVORCH_CODEX_TOKEN".to_string(),
        },
    );

    let error = build_provider_client(&profile, store, None, &FactoryOptions::default())
        .err()
        .expect("codex は環境変数参照を拒否する");

    assert!(
        matches!(error, RoutingError::InvalidProfile { .. }),
        "actual: {error:?}"
    );
    assert!(error.to_string().contains("keyring"), "actual: {error}");
}

// Given: codex プロバイダと非 codex protocol の組み合わせ
// When: build_provider_client を呼び出す
// Then: protocol 是正を案内する InvalidProfile
#[test]
fn factory_rejects_protocol_mismatch() {
    let (_dir, store) = temp_file_store();
    let profile = profile(
        model::ProviderType::OpenAiCodex,
        model::ApiProtocol::OpenAiResponses,
        keyring_credential(),
    );

    let error = build_provider_client(&profile, store, None, &FactoryOptions::default())
        .err()
        .expect("codex は openai-responses を拒否する");

    assert!(
        matches!(error, RoutingError::InvalidProfile { .. }),
        "actual: {error:?}"
    );
    assert!(
        error.to_string().contains("openai-codex-responses"),
        "actual: {error}"
    );
}

// Given: codex 以外の provider type
// When: build_provider_client を呼び出す
// Then: UnsupportedProviderType が設定識別子を保持する
#[test]
fn factory_returns_unsupported_for_other_types() {
    for (provider_type, label) in [
        (model::ProviderType::Anthropic, "anthropic"),
        (model::ProviderType::GithubCopilot, "github-copilot"),
    ] {
        let (_dir, store) = temp_file_store();
        let profile = profile(
            provider_type,
            model::ApiProtocol::AnthropicMessages,
            keyring_credential(),
        );

        let error = build_provider_client(&profile, store, None, &FactoryOptions::default())
            .err()
            .expect("codex 以外は未対応");

        assert_eq!(
            error,
            RoutingError::UnsupportedProviderType {
                provider_type: label.to_string()
            }
        );
        assert!(error.to_string().contains(label), "actual: {error}");
    }
}

// Given: FileCredentialStore 上のトークンストアアダプタ
// When: load → save → load → 生ストアで破壊 → load の順に操作する
// Then: None → Some(等価) → InvalidJson が型付きで返る
#[test]
fn credential_store_token_store_round_trip() {
    let (_dir, store) = temp_file_store();
    let adapter = CredentialStoreTokenStore::new(Arc::clone(&store), ACCOUNT.to_string());

    assert_eq!(adapter.load().expect("空ストアの load は成功する"), None);

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time is after epoch")
        .as_secs();
    let bundle = TokenBundle {
        access_token: "access-tok-1".to_string(),
        refresh_token: "refresh-1".to_string(),
        id_token: dummy_jwt(now + 3_600, "acc-123"),
    };
    adapter.save(&bundle).expect("save は成功する");

    assert_eq!(adapter.load().expect("load は成功する"), Some(bundle));

    store
        .set(ACCOUNT, &Secret::from("not-json".to_string()))
        .expect("生ストアへ直接書き込める");

    let error = adapter
        .load()
        .expect_err("破損した JSON は load に失敗する");
    assert!(
        matches!(error, ProviderError::InvalidJson { .. }),
        "actual: {error:?}"
    );
}

// Given: 既定の認証先定数
// When: 参照する
// Then: OpenAI の auth endpoint に固定されている
#[test]
fn factory_default_auth_base_url_is_openai() {
    assert_eq!(DEFAULT_AUTH_BASE_URL, "https://auth.openai.com");
}
