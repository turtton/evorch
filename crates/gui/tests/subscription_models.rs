use gui::model::provider_settings::{
    ModelsFetchState, ProfileEditor, ProviderKind, ProviderSettingsModel,
};
use providers::provider::claude::ClaudeTokenStore;
use std::{
    io::{Read, Write},
    net::TcpListener,
    sync::{Arc, mpsc},
};

fn catalog(status: &str, body: &str) -> (String, std::thread::JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let thread = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = Vec::new();
        let mut chunk = [0; 1024];
        while !request.windows(4).any(|w| w == b"\r\n\r\n") {
            let count = stream.read(&mut chunk).unwrap();
            assert!(count > 0);
            request.extend_from_slice(&chunk[..count]);
        }
        stream.write_all(response.as_bytes()).unwrap();
        String::from_utf8(request).unwrap()
    });
    (url, thread)
}
fn finish(settings: &mut ProviderSettingsModel) {
    let editor = match settings.editor.as_mut().unwrap() {
        ProfileEditor::OpenAiCompatible(editor) => editor,
        ProfileEditor::Subscription(editor) => &mut editor.models,
        _ => panic!("native editor"),
    };
    // The model worker emits completion; no runner-speed deadline is involved.
    let result = editor.models_rx.take().unwrap().recv().unwrap();
    let (tx, rx) = mpsc::channel();
    tx.send(result).unwrap();
    editor.models_rx = Some(rx);
    assert!(settings.poll_models());
}
#[test]
fn claude_api_catalog_uses_api_key_header_and_masks_echoed_error_body() {
    for (suffix, status) in [
        ("", "200 OK"),
        ("/", "200 OK"),
        ("/v1", "200 OK"),
        ("/v1/", "200 OK"),
        ("", "401 Unauthorized"),
    ] {
        let body = if status.starts_with("200") {
            r#"{"data":[{"id":"claude-test"}],"has_more":false}"#
        } else {
            r#"{"error":"echoed sensitive-api-key"}"#
        };
        let (base_url, server) = catalog(status, body);
        let mut settings = ProviderSettingsModel::default();
        settings.add(ProviderKind::ClaudeApi);
        let editor = settings.openai_mut().unwrap();
        editor.base_url = format!("{base_url}{suffix}");
        editor.start_models_fetch_with_key(Some("sensitive-api-key".into()));
        finish(&mut settings);
        let request = server.join().unwrap().to_ascii_lowercase();
        assert!(request.starts_with("get /v1/models?"));
        assert!(request.contains("x-api-key: sensitive-api-key"));
        assert!(!request.contains("authorization:"));
        let editor = settings.openai().unwrap();
        if status.starts_with("200") {
            assert_eq!(editor.available_models, Some(vec!["claude-test".into()]));
        } else {
            assert!(
                matches!(&editor.models_fetch_state, ModelsFetchState::Failed(message) if !message.contains("sensitive-api-key") && !message.contains("echoed"))
            );
        }
    }
}
#[test]
fn claude_subscription_catalog_uses_saved_oauth_account() {
    let (base_url, server) = catalog(
        "200 OK",
        r#"{"data":[{"id":"claude-account-model"}],"has_more":false}"#,
    );
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(sandbox::credential::FileCredentialStore::open(directory.path()).unwrap());
    routing::factory::CredentialStoreClaudeTokenStore::new(
        store.clone(),
        "selected-account".into(),
    )
    .save(&providers::provider::claude::ClaudeTokenBundle {
        access_token: "saved-subscription-token".into(),
        refresh_token: "saved-refresh".into(),
        expires_at: u64::MAX,
        account_id: None,
        email: None,
        org_id: None,
        org_name: None,
    })
    .unwrap();
    let mut settings = ProviderSettingsModel::default();
    settings.add(ProviderKind::ClaudeSubscription);
    let editor = settings.subscription_mut().unwrap();
    editor.account = "selected-account".into();
    editor.models.base_url = base_url;
    settings.start_models_fetch_with_store(Some(store));
    finish(&mut settings);
    let request = server.join().unwrap().to_ascii_lowercase();
    assert!(request.contains("authorization: bearer saved-subscription-token"));
    assert!(request.contains("anthropic-beta:"));
    assert!(!request.contains("x-api-key:"));
    assert_eq!(
        settings.subscription_mut().unwrap().models.available_models,
        Some(vec!["claude-account-model".into()])
    );
}
