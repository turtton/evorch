use super::*;

#[tokio::test]
async fn malformed_or_empty_quota_response_is_not_an_empty_success() {
    let server = MockServer::start().await;
    Mock::given(path("/auth/usage"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"unexpected":"secret"})))
        .mount(&server)
        .await;
    Mock::given(path("/summary")).respond_with(ResponseTemplate::new(200).set_body_json(json!({"individualUsage":{"plan":{"used":0,"limit":0},"onDemand":{"enabled":false,"used":42}}}))).mount(&server).await;
    let quota = CursorQuotaClient::new(CursorQuotaConfig {
        base_url: server.uri(),
        summary_url: format!("{}/summary", server.uri()),
    })
    .unwrap();
    let error = quota.fetch(&token()).await.unwrap_err();
    assert!(matches!(error, ProviderError::InvalidJson { .. }));
    assert!(!error.to_string().contains("secret"));
}

#[tokio::test]
async fn quota_separates_model_pools_and_preserves_unknown_caps() {
    let server = MockServer::start().await;
    Mock::given(path("/auth/usage")).and(header("authorization", format!("Bearer {}", token().access_token)))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"gpt-4":{"numRequests":0,"maxRequestUsage":null}, "requests":{"numRequests":4,"maxRequestUsage":10}}))).mount(&server).await;
    Mock::given(path("/summary")).and(header("cookie", super::oauth::session_cookie("user_1", &token().access_token)))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"membershipType":"pro-plus", "billingCycleEnd":"2026-11-01T00:00:00Z", "individualUsage":{"plan":{"limit":7000,"used":0,"autoPercentUsed":12.5,"apiPercentUsed":40}, "onDemand":{"used":200,"limit":null}}}))).mount(&server).await;
    let quota = CursorQuotaClient::new(CursorQuotaConfig {
        base_url: server.uri(),
        summary_url: format!("{}/summary", server.uri()),
    })
    .unwrap()
    .fetch(&token())
    .await
    .unwrap();
    assert_eq!(quota.plan_type.as_deref(), Some("pro-plus"));
    assert_eq!(quota.windows.len(), 4);
    let auto = &quota.windows[1];
    let other = &quota.windows[2];
    let demand = &quota.windows[3];
    assert_eq!(
        (auto.label.as_str(), auto.used_percent, auto.limit),
        ("Cursor Models", Some(12.5), None)
    );
    assert_eq!(
        (other.used, other.limit, other.used_percent),
        (Some(28.0), Some(70.0), Some(40.0))
    );
    assert_eq!(
        (demand.used, demand.limit, demand.used_percent),
        (Some(2.0), None, None)
    );
    assert_eq!(auto.resets_at, Some(1_793_491_200));
}

#[tokio::test]
async fn custom_quota_endpoint_uses_only_its_legacy_usage_and_rejects_credential_urls() {
    let server = MockServer::start().await;
    Mock::given(path("/auth/usage"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"gpt-4":{"numRequests":4,"maxRequestUsage":null}})),
        )
        .expect(1)
        .mount(&server)
        .await;
    let quota = CursorQuotaClient::new(CursorQuotaConfig {
        base_url: server.uri(),
        ..Default::default()
    })
    .unwrap()
    .fetch(&token())
    .await
    .unwrap();
    assert_eq!(quota.windows[0].used_percent, None);
    assert_eq!(quota.windows[0].used, Some(4.0));
    assert!(
        CursorOAuthClient::new(CursorOAuthConfig {
            token_url: "https://secret@cursor.example/token".into(),
            ..Default::default()
        })
        .is_err()
    );
    assert!(
        CursorQuotaClient::new(CursorQuotaConfig {
            summary_url: "https://cursor.example/summary?token=secret".into(),
            ..Default::default()
        })
        .is_err()
    );
}

#[tokio::test]
async fn unavailable_summary_does_not_publish_an_uncapped_zero_legacy_bucket() {
    for status in [429, 503] {
        let server = MockServer::start().await;
        Mock::given(path("/auth/usage"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"gpt-4":{"numRequests":0,"maxRequestUsage":null}})),
            )
            .mount(&server)
            .await;
        Mock::given(path("/summary"))
            .respond_with(ResponseTemplate::new(status).set_body_string("credential-sentinel"))
            .mount(&server)
            .await;
        let quota = CursorQuotaClient::new(CursorQuotaConfig {
            base_url: server.uri(),
            summary_url: format!("{}/summary", server.uri()),
        })
        .unwrap();
        let error = quota.fetch(&token()).await.unwrap_err();
        assert_eq!(error.status(), Some(status));
        assert!(!error.to_string().contains("credential-sentinel"));
        server.reset().await;
        Mock::given(path("/auth/usage")).respond_with(ResponseTemplate::new(200).set_body_json(json!({"zero":{"numRequests":0,"maxRequestUsage":null},"capped":{"numRequests":0,"maxRequestUsage":100},"uncapped":{"numRequests":3,"maxRequestUsage":null}}))).mount(&server).await;
        Mock::given(path("/summary"))
            .respond_with(ResponseTemplate::new(status))
            .mount(&server)
            .await;
        let snapshot = quota.fetch(&token()).await.unwrap();
        assert_eq!(snapshot.windows.len(), 2);
        assert!(
            snapshot
                .windows
                .iter()
                .any(|w| w.limit == Some(100.0) && w.used == Some(0.0))
        );
        assert!(
            snapshot
                .windows
                .iter()
                .any(|w| w.limit.is_none() && w.used == Some(3.0))
        );
    }
}

#[tokio::test]
async fn summary_transport_failure_is_preserved_when_legacy_is_uninformative() {
    let server = MockServer::start().await;
    Mock::given(path("/auth/usage"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"gpt-4":{"numRequests":0,"maxRequestUsage":null}})),
        )
        .mount(&server)
        .await;
    // Explicitly close each accepted connection: no sleep or timeout is used to
    // simulate a transport outage, and possible HTTP retries see the same failure.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let summary_url = format!("http://{}/summary", listener.local_addr().unwrap());
    let closer = tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            drop(socket);
        }
    });
    let result = CursorQuotaClient::new(CursorQuotaConfig {
        base_url: server.uri(),
        summary_url,
    })
    .unwrap()
    .fetch(&token())
    .await;
    closer.abort();
    assert!(closer.await.unwrap_err().is_cancelled());
    assert!(matches!(result, Err(ProviderError::Transport { .. })));
}
