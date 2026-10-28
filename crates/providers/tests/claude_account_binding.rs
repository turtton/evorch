use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use providers::provider::anthropic::AnthropicConfig;
use providers::provider::claude::{
    ClaudeClient, ClaudeOAuthConfig, ClaudeTokenBundle, ClaudeTokenStore, InMemoryClaudeTokenStore,
};
use providers::{ChatRequest, ObservationContext, ProviderAuth, ProviderClient, ProviderError};
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn bundle(account: Option<&str>, token: &str, expires_at: u64) -> ClaudeTokenBundle {
    ClaudeTokenBundle {
        access_token: token.into(),
        refresh_token: "refresh-private".into(),
        expires_at,
        account_id: account.map(str::to_owned),
        email: None,
        org_id: None,
        org_name: None,
    }
}

fn input(run: &str) -> ChatRequest {
    let mut input: ChatRequest =
        serde_json::from_value(json!({"model":"claude-sonnet-4-6", "messages":[
            {"role":"user","content":[{"type":"text","text":"Reply LOCAL_CAPTURE_OK."}]}
        ]}))
        .unwrap();
    input.observation = Some(ObservationContext {
        attempts: None,
        run_id: run.into(),
        ..Default::default()
    });
    input
}

fn client(server: &MockServer, store: Arc<InMemoryClaudeTokenStore>) -> ClaudeClient {
    ClaudeClient::new(
        AnthropicConfig {
            base_url: server.uri(),
            ..Default::default()
        },
        store,
    )
    .unwrap()
    .with_oauth_config(ClaudeOAuthConfig {
        token_url: format!("{}/token", server.uri()),
        ..Default::default()
    })
    .unwrap()
}

fn identity(request: &wiremock::Request) -> (Value, Value) {
    let body: Value = serde_json::from_slice(&request.body).unwrap();
    let identity = serde_json::from_str(body["metadata"]["user_id"].as_str().unwrap()).unwrap();
    (body, identity)
}

async fn mount_messages(server: &MockServer, count: u64) {
    let sequence = AtomicUsize::new(0);
    Mock::given(method("POST"))
        .and(path("/messages"))
        .respond_with(move |_: &wiremock::Request| {
            let sequence = sequence.fetch_add(1, Ordering::SeqCst);
            ResponseTemplate::new(200)
                .insert_header("request-id", format!("req_account_{sequence}"))
                .set_body_json(
                    json!({"id":format!("msg_account_{sequence}"),"role":"assistant",
                "content":[{"type":"text","text":"ok"}], "stop_reason":"end_turn",
                "usage":{"input_tokens":10,"output_tokens":1}}),
                )
        })
        .expect(count)
        .mount(server)
        .await;
}

#[tokio::test]
async fn login_after_construction_binds_actual_bundle_and_rejects_warm_account_switch() {
    let server = MockServer::start().await;
    mount_messages(&server, 3).await;
    let store = Arc::new(InMemoryClaudeTokenStore::new());
    let existing = client(&server, store.clone());
    let auth = ProviderAuth::new("unused-private");
    store
        .save(&bundle(
            Some("account-alpha-private"),
            "token-alpha-private",
            u64::MAX,
        ))
        .unwrap();
    let first_input = input("first-run");
    existing.send(&auth, &first_input).await.unwrap();
    store
        .save(&bundle(
            Some("account-alpha-private"),
            "token-rotated-private",
            u64::MAX,
        ))
        .unwrap();
    existing.send(&auth, &first_input).await.unwrap();

    // An expired replacement must fail before refresh HTTP as well as inference HTTP.
    store
        .save(&bundle(
            Some("account-beta-private"),
            "token-beta-private",
            0,
        ))
        .unwrap();
    let send_error = existing.send(&auth, &first_input).await.unwrap_err();
    let stream_error = match existing.stream(&auth, &first_input).await {
        Ok(_) => panic!("warm account switch opened a stream"),
        Err(error) => error,
    };
    for error in [send_error, stream_error] {
        assert!(
            matches!(&error, ProviderError::Request(detail) if detail.contains("recreate") && detail.contains("new run"))
        );
        let detail = error.to_string();
        assert!(!detail.contains("private"));
        assert!(!detail.contains("alpha"));
        assert!(!detail.contains("beta"));
    }
    assert_eq!(server.received_requests().await.unwrap().len(), 2);

    store
        .save(&bundle(
            Some("account-beta-private"),
            "token-beta-private",
            u64::MAX,
        ))
        .unwrap();
    client(&server, store)
        .send(&auth, &input("new-run"))
        .await
        .unwrap();
    let captured = server.received_requests().await.unwrap();
    let (first, first_identity) = identity(&captured[0]);
    let (second, second_identity) = identity(&captured[1]);
    let (new, new_identity) = identity(&captured[2]);
    assert_eq!(first_identity["account_uuid"], "account-alpha-private");
    assert_eq!(first_identity, second_identity);
    assert_eq!(
        mock_openai::claude_contract::claude_model_input(&captured[0].body).unwrap(),
        mock_openai::claude_contract::claude_model_input(&captured[1].body).unwrap()
    );
    assert_eq!(
        captured[0].headers["authorization"],
        "Bearer token-alpha-private"
    );
    assert_eq!(
        captured[1].headers["authorization"],
        "Bearer token-rotated-private"
    );
    assert_eq!(first["diagnostics"]["previous_message_id"], Value::Null);
    assert_eq!(
        second["diagnostics"]["previous_message_id"],
        "msg_account_0"
    );
    assert!(
        second["system"][0]["text"]
            .as_str()
            .unwrap()
            .contains("cc_prev_req=req_account_0;")
    );
    assert_eq!(new_identity["account_uuid"], "account-beta-private");
    assert_ne!(new_identity["device_id"], first_identity["device_id"]);
    assert_ne!(new_identity["session_id"], first_identity["session_id"]);
    assert_eq!(new["diagnostics"]["previous_message_id"], Value::Null);
    assert!(
        !new["system"][0]["text"]
            .as_str()
            .unwrap()
            .contains("cc_prev_req=")
    );
}

#[tokio::test]
async fn model_listing_does_not_bind_messages_and_empty_accounts_share_none_identity() {
    let server = MockServer::start().await;
    mount_messages(&server, 2).await;
    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data":[]})))
        .expect(1)
        .mount(&server)
        .await;
    let store = Arc::new(InMemoryClaudeTokenStore::new());
    store
        .save(&bundle(
            Some("account-before-listing"),
            "listing-token",
            u64::MAX,
        ))
        .unwrap();
    let client = client(&server, store.clone());
    let auth = ProviderAuth::new("unused");
    assert_eq!(client.list_models(&auth).await.unwrap(), Some(Vec::new()));
    store
        .save(&bundle(Some("  "), "empty-account-token", u64::MAX))
        .unwrap();
    let input = input("none-account-run");
    client.send(&auth, &input).await.unwrap();
    store
        .save(&bundle(None, "none-account-token", u64::MAX))
        .unwrap();
    client.send(&auth, &input).await.unwrap();
    let captured: Vec<_> = server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|request| request.method == "POST")
        .collect();
    let (first, first_identity) = identity(&captured[0]);
    let (second, second_identity) = identity(&captured[1]);
    assert!(first_identity.get("account_uuid").is_none());
    assert_eq!(first_identity, second_identity);
    assert_eq!(first["diagnostics"]["previous_message_id"], Value::Null);
    assert_eq!(
        second["diagnostics"]["previous_message_id"],
        "msg_account_0"
    );
}
