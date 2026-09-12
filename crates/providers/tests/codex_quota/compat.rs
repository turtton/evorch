use super::*;

#[tokio::test]
async fn unknown_rpc_window_fields_preserve_other_window() {
    // Given: each nullable field independently, both null, and missing fields.
    for replacement in [
        "\"windowDurationMins\":null,\"resetsAt\":2000000000",
        "\"windowDurationMins\":300,\"resetsAt\":null",
        "\"windowDurationMins\":null,\"resetsAt\":null",
        "\"unused\":true",
    ] {
        let server = MockServer::start().await;
        let mut config = fixture_config(server.uri());
        config.app_server_args[1] = config.app_server_args[1].replace(
            "\"windowDurationMins\":300,\"resetsAt\":2000000000",
            replacement,
        );
        let mut client = CodexQuotaClient::new(config, store()).unwrap();
        // When
        let snapshot = client.fetch_quota().await.unwrap();
        // Then
        assert_eq!(snapshot.source, QuotaSource::AppServer);
        assert!(snapshot.quota.primary.is_none());
        assert!(snapshot.quota.secondary.is_some());
        assert!(server.received_requests().await.unwrap().is_empty());
    }
}

#[tokio::test]
async fn null_wham_limit_preserves_plan_without_inventing_windows() {
    // Given: unknown main limits with newer additional-limit fields.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "plan_type":"plus", "rate_limit":null,
            "additional_rate_limits":[], "rate_limits_by_limit_id":{}
        })))
        .mount(&server)
        .await;
    let mut client = CodexQuotaClient::new(missing_config(server.uri()), store()).unwrap();
    // When
    let snapshot = client.fetch_quota().await.unwrap();
    // Then
    assert_eq!(snapshot.quota.plan.as_deref(), Some("plus"));
    assert!(snapshot.quota.primary.is_none());
    assert!(snapshot.quota.secondary.is_none());
}
