use super::*;

#[tokio::test]
async fn browser_pkce_pending_poll_and_refresh_retains_token_family_without_secret_diagnostics() {
    let server = MockServer::start().await;
    let oauth = oauth(&server);
    let login = oauth.begin().unwrap();
    let url = reqwest::Url::parse(&login.authorization_url).unwrap();
    let params: std::collections::HashMap<_, _> = url.query_pairs().collect();
    assert_eq!(params["redirectTarget"], "cli");
    assert_eq!(params["mode"], "login");
    assert!(!format!("{login:?}").contains(params["challenge"].as_ref()));
    Mock::given(path("/poll"))
        .respond_with(ResponseTemplate::new(404))
        .up_to_n_times(1)
        .with_priority(1)
        .expect(1)
        .mount(&server)
        .await;
    assert!(oauth.poll(&login).await.unwrap().is_none());
    Mock::given(path("/poll"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"accessToken":token().access_token,"refreshToken":"first-refresh"}),
        ))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(path("/profile"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"sub":"user_1","email":"user@example.test"})),
        )
        .mount(&server)
        .await;
    let logged_in = oauth.poll(&login).await.unwrap().unwrap();
    assert_eq!(logged_in.expires_at, 9_999_999_999);
    assert_eq!(logged_in.account_id.as_deref(), Some("user_1"));
    assert_eq!(logged_in.email.as_deref(), Some("user@example.test"));
    let polls = server.received_requests().await.unwrap();
    let poll = polls.iter().find(|r| r.url.path() == "/poll").unwrap();
    let query: std::collections::HashMap<_, _> = poll.url.query_pairs().collect();
    use sha2::Digest;
    assert_eq!(
        URL_SAFE_NO_PAD.encode(sha2::Sha256::digest(query["verifier"].as_bytes())),
        params["challenge"]
    );
    Mock::given(path("/token"))
        .and(method("POST"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"access_token":token().access_token})),
        )
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    let refreshed = oauth.refresh(&logged_in).await.unwrap();
    assert_eq!(refreshed.refresh_token, "first-refresh");
    assert_eq!(refreshed.email, logged_in.email);
    assert!(!format!("{refreshed:?}").contains("first-refresh"));
    Mock::given(path("/token"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"shouldLogout":true, "access_token":"access-secret"})),
        )
        .mount(&server)
        .await;
    let error = oauth.refresh(&refreshed).await.unwrap_err().to_string();
    assert!(error.contains("log in again"));
    assert!(!error.contains("access-secret"));
}

#[tokio::test]
async fn independent_clients_share_refresh_rotation_lock() {
    let server = MockServer::start().await;
    let mut expired = token();
    expired.expires_at = 0;
    let store = Arc::new(InMemoryCursorTokenStore::new());
    store.save(&expired).unwrap();
    Mock::given(path("/token"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(
                json!({"access_token":token().access_token, "refresh_token":"rotated"}),
            ),
        )
        .expect(1)
        .mount(&server)
        .await;
    let config = CursorOAuthConfig {
        token_url: format!("{}/token", server.uri()),
        ..Default::default()
    };
    let a = CursorClient::new(CursorConfig::default(), store.clone())
        .unwrap()
        .with_oauth_config(config.clone())
        .unwrap();
    let b = CursorClient::new(CursorConfig::default(), store.clone())
        .unwrap()
        .with_oauth_config(config)
        .unwrap();
    let (a, b) = tokio::join!(a.access_token(), b.access_token());
    assert_eq!(a.unwrap().refresh_token, "rotated");
    assert_eq!(b.unwrap().refresh_token, "rotated");
}

#[tokio::test]
async fn api_key_exchange_uses_bearer_header_and_never_treats_key_as_refresh_token() {
    let server = MockServer::start().await;
    Mock::given(path("/exchange"))
        .and(method("POST"))
        .and(header("authorization", "Bearer cursor-key-secret"))
        .and(wiremock::matchers::body_json(json!({})))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"accessToken": token().access_token})),
        )
        .expect(1)
        .mount(&server)
        .await;
    let bundle = oauth(&server)
        .exchange_api_key("  cursor-key-secret\n")
        .await
        .unwrap();
    assert_eq!(bundle.access_token, token().access_token);
    assert_eq!(bundle.account_id.as_deref(), Some("user_1"));
    assert_eq!(bundle.expires_at, token().expires_at);
    assert_eq!(bundle.api_key.as_deref(), Some("cursor-key-secret"));
    assert!(bundle.refresh_token.is_empty());
    assert!(!format!("{bundle:?}").contains("cursor-key-secret"));
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn api_key_exchange_rejects_empty_keys_and_redacts_failed_or_malformed_responses() {
    let server = MockServer::start().await;
    let client = oauth(&server);
    assert!(client.exchange_api_key(" \n").await.is_err());
    assert!(server.received_requests().await.unwrap().is_empty());
    for response in [
        ResponseTemplate::new(401).set_body_string("cursor-key-secret"),
        ResponseTemplate::new(403).set_body_json(json!({"error":"cursor-key-secret"})),
        ResponseTemplate::new(200).set_body_string("cursor-key-secret"),
        ResponseTemplate::new(200)
            .set_body_json(json!({"accessToken":"  ","error":"cursor-key-secret"})),
    ] {
        server.reset().await;
        Mock::given(path("/exchange"))
            .respond_with(response)
            .mount(&server)
            .await;
        let error = client
            .exchange_api_key("cursor-key-secret")
            .await
            .unwrap_err();
        assert!(!format!("{error:?} {error}").contains("cursor-key-secret"));
    }
    assert!(
        CursorOAuthClient::new(CursorOAuthConfig {
            api_key_url: "https://cursor.example/exchange?key=secret".into(),
            ..Default::default()
        })
        .is_err()
    );
}

#[tokio::test]
async fn api_key_clients_share_reexchange_and_quota_uses_renewed_access_token() {
    let server = MockServer::start().await;
    let mut expired = token();
    expired.expires_at = 0;
    expired.refresh_token.clear();
    expired.api_key = Some("cursor-key-secret".into());
    let store = Arc::new(InMemoryCursorTokenStore::new());
    store.save(&expired).unwrap();
    Mock::given(path("/exchange"))
        .and(header("authorization", "Bearer cursor-key-secret"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"accessToken":token().access_token})),
        )
        .expect(1)
        .mount(&server)
        .await;
    let config = CursorOAuthConfig {
        api_key_url: format!("{}/exchange", server.uri()),
        ..Default::default()
    };
    let a = CursorClient::new(CursorConfig::default(), store.clone())
        .unwrap()
        .with_oauth_config(config.clone())
        .unwrap();
    let b = CursorClient::new(CursorConfig::default(), store.clone())
        .unwrap()
        .with_oauth_config(config)
        .unwrap();
    let (a, b) = tokio::join!(a.access_token(), b.access_token());
    for renewed in [a.unwrap(), b.unwrap()] {
        assert_eq!(renewed.api_key, expired.api_key);
        assert_eq!(renewed.email, expired.email);
        assert_eq!(renewed.expires_at, token().expires_at);
        assert!(renewed.refresh_token.is_empty());
    }
    let mut expired = store.load().unwrap().unwrap();
    expired.expires_at = 0;
    store.save(&expired).unwrap();
    server.reset().await;
    Mock::given(path("/exchange"))
        .and(header("authorization", "Bearer cursor-key-secret"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"accessToken":token().access_token})),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(path("/auth/usage"))
        .and(header(
            "authorization",
            format!("Bearer {}", token().access_token),
        ))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"requests":{"numRequests":4,"maxRequestUsage":10}})),
        )
        .expect(1)
        .mount(&server)
        .await;
    let snapshot = CursorQuotaClient::new(CursorQuotaConfig {
        base_url: server.uri(),
        ..Default::default()
    })
    .unwrap()
    .fetch_for_store(store.as_ref(), &oauth(&server))
    .await
    .unwrap();
    assert_eq!(snapshot.windows[0].used, Some(4.0));
    assert_eq!(store.load().unwrap().unwrap().api_key, expired.api_key);
}

#[tokio::test]
async fn rejected_api_key_renewal_preserves_the_stored_credential_family() {
    let server = MockServer::start().await;
    let mut expired = token();
    expired.expires_at = 0;
    expired.api_key = Some("cursor-key-secret".into());
    let store = Arc::new(InMemoryCursorTokenStore::new());
    store.save(&expired).unwrap();
    Mock::given(path("/exchange"))
        .respond_with(ResponseTemplate::new(401).set_body_string("cursor-key-secret"))
        .expect(1)
        .mount(&server)
        .await;
    let client = CursorClient::new(CursorConfig::default(), store.clone())
        .unwrap()
        .with_oauth_config(CursorOAuthConfig {
            api_key_url: format!("{}/exchange", server.uri()),
            ..Default::default()
        })
        .unwrap();
    let error = client.access_token().await.unwrap_err();
    assert_eq!(error.status(), Some(401));
    assert!(!error.to_string().contains("cursor-key-secret"));
    assert_eq!(store.load().unwrap().unwrap(), expired);
}
