use super::*;
use providers::provider::cursor::{CursorOAuthConfig, CursorTokenBundle};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{header, path},
};

fn bundle() -> CursorTokenBundle {
    CursorTokenBundle {
        access_token: "access-secret".into(),
        refresh_token: String::new(),
        api_key: Some("cursor-key-secret".into()),
        expires_at: 9_999_999_999,
        account_id: None,
        email: None,
    }
}

#[test]
fn api_key_credentials_restore_method_without_populating_input_or_config() {
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(sandbox::credential::FileCredentialStore::open(directory.path()).unwrap());
    routing::factory::CredentialStoreCursorTokenStore::new(store.clone(), "work".into())
        .save(&bundle())
        .unwrap();
    let mut editor = SubscriptionEditorModel::new("work".into(), ProviderKind::Cursor);
    editor.load_auth(Some(store));
    assert_eq!(editor.auth_method, SubscriptionAuthMethod::ApiKey);
    assert_eq!(
        editor.auth,
        SubscriptionAuthState::SignedIn {
            expires_at: bundle().expires_at
        }
    );
    assert!(editor.api_key_input.is_empty());
    editor.api_key_input = "cursor-key-secret".into();
    assert!(!format!("{editor:?}").contains("cursor-key-secret"));
    let path = directory.path().join("config.toml");
    config::save_subscription_provider_edit(&path, &editor.to_input(), None).unwrap();
    let saved = std::fs::read_to_string(path).unwrap();
    for secret in [
        "cursor-key-secret",
        "access-secret",
        "api_key",
        "access_token",
        "refresh_token",
    ] {
        assert!(!saved.contains(secret));
    }
    editor.set_auth_method(SubscriptionAuthMethod::Browser);
    assert!(editor.api_key_input.is_empty());
}

#[test]
fn api_key_start_requires_nonempty_input_and_clears_it_on_failure_or_cancel() {
    let mut editor = SubscriptionEditorModel::new("work".into(), ProviderKind::Cursor);
    editor.set_auth_method(SubscriptionAuthMethod::ApiKey);
    editor.api_key_input = " \n".into();
    editor.start_login(None);
    assert_eq!(
        editor.auth,
        SubscriptionAuthState::Failed("Enter a Cursor user API key")
    );
    assert!(editor.api_key_input.is_empty());
    editor.api_key_input = "cursor-key-secret".into();
    editor.start_login(None);
    assert_eq!(
        editor.auth,
        SubscriptionAuthState::Failed("Credential store unavailable")
    );
    assert!(editor.api_key_input.is_empty());
    editor.auth = SubscriptionAuthState::Exchanging;
    editor.api_key_input = "cursor-key-secret".into();
    editor.set_auth_method(SubscriptionAuthMethod::Browser);
    assert_eq!(editor.auth_method, SubscriptionAuthMethod::ApiKey);
    editor.cancel_login();
    assert_eq!(editor.auth, SubscriptionAuthState::SignedOut);
    assert!(editor.api_key_input.is_empty());
}

#[tokio::test]
async fn api_key_worker_persists_only_success_and_cancels_before_mutating_credentials() {
    let server = MockServer::start().await;
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(sandbox::credential::FileCredentialStore::open(directory.path()).unwrap());
    let tokens =
        routing::factory::CredentialStoreCursorTokenStore::new(store.clone(), "work".into());
    let config = CursorOAuthConfig {
        api_key_url: format!("{}/exchange", server.uri()),
        ..Default::default()
    };
    let (events, received) = mpsc::channel();
    let (commands, rx) = tokio::sync::mpsc::unbounded_channel();
    Mock::given(path("/exchange"))
        .and(header("authorization", "Bearer cursor-key-secret"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"accessToken":"access-secret"})),
        )
        .mount(&server)
        .await;
    let expires_at = oauth::cursor_login(
        store.clone(),
        "work".into(),
        &events,
        rx,
        Some(sandbox::Secret::from("cursor-key-secret".to_owned())),
        config.clone(),
    )
    .await
    .unwrap();
    assert!(expires_at > unix_now());
    assert!(
        received.try_recv().is_err(),
        "API key login must not open a browser"
    );
    assert_eq!(
        tokens.load().unwrap().unwrap().api_key.as_deref(),
        Some("cursor-key-secret")
    );
    drop(commands);
    let prior = tokens.load().unwrap().unwrap();

    server.reset().await;
    Mock::given(path("/exchange"))
        .respond_with(ResponseTemplate::new(401).set_body_string("cursor-key-secret"))
        .mount(&server)
        .await;
    let (_commands, rx) = tokio::sync::mpsc::unbounded_channel();
    let error = oauth::cursor_login(
        store.clone(),
        "work".into(),
        &events,
        rx,
        Some(sandbox::Secret::from("cursor-key-secret".to_owned())),
        config.clone(),
    )
    .await
    .unwrap_err();
    assert!(!error.contains("cursor-key-secret"));
    assert_eq!(tokens.load().unwrap().unwrap(), prior);

    server.reset().await;
    Mock::given(path("/exchange"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"accessToken":"replacement-secret"})),
        )
        .mount(&server)
        .await;
    let lock = tokens.refresh_lock();
    let guard = lock.lock().await;
    let (commands, rx) = tokio::sync::mpsc::unbounded_channel();
    let worker = oauth::cursor_login(
        store,
        "work".into(),
        &events,
        rx,
        Some(sandbox::Secret::from("cursor-key-secret".to_owned())),
        config,
    );
    let cancel = async {
        commands.send(LoginCommand::Cancel).unwrap();
        drop(guard);
    };
    let (result, ()) = tokio::join!(worker, cancel);
    assert_eq!(result, Err("Login cancelled"));
    assert_eq!(tokens.load().unwrap().unwrap(), prior);
}
