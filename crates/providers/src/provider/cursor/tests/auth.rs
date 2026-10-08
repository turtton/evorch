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
