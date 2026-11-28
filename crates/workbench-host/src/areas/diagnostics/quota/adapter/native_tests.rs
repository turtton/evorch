//! Exercise real native HTTP decoding and the diagnostics adapter, not a fake fetch.
use super::*;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use providers::provider::codex::tokens::{CodexTokenStore, InMemoryTokenStore, TokenBundle};
use sandbox::CredentialStore;
use serde_json::{Value, json};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{header, method, path},
};

fn assert_unknown_first(windows: &[QuotaWindow]) {
    assert_eq!(windows.len(), 2);
    assert_eq!(windows[0].remaining_basis_points, None);
    assert!(windows[0].resets_at_ms.is_some());
    assert_eq!(windows[1].remaining_basis_points, Some(7500));
    assert!(serde_json::to_value(windows).unwrap()[0]["remaining_basis_points"].is_null());
}

#[tokio::test]
async fn codex_native_percentage_anomalies_keep_windows_and_other_values() {
    for used in [
        json!(-0.1),
        json!(100.1),
        json!("NaN"),
        json!("inf"),
        json!("-inf"),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/usage"))
            .and(header("authorization", "Bearer fixture-access"))
            .and(header("chatgpt-account-id", "fixture-account"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "rate_limit": {
                    "primary_window": {
                        "used_percent": used, "limit_window_seconds": 18000, "reset_at": 2000000000
                    },
                    "secondary_window": {
                        "used_percent": 25, "limit_window_seconds": 604800, "reset_at": 2000600000
                    }
                }
            })))
            .expect(1)
            .mount(&server)
            .await;
        let store = Arc::new(InMemoryTokenStore::new());
        let claims = URL_SAFE_NO_PAD.encode(
            serde_json::to_vec(&json!({
                "exp": 4000000000_u64,
                "https://api.openai.com/auth": {"chatgpt_account_id": "fixture-account"}
            }))
            .unwrap(),
        );
        store
            .save(&TokenBundle {
                access_token: "fixture-access".into(),
                refresh_token: "fixture-refresh".into(),
                id_token: format!("e30.{claims}.sig"),
            })
            .unwrap();
        let mut native = Native::Codex(Box::new(
            CodexQuotaClient::new_for_account(
                QuotaConfig {
                    wham_endpoint: format!("{}/usage", server.uri()),
                    auth_base_url: server.uri(),
                    ..Default::default()
                },
                store,
            )
            .unwrap(),
        ));
        let windows = native.fetch().await.unwrap();
        assert_unknown_first(&windows);
        assert_eq!(windows[0].duration_seconds, Some(18000));
    }
}

#[tokio::test]
async fn claude_native_percentage_anomalies_do_not_remove_windows() {
    use providers::provider::claude::{ClaudeTokenBundle, InMemoryClaudeTokenStore};
    for used in [
        json!(-0.1),
        json!(100.1),
        json!("NaN"),
        json!("inf"),
        json!("-inf"),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/oauth/usage"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "five_hour": {"utilization": used, "resets_at": "2033-05-18T03:33:20Z"},
                "seven_day": {"utilization": 25}
            })))
            .expect(1)
            .mount(&server)
            .await;
        let store = Arc::new(InMemoryClaudeTokenStore::new());
        store
            .save(&ClaudeTokenBundle {
                access_token: "fixture-access".into(),
                refresh_token: "fixture-refresh".into(),
                expires_at: u64::MAX,
                account_id: None,
                email: None,
                org_id: None,
                org_name: None,
            })
            .unwrap();
        let mut native = Native::Claude(
            ClaudeQuotaClient::new(&server.uri(), Duration::from_secs(10)).unwrap(),
            ClaudeOAuthClient::new(ClaudeOAuthConfig {
                token_url: format!("{}/token", server.uri()),
                ..Default::default()
            })
            .unwrap(),
            store,
        );
        assert_unknown_first(&native.fetch().await.unwrap());
    }
}

#[tokio::test]
async fn cursor_native_percentage_anomalies_keep_legacy_and_unified_windows() {
    use providers::provider::cursor::{CursorTokenBundle, InMemoryCursorTokenStore};
    for used in [
        json!(-0.1),
        json!(100.1),
        json!("NaN"),
        json!("inf"),
        json!("-inf"),
    ] {
        for unified in [false, true] {
            let server = MockServer::start().await;
            let body = if unified {
                json!({"membershipType": "unified_pro", "individualUsage": {
                    "plan": {"limit": 100, "totalPercentUsed": used},
                    "onDemand": {"limit": 100, "used": 25}
                }})
            } else {
                json!({"billingCycleEnd": 2000000000, "individualUsage": {
                    "plan": {"autoPercentUsed": used, "apiPercentUsed": 25}
                }})
            };
            Mock::given(method("GET"))
                .and(path("/summary"))
                .respond_with(ResponseTemplate::new(200).set_body_json(body))
                .expect(1)
                .mount(&server)
                .await;
            Mock::given(method("GET"))
                .and(path("/auth/usage"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
                .expect(1)
                .mount(&server)
                .await;
            if unified {
                Mock::given(method("POST"))
                    .and(path("/aiserver.v1.DashboardService/GetPlanInfo"))
                    .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                        "planInfo": {"includedUsageResetsAt": 2000000000}
                    })))
                    .expect(1)
                    .mount(&server)
                    .await;
            }
            let store = Arc::new(InMemoryCursorTokenStore::new());
            store
                .save(&CursorTokenBundle {
                    access_token: "fixture-access".into(),
                    refresh_token: "fixture-refresh".into(),
                    expires_at: u64::MAX,
                    api_key: None,
                    account_id: Some("fixture-account".into()),
                    email: None,
                })
                .unwrap();
            let mut native = Native::Cursor(
                CursorQuotaClient::new(CursorQuotaConfig {
                    base_url: server.uri(),
                    summary_url: format!("{}/summary", server.uri()),
                })
                .unwrap(),
                CursorOAuthClient::new(CursorOAuthConfig {
                    login_url: server.uri(),
                    poll_url: server.uri(),
                    token_url: server.uri(),
                    api_key_url: server.uri(),
                    profile_url: server.uri(),
                })
                .unwrap(),
                store,
            );
            assert_unknown_first(&native.fetch().await.unwrap());
        }
    }
}

async fn kimi_windows(body: Value) -> Result<Vec<QuotaWindow>, QuotaIssue> {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/usages"))
        .and(header("authorization", "Bearer fixture-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(sandbox::FileCredentialStore::open(dir.path()).unwrap());
    store
        .set("fixture", &"fixture-key".to_owned().into())
        .unwrap();
    let mut native = Native::Kimi(
        KimiQuotaClient::new(&server.uri(), Duration::from_secs(10)).unwrap(),
        CredentialRefConfig::Keyring {
            service: "evorch".into(),
            account: "fixture".into(),
        },
        store,
    );
    native.fetch().await
}

#[tokio::test]
async fn kimi_native_percentage_anomalies_keep_named_and_legacy_windows() {
    for used in [
        json!(-0.1),
        json!(100.1),
        json!("NaN"),
        json!("inf"),
        json!("-inf"),
    ] {
        let named = kimi_windows(json!({"usages": {
            "limit_5h": {"used_ratio": used, "reset_time": "2033-05-18T03:33:20Z"},
            "limit_7d": {"used_ratio": 25}
        }}))
        .await
        .unwrap();
        assert_unknown_first(&named);
        let legacy = kimi_windows(json!({"limits": [
            {"window": {"duration": 5, "timeUnit": "TIME_UNIT_HOUR"},
             "detail": {"limit": 100, "used": used, "resetTime": "2033-05-18T03:33:20Z"}},
            {"window": {"duration": 7, "timeUnit": "TIME_UNIT_DAY"},
             "detail": {"limit": 100, "used": 25}}
        ]}))
        .await
        .unwrap();
        assert_unknown_first(&legacy);
    }
    // Do not broaden tolerance to malformed reset metadata or contradictory counts.
    for malformed in [
        json!({"usages": {"limit_5h": {"used_ratio": -1, "reset_time": "invalid"}}}),
        json!({"usage": {"limit": 100, "used": 10, "remaining": 20}}),
    ] {
        assert_eq!(
            kimi_windows(malformed).await.unwrap_err().code,
            QuotaIssueCode::Protocol
        );
    }
}
