use super::*;

#[tokio::test]
async fn account_quota_uses_each_token_store_despite_available_app_server() {
    let server = MockServer::start().await;
    let mut clients = Vec::new();
    for (account, access_token, used_percent) in [
        ("account-1", "first-access", 12.0),
        ("account-2", "second-access", 83.0),
    ] {
        let store = store();
        let mut token = store.load().unwrap().unwrap();
        token.access_token = access_token.into();
        token.id_token = format!(
            "e30.{}.sig",
            URL_SAFE_NO_PAD.encode(
                serde_json::to_vec(&json!({
                    "exp": 4000000000_u64,
                    "https://api.openai.com/auth": {"chatgpt_account_id": account}
                }))
                .unwrap()
            )
        );
        store.save(&token).unwrap();
        let mut response = usage();
        response["rate_limit"]["primary_window"]["used_percent"] = json!(used_percent);
        Mock::given(method("GET"))
            .and(path("/usage"))
            .and(header("authorization", format!("Bearer {access_token}")))
            .and(header("chatgpt-account-id", account))
            .respond_with(ResponseTemplate::new(200).set_body_json(response))
            .expect(1)
            .mount(&server)
            .await;
        // This app-server would succeed with the ambient account's 25% usage.
        clients.push((
            CodexQuotaClient::new_for_account(
                fixture_config(format!("{}/usage", server.uri())),
                store,
            )
            .unwrap(),
            used_percent,
        ));
    }

    for (mut client, used_percent) in clients {
        let snapshot = client.fetch_quota().await.unwrap();
        assert_eq!(snapshot.source, QuotaSource::Wham);
        assert_eq!(snapshot.quota.primary.unwrap().used_percent, used_percent);
    }
}

#[tokio::test]
async fn expired_credentials_require_login_without_sending_request() {
    // Given: expired credentials and a server that would otherwise accept them.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(usage()))
        .expect(0)
        .mount(&server)
        .await;
    let store = store();
    let mut token = store.load().unwrap().unwrap();
    token.id_token = format!(
        "e30.{}.sig",
        URL_SAFE_NO_PAD.encode(
            serde_json::to_vec(&json!({
                "exp": 1, "https://api.openai.com/auth": {"chatgpt_account_id":"account-1"}
            }))
            .unwrap()
        )
    );
    store.save(&token).unwrap();
    let mut client = CodexQuotaClient::new(missing_config(server.uri()), store).unwrap();
    // When
    let error = client.fetch_quota().await.unwrap_err();
    // Then: the user can distinguish authentication recovery from transport failure.
    let QuotaError::Sources { wham, .. } = error else {
        panic!("expected sources")
    };
    assert!(matches!(*wham, QuotaError::ReauthenticationRequired));
}

#[tokio::test]
async fn unauthorized_response_requires_login() {
    // Given: a token with a future exp but rejected by the backend.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(401).set_body_string("SECRET"))
        .expect(1)
        .mount(&server)
        .await;
    let mut client = CodexQuotaClient::new(missing_config(server.uri()), store()).unwrap();
    // When
    let error = client.fetch_quota().await.unwrap_err();
    // Then
    let QuotaError::Sources { wham, .. } = error else {
        panic!("expected sources")
    };
    assert!(matches!(*wham, QuotaError::ReauthenticationRequired));
    assert!(!format!("{wham:?}").contains("SECRET"));
}
