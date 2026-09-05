//! codex の資格情報が sandbox 子プロセス環境へ漏れないことを検証します。

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use providers::{ChatRequest, ContentBlock, Message, ProviderAuth, Role};
use routing::CredentialRef;
use routing::factory::{FactoryOptions, build_provider_client};
use sandbox::Sandbox;
use sandbox::credential::{CredentialStore, FileCredentialStore, Secret};
use sandbox::exec::{CommandSpec, DirectSandbox};
use serde_json::json;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const ACCOUNT: &str = "codex-isolation";
const MODEL: &str = "gpt-5.1-codex";
const ACCESS: &str = "SENTINEL-ACCESS-XYZ";
const REFRESH: &str = "SENTINEL-REFRESH-XYZ";
const ACCOUNT_ID: &str = "SENTINEL-ACC-ID";
const SSE_SUCCESS: &str = "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp-1\",\"status\":\"in_progress\"}}\n\nevent: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp-1\",\"status\":\"completed\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1,\"total_tokens\":2}}}\n\ndata: [DONE]\n";

fn dummy_jwt(exp: u64) -> String {
    let payload = json!({
        "exp": exp,
        "https://api.openai.com/auth": {"chatgpt_account_id": ACCOUNT_ID}
    });
    format!(
        "e30.{}.signature",
        URL_SAFE_NO_PAD.encode(payload.to_string())
    )
}

fn request() -> ChatRequest {
    ChatRequest {
        model: MODEL.to_owned(),
        messages: vec![Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: "hello".to_owned(),
            }],
        }],
        tools: Vec::new(),
        temperature: None,
        max_tokens: Some(1),
        observation: None,
    }
}

// Given: codex credentials are live in process memory after one successful request.
// When: a command is wrapped by the direct sandbox.
// Then: only the documented parent allowlist and explicit extra environment are present.
#[tokio::test]
async fn codex_tokens_never_reach_sandbox_child_env() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/backend-api/codex/responses"))
        .and(header("authorization", format!("Bearer {ACCESS}")))
        .and(header("chatgpt-account-id", ACCOUNT_ID))
        .respond_with(ResponseTemplate::new(200).set_body_raw(SSE_SUCCESS, "text/event-stream"))
        .expect(1)
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().expect("temporary directory is available");
    let store: Arc<dyn CredentialStore> =
        Arc::new(FileCredentialStore::open(dir.path()).expect("credential store is available"));
    let bundle = json!({
        "access_token": ACCESS,
        "refresh_token": REFRESH,
        "id_token": dummy_jwt(SystemTime::now().duration_since(UNIX_EPOCH).expect("clock is valid").as_secs() + 86_400),
    });
    store
        .set(ACCOUNT, &Secret::from(bundle.to_string()))
        .expect("credentials can be seeded");

    let profile = routing::ProviderProfile {
        name: ACCOUNT.to_owned(),
        provider_type: model::ProviderType::OpenAiCodex,
        api_protocol: model::ApiProtocol::OpenAiCodexResponses,
        base_url: server.uri(),
        credential: CredentialRef::Keyring {
            service: "evorch".to_owned(),
            account: ACCOUNT.to_owned(),
        },
        models: vec![MODEL.to_owned()],
        default_model: MODEL.to_owned(),
    };
    let client = build_provider_client(
        &profile,
        store,
        None,
        &FactoryOptions {
            auth_base_url_override: Some(server.uri()),
            ..FactoryOptions::default()
        },
    )
    .expect("codex client is built");
    client
        .send(&ProviderAuth::new(""), &request())
        .await
        .expect("codex request succeeds");

    let wrapped = DirectSandbox::new_unchecked()
        .wrap(CommandSpec {
            program: "true".to_owned(),
            args: Vec::new(),
            cwd: Some(dir.path().to_owned()),
            extra_env: vec![("MY_ENV".to_owned(), "my-val".to_owned())],
        })
        .expect("command is wrapped");
    let allowlist = ["PATH", "TERM", "LANG", "LC_ALL"];
    let parent: std::collections::HashMap<_, _> = std::env::vars().collect();
    for (key, value) in &wrapped.env {
        assert!(
            ![ACCESS, REFRESH, ACCOUNT_ID]
                .iter()
                .any(|sentinel| value.contains(sentinel))
        );
        if parent
            .get(key)
            .is_some_and(|parent_value| parent_value == value)
        {
            assert!(
                allowlist.contains(&key.as_str()),
                "unexpected inherited key: {key}"
            );
        }
    }
    assert!(
        wrapped
            .env
            .contains(&("MY_ENV".to_owned(), "my-val".to_owned()))
    );
    assert!(!std::env::vars().any(|(_, value)| {
        [ACCESS, REFRESH, ACCOUNT_ID]
            .iter()
            .any(|sentinel| value.contains(sentinel))
    }));
}

// DirectSandbox and merge_environment cover the shared environment merge path used by bwrap.
