use super::*;

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
