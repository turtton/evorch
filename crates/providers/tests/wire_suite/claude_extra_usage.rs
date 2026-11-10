//! Public quota contracts for current/legacy monetary spend and shared weekly caps.
use providers::provider::claude::{ClaudeExtraUsage, ClaudeQuotaClient, ClaudeTokenBundle};
use serde_json::json;
use std::time::Duration;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn credentials() -> ClaudeTokenBundle {
    ClaudeTokenBundle {
        access_token: "quota-access".into(),
        refresh_token: "quota-refresh".into(),
        expires_at: u64::MAX,
        account_id: None,
        email: None,
        org_id: None,
        org_name: None,
    }
}

#[tokio::test]
async fn claude_extra_usage_preserves_currency_units_caps_and_authoritative_spend() {
    let server = MockServer::start().await;
    let client = ClaudeQuotaClient::new(&server.uri(), Duration::from_secs(30)).unwrap();
    let legacy = json!({"is_enabled":true,"used_credits":1234,"monthly_limit":10000});
    let cases = [
        (
            json!({"extra_usage":legacy}),
            Some(ClaudeExtraUsage {
                used_usd: 12.34,
                limit_usd: Some(100.0),
            }),
        ),
        (
            json!({"extra_usage":{"is_enabled":true,"used_credits":567,"monthly_limit":null}}),
            Some(ClaudeExtraUsage {
                used_usd: 5.67,
                limit_usd: None,
            }),
        ),
        (
            json!({"extra_usage":{"is_enabled":true,"used_credits":567,"monthly_limit":5000,"decimal_places":3,"currency":"usd"}}),
            Some(ClaudeExtraUsage {
                used_usd: 0.567,
                limit_usd: Some(5.0),
            }),
        ),
        (
            json!({"spend":{"enabled":true,"used":{"amount_minor":2500.0,"exponent":2.0,"currency":"USD"},"limit":{"amount_minor":10000,"exponent":2,"currency":"USD"}},"extra_usage":legacy}),
            Some(ClaudeExtraUsage {
                used_usd: 25.0,
                limit_usd: Some(100.0),
            }),
        ),
        (
            json!({"spend":{"enabled":true,"used":{"amount_minor":0,"exponent":2,"currency":"USD"},"limit":null}}),
            Some(ClaudeExtraUsage {
                used_usd: 0.0,
                limit_usd: None,
            }),
        ),
        (
            json!({"spend":{"enabled":false},"extra_usage":legacy}),
            None,
        ),
        (
            json!({"spend":{"enabled":true,"used":{"amount_minor":100,"exponent":2,"currency":"EUR"},"limit":null},"extra_usage":legacy}),
            None,
        ),
        (
            json!({"extra_usage":{"is_enabled":true,"used_credits":12.5,"monthly_limit":10000}}),
            None,
        ),
        (
            json!({"extra_usage":{"is_enabled":true,"used_credits":100,"monthly_limit":0}}),
            None,
        ),
    ];
    for (mut payload, expected) in cases {
        // A legacy window permits testing malformed/disabled spend without fabricating extra usage.
        payload["seven_day"] = json!({"utilization":20});
        Mock::given(method("GET"))
            .and(path("/api/oauth/usage"))
            .respond_with(ResponseTemplate::new(200).set_body_json(payload))
            .expect(1)
            .mount(&server)
            .await;
        assert_eq!(
            client
                .fetch_quota(&credentials())
                .await
                .unwrap()
                .extra_usage,
            expected
        );
        server.verify().await;
        server.reset().await;
    }
}

#[tokio::test]
async fn claude_api_limits_weekly_all_and_extra_only_payloads_remain_reportable() {
    let server = MockServer::start().await;
    let client = ClaudeQuotaClient::new(&server.uri(), Duration::from_secs(30)).unwrap();
    Mock::given(method("GET")).respond_with(ResponseTemplate::new(200).set_body_json(json!({
        "api_limits":[{"kind":"session","percent":15},{"kind":"weekly_all","percent":65,"is_active":false}]})))
        .expect(1).mount(&server).await;
    let quota = client.fetch_quota(&credentials()).await.unwrap();
    assert_eq!(
        quota
            .windows
            .iter()
            .map(|w| w.label.as_str())
            .collect::<Vec<_>>(),
        ["5h", "7d"]
    );
    assert_eq!(quota.windows[1].remaining_percent, 35.0);
    assert_eq!(quota.extra_usage, None);
    server.verify().await;
    server.reset().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
        "extra_usage":{"is_enabled":true,"used_credits":500,"monthly_limit":null}})))
        .expect(1)
        .mount(&server)
        .await;
    let quota = client.fetch_quota(&credentials()).await.unwrap();
    assert!(quota.windows.is_empty());
    assert_eq!(
        quota.extra_usage,
        Some(ClaudeExtraUsage {
            used_usd: 5.0,
            limit_usd: None
        })
    );
}
