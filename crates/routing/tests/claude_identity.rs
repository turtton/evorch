use std::path::Path;
use std::sync::Arc;

use providers::provider::claude::{ClaudeInstallationId, ClaudeTokenBundle, ClaudeTokenStore};
use providers::{ChatRequest, ProviderAuth};
use routing::factory::{CredentialStoreClaudeTokenStore, FactoryOptions, build_provider_client};
use routing::{CredentialRef, ProviderProfile};
use sandbox::credential::{CredentialStore, FileCredentialStore};
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn profile(base_url: String) -> ProviderProfile {
    ProviderProfile {
        name: "first-profile".into(),
        provider_type: model::ProviderType::AnthropicSubscription,
        api_protocol: model::ApiProtocol::AnthropicMessages,
        base_url,
        credential: CredentialRef::Keyring {
            service: "evorch".into(),
            account: "claude".into(),
        },
        models: vec!["claude-test".into()],
        default_model: "claude-test".into(),
    }
}

fn request(run: &str) -> ChatRequest {
    serde_json::from_value(json!({"model":"claude-test","max_tokens":1024,
        "messages":[{"role":"user","content":[{"type":"text","text":"hello"}]}],
        "observation":{"run_id":run}}))
    .unwrap()
}

fn tokens(access: &str, refresh: &str) -> ClaudeTokenBundle {
    ClaudeTokenBundle {
        access_token: access.into(),
        refresh_token: refresh.into(),
        expires_at: u64::MAX,
        account_id: Some("account-a".into()),
        email: None,
        org_id: None,
        org_name: None,
    }
}

async fn server() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"role":"assistant",
            "content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn",
            "usage":{"input_tokens":1,"output_tokens":1}})),
        )
        .mount(&server)
        .await;
    server
}

async fn identity(server: &MockServer, index: usize) -> Value {
    let requests = server.received_requests().await.unwrap();
    let body: Value = requests[index].body_json().unwrap();
    serde_json::from_str(body["metadata"]["user_id"].as_str().unwrap()).unwrap()
}

fn options(path: &Path) -> FactoryOptions {
    FactoryOptions {
        claude_installation_id_path: Some(path.into()),
        ..Default::default()
    }
}

#[tokio::test]
async fn persistent_device_survives_token_rotation_profile_url_and_client_recreation() {
    let first_server = server().await;
    let second_server = server().await;
    let directory = tempfile::tempdir().unwrap();
    let seed_path = directory.path().join("config/claude-installation-id");
    let store: Arc<dyn CredentialStore> =
        Arc::new(FileCredentialStore::open(directory.path().join("credentials")).unwrap());
    let token_store = CredentialStoreClaudeTokenStore::new(store.clone(), "claude".into());
    token_store
        .save(&tokens("access-first", "refresh-first"))
        .unwrap();
    let client = build_provider_client(
        &profile(first_server.uri()),
        store.clone(),
        None,
        &options(&seed_path),
    )
    .unwrap();
    let installation = ClaudeInstallationId::load_or_create(&seed_path).unwrap();
    client
        .send(&ProviderAuth::new(""), &request("same-run"))
        .await
        .unwrap();
    let first = identity(&first_server, 0).await;
    token_store
        .save(&tokens("access-rotated", "refresh-rotated"))
        .unwrap();
    let mut renamed = profile(second_server.uri());
    renamed.name = "renamed-profile".into();
    let recreated =
        build_provider_client(&renamed, store.clone(), None, &options(&seed_path)).unwrap();
    recreated
        .send(&ProviderAuth::new(""), &request("same-run"))
        .await
        .unwrap();
    recreated
        .send(&ProviderAuth::new(""), &request("other-run"))
        .await
        .unwrap();
    assert_eq!(identity(&second_server, 0).await, first);
    let other_run = identity(&second_server, 1).await;
    assert_eq!(other_run["device_id"], first["device_id"]);
    assert_ne!(other_run["session_id"], first["session_id"]);
    assert_eq!(
        ClaudeInstallationId::load_or_create(&seed_path).unwrap(),
        installation
    );
    let second_installation = build_provider_client(
        &renamed,
        store,
        None,
        &options(&directory.path().join("second-config/seed")),
    )
    .unwrap();
    second_installation
        .send(&ProviderAuth::new(""), &request("same-run"))
        .await
        .unwrap();
    let second = identity(&second_server, 2).await;
    assert_ne!(second["device_id"], first["device_id"]);
    assert_eq!(second["session_id"], first["session_id"]);
    let requests = second_server.received_requests().await.unwrap();
    assert!(
        requests
            .iter()
            .all(|request| request.headers["authorization"] == "Bearer access-rotated")
    );
}

#[tokio::test]
async fn api_profile_never_reads_or_creates_installation_state_or_sends_metadata() {
    let server = server().await;
    let directory = tempfile::tempdir().unwrap();
    let store: Arc<dyn CredentialStore> =
        Arc::new(FileCredentialStore::open(directory.path().join("credentials")).unwrap());
    let mut api_profile = profile(server.uri());
    api_profile.provider_type = model::ProviderType::Anthropic;
    let missing = directory.path().join("absent/seed");
    let invalid = directory.path().join("invalid-seed");
    std::fs::write(&invalid, "private-invalid-seed").unwrap();
    for seed in [&missing, &invalid] {
        let client =
            build_provider_client(&api_profile, store.clone(), None, &options(seed)).unwrap();
        client
            .send(&ProviderAuth::new("api-key"), &request("api-run"))
            .await
            .unwrap();
    }
    assert!(!missing.parent().unwrap().exists());
    assert_eq!(
        std::fs::read_to_string(&invalid).unwrap(),
        "private-invalid-seed"
    );
    for request in server.received_requests().await.unwrap() {
        let body: Value = request.body_json().unwrap();
        assert!(body.get("metadata").is_none());
        assert!(!request.headers.contains_key("authorization"));
    }
    let error = build_provider_client(&profile(server.uri()), store, None, &options(&invalid))
        .err()
        .unwrap();
    assert!(!format!("{error:?} {error}").contains("private-invalid-seed"));
    assert_eq!(
        std::fs::read_to_string(&invalid).unwrap(),
        "private-invalid-seed"
    );
}
