use providers::{
    ProviderAuth,
    provider::{codex::quota::QuotaError, kimi_quota::KimiQuotaClient},
};
use serde_json::json;
use std::time::Duration;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{header, method, path},
};

#[tokio::test]
async fn named_windows_preserve_monthly_limits_and_use_configured_endpoint() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/coding/v1/usages"))
        .and(header("Authorization", "Bearer fixture-key"))
        .and(header("Accept", "application/json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "usages": {
                "limit_5h": {"used_ratio": 0.3, "reset_time": "2026-10-01T12:00:00Z"},
                "limit_7d": {"used_ratio": "0.2"},
                "limit_month_total": {"used_ratio": 0.4},
                "limit_month_code": {"used_ratio": 0.25}
            },
            "usage": {"limit":"100", "used":"99"}
        })))
        .expect(1)
        .mount(&server)
        .await;
    let client = KimiQuotaClient::new(
        &format!("{}/coding/v1/", server.uri()),
        Duration::from_secs(2),
    )
    .unwrap();
    let snapshot = client
        .fetch_quota(&ProviderAuth::new("fixture-key"))
        .await
        .unwrap();
    assert_eq!(
        snapshot
            .windows
            .iter()
            .map(|window| (window.label.as_str(), window.remaining_percent))
            .collect::<Vec<_>>(),
        [
            ("5h", 70.0),
            ("wk", 80.0),
            ("month", 60.0),
            ("code month", 75.0)
        ]
    );
    assert!(snapshot.windows[0].resets_at.is_some());
    assert!(snapshot.windows[1].resets_at.is_none());
}

#[tokio::test]
async fn legacy_plan_parses_string_counts_and_remaining_with_nanosecond_reset() {
    let server = MockServer::start().await;
    Mock::given(method("GET")).respond_with(ResponseTemplate::new(200).set_body_json(json!({
        "usage": {"limit":"200", "remaining":"150", "resetTime":"2026-10-01T12:00:00.123456789Z"},
        "limits": [{"window":{"duration":300,"timeUnit":"TIME_UNIT_MINUTE"}, "detail":{"limit":"100", "used":"90"}}]
    }))).mount(&server).await;
    let client = KimiQuotaClient::new(&server.uri(), Duration::from_secs(2)).unwrap();
    let snapshot = client
        .fetch_quota(&ProviderAuth::new("fixture-key"))
        .await
        .unwrap();
    assert_eq!(snapshot.windows[0].label, "5h");
    assert_eq!(snapshot.windows[0].remaining_percent, 10.0);
    assert_eq!(snapshot.windows[1].label, "wk");
    assert_eq!(snapshot.windows[1].remaining_percent, 75.0);
    assert!(snapshot.windows[1].resets_at.is_some());
}

#[tokio::test]
async fn malformed_or_absent_usage_never_fabricates_full_remaining_quota() {
    for body in [
        json!({}),
        json!({"usages":{"limit_5h":{"used_ratio":"NaN"}}}),
        json!({"usage":{"limit":"0","used":"0"}}),
        json!({"usage":{"limit":"100"}}),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;
        let client = KimiQuotaClient::new(&server.uri(), Duration::from_secs(2)).unwrap();
        assert!(matches!(
            client.fetch_quota(&ProviderAuth::new("fixture-key")).await,
            Err(QuotaError::Protocol(_))
        ));
    }
}

#[tokio::test]
async fn authentication_failures_and_redirects_are_sanitized_and_not_followed() {
    let destination = MockServer::start().await;
    for status in [401, 403, 302, 500] {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(status)
                    .insert_header("Location", destination.uri())
                    .set_body_string("fixture-key secret echoed by upstream"),
            )
            .expect(1)
            .mount(&server)
            .await;
        let client = KimiQuotaClient::new(&server.uri(), Duration::from_secs(2)).unwrap();
        let error = client
            .fetch_quota(&ProviderAuth::new("fixture-key"))
            .await
            .unwrap_err();
        assert!(!error.to_string().contains("fixture-key"));
        assert!(if status == 401 || status == 403 {
            matches!(error, QuotaError::ReauthenticationRequired)
        } else {
            matches!(error, QuotaError::HttpStatus(code) if code == status)
        });
    }
    assert!(destination.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn missing_credentials_never_send_a_request() {
    let server = MockServer::start().await;
    let client = KimiQuotaClient::new(&server.uri(), Duration::from_secs(2)).unwrap();
    assert!(matches!(
        client.fetch_quota(&ProviderAuth::new(" ")).await,
        Err(QuotaError::Credentials)
    ));
    assert!(server.received_requests().await.unwrap().is_empty());
}
