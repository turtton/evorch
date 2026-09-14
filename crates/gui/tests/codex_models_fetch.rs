use gui::model::provider_settings::{ModelsFetchState, ProviderKind, ProviderSettingsModel};
use std::sync::{Arc, mpsc};
use std::time::Duration;

fn settings() -> ProviderSettingsModel {
    let mut settings = ProviderSettingsModel::default();
    settings.add(ProviderKind::CodexSubscription);
    settings
}

fn finish(settings: &mut ProviderSettingsModel) {
    let editor = settings.codex_mut().unwrap();
    let result = editor
        .fetch
        .models_rx
        .take()
        .unwrap()
        .recv_timeout(Duration::from_secs(5))
        .unwrap();
    let (tx, rx) = mpsc::channel();
    tx.send(result).unwrap();
    editor.fetch.models_rx = Some(rx);
    assert!(settings.poll_models());
}

#[test]
fn fails_when_credential_store_is_unavailable() {
    // Given
    let mut settings = settings();
    // When
    settings.start_models_fetch_with_store(None);
    assert_eq!(
        settings.codex_mut().unwrap().fetch.models_fetch_state,
        ModelsFetchState::Loading
    );
    finish(&mut settings);
    // Then
    assert!(
        matches!(&settings.codex_mut().unwrap().fetch.models_fetch_state,
        ModelsFetchState::Failed(error) if error.contains("Credential store unavailable"))
    );
}

#[test]
fn fails_when_oauth_is_missing_invalid_or_expired() {
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    use sandbox::CredentialStore;
    // Given
    let expired = format!(
        "header.{}.signature",
        URL_SAFE_NO_PAD
            .encode(r#"{"exp":1,"https://api.openai.com/auth":{"chatgpt_account_id":"account"}}"#)
    );
    let bundle = providers::provider::codex::tokens::TokenBundle {
        access_token: expired.clone(),
        refresh_token: "secret-refresh".into(),
        id_token: expired,
    };
    for secret in [
        None,
        Some("bad-secret-json".into()),
        Some(serde_json::to_string(&bundle).unwrap()),
    ] {
        let mut settings = settings();
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(sandbox::FileCredentialStore::open(dir.path()).unwrap());
        if let Some(secret) = secret {
            store
                .set(
                    &settings.codex_mut().unwrap().account,
                    &sandbox::Secret::from(secret),
                )
                .unwrap();
        }
        // When
        settings.start_models_fetch_with_store(Some(store));
        finish(&mut settings);
        // Then
        let editor = settings.codex_mut().unwrap();
        assert!(
            matches!(&editor.fetch.models_fetch_state, ModelsFetchState::Failed(error)
            if error.contains("Sign in") && !error.contains("secret"))
        );
        assert!(editor.fetch.available_models.is_none());
        assert_eq!(editor.models.len(), 5);
    }
}

#[test]
fn appends_selected_models_once_when_catalog_is_applied() {
    // Given
    let mut settings = settings();
    let editor = settings.codex_mut().unwrap();
    editor.fetch.models_fetch_state = ModelsFetchState::Loaded;
    editor.fetch.available_models = Some(vec![
        "new-b".into(),
        "gpt-6-astra".into(),
        "new-a".into(),
        "new-b".into(),
    ]);
    editor
        .fetch
        .fetch_selected
        .extend(["new-a", "new-b", "gpt-6-astra", "not-fetched"].map(str::to_owned));
    // When
    editor.apply_fetched_selection();
    // Then
    assert_eq!(&editor.models[5..], ["new-b", "new-a"]);
    assert_eq!(editor.default_model, "gpt-6-astra");
    assert!(editor.fetch.fetch_selected.is_empty());
}

#[test]
fn discards_result_when_account_changes_during_fetch() {
    // Given
    let mut settings = settings();
    settings.start_models_fetch_with_store(None);
    settings.codex_mut().unwrap().account = "other-account".into();
    // When
    finish(&mut settings);
    // Then
    assert!(
        matches!(&settings.codex_mut().unwrap().fetch.models_fetch_state,
        ModelsFetchState::Failed(error) if error.contains("changed"))
    );
}

#[test]
fn loads_catalog_when_profile_account_has_valid_access_token() {
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    use sandbox::CredentialStore;
    use std::io::{Read, Write};
    // Given
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut bytes = Vec::new();
        let mut buffer = [0; 1024];
        while !bytes.windows(4).any(|part| part == b"\r\n\r\n") {
            let count = stream.read(&mut buffer).unwrap();
            assert!(count > 0);
            bytes.extend_from_slice(&buffer[..count]);
        }
        let body = r#"{"models":[{"slug":"gpt-fetched"},{"slug":"gpt-fetched"}]}"#;
        write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        String::from_utf8(bytes).unwrap()
    });
    let token = format!(
        "header.{}.signature",
        URL_SAFE_NO_PAD.encode(r#"{"exp":18446744073709551615}"#)
    );
    let bundle = providers::provider::codex::tokens::TokenBundle {
        access_token: token.clone(),
        refresh_token: "unused-refresh".into(),
        id_token: format!(
            "header.{}.signature",
            URL_SAFE_NO_PAD.encode(r#"{"exp":18446744073709551615,"https://api.openai.com/auth":{"chatgpt_account_id":"jwt-account"}}"#)
        ),
    };
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(sandbox::FileCredentialStore::open(dir.path()).unwrap());
    store
        .set(
            "oauth-account",
            &sandbox::Secret::from(serde_json::to_string(&bundle).unwrap()),
        )
        .unwrap();
    let mut settings = settings();
    let editor = settings.codex_mut().unwrap();
    editor.account = "oauth-account".into();
    editor.fetch.base_url = base_url;
    // When
    settings.start_models_fetch_with_store(Some(store));
    finish(&mut settings);
    // Then
    let editor = settings.codex_mut().unwrap();
    assert_eq!(editor.fetch.models_fetch_state, ModelsFetchState::Loaded);
    assert_eq!(
        editor.fetch.available_models,
        Some(vec!["gpt-fetched".into()])
    );
    let request = server.join().unwrap();
    assert!(request.starts_with("GET /models?client_version=0.153.0 HTTP/1.1"));
    assert!(request.contains("chatgpt-account-id: jwt-account\r\n"));
    assert!(request.contains("originator: codex_cli_rs\r\n"));
    assert!(request.contains("user-agent: codex_cli_rs/0.153.0\r\n"));
    assert!(request.contains(&format!("authorization: Bearer {token}\r\n")));
    assert!(!request.contains("unused-refresh"));
}
