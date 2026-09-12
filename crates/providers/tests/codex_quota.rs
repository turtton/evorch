use std::{sync::Arc, time::Duration};

#[path = "codex_quota/auth.rs"]
mod auth;
#[path = "codex_quota/compat.rs"]
mod compat;
#[path = "codex_quota/process.rs"]
mod process;

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use providers::provider::codex::{
    quota::{CodexQuotaClient, QuotaConfig, QuotaError, QuotaSource},
    tokens::{CodexTokenStore, InMemoryTokenStore, TokenBundle},
};
use serde_json::json;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{header, method, path},
};

fn store() -> Arc<InMemoryTokenStore> {
    let store = Arc::new(InMemoryTokenStore::new());
    store
        .save(&TokenBundle {
            access_token: "test-access".into(),
            refresh_token: "test-refresh".into(),
            id_token: format!("e30.{}.sig", URL_SAFE_NO_PAD.encode(serde_json::to_vec(&json!({
            "exp": 4000000000_u64, "https://api.openai.com/auth": {"chatgpt_account_id":"account-1"}
        })).unwrap())),
        })
        .unwrap();
    store
}

fn missing_config(endpoint: String) -> QuotaConfig {
    QuotaConfig {
        app_server_program: "/nonexistent/evorch-codex-quota".into(),
        wham_endpoint: endpoint,
        ..QuotaConfig::default()
    }
}

fn usage() -> serde_json::Value {
    json!({"plan_type":"plus", "rate_limit": {
        "primary_window":{"used_percent":25.0,"limit_window_seconds":18000,"reset_at":2000000000},
        "secondary_window":{"used_percent":40.0,"limit_window_seconds":604800,"reset_at":2000600000}
    }, "code_review_rate_limit":{"used_percent":10.0,"limit_window_seconds":604800,"reset_at":2000600000}})
}

// A real child process validates request order and emits newline-delimited RPC.
fn fixture_config(endpoint: String) -> QuotaConfig {
    let script = r#"
IFS= read -r r
case "$r" in *'"method":"initialize"'*'"name":"evorch"'*) ;; *) exit 1;; esac
printf '%s\n' '{"id":1,"result":{"userAgent":"fixture"}}'
IFS= read -r r
case "$r" in *'"id":'*) exit 2;; *'"method":"initialized"'*) ;; *) exit 3;; esac
IFS= read -r r
case "$r" in *'"method":"account/read"'*'"refreshToken":false'*) ;; *) exit 4;; esac
printf '%s\n' '{"id":2,"result":{"account":{"type":"chatgpt","planType":"plus"}}}'
IFS= read -r r
case "$r" in *'"method":"account/rateLimits/read"'*) ;; *) exit 5;; esac
printf '%s\n' '{"method":"account/rateLimits/updated","params":{}}' '{"id":999,"result":{}}'
printf '%s\n' '{"id":3,"result":{"rateLimits":{"primary":{"usedPercent":25.0,"windowDurationMins":300,"resetsAt":2000000000},"secondary":{"usedPercent":40.0,"windowDurationMins":10080,"resetsAt":2000600000}}}}'
IFS= read -r r
"#;
    QuotaConfig {
        app_server_program: "sh".into(),
        app_server_args: vec!["-c".into(), script.into()],
        wham_endpoint: endpoint,
        ..QuotaConfig::default()
    }
}

#[tokio::test]
async fn rate_limits_read_parses_stdio_jsonrpc_response() {
    // Given: a protocol-checking fixture and an HTTP endpoint that must not be used.
    let server = MockServer::start().await;
    let mut client = CodexQuotaClient::new(fixture_config(server.uri()), store()).unwrap();
    // When
    let snapshot = client.fetch_quota().await.unwrap();
    // Then
    assert_eq!(snapshot.source, QuotaSource::AppServer);
    assert!(!snapshot.stale);
    assert_eq!(snapshot.quota.plan.as_deref(), Some("plus"));
    let primary = snapshot.quota.primary.unwrap();
    assert_eq!(primary.used_percent, 25.0);
    assert_eq!(primary.remaining_percent, 75.0);
    assert_eq!(primary.window_duration, Duration::from_secs(18000));
    assert_eq!(primary.resets_at.timestamp(), 2000000000);
    assert_eq!(
        snapshot.quota.secondary.unwrap().window_duration,
        Duration::from_secs(604800)
    );
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn wham_fallback_when_app_server_missing() {
    // Given
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/usage"))
        .and(header("authorization", "Bearer test-access"))
        .and(header("chatgpt-account-id", "account-1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(usage()))
        .expect(1)
        .mount(&server)
        .await;
    let mut client =
        CodexQuotaClient::new(missing_config(format!("{}/usage", server.uri())), store()).unwrap();
    // When
    let snapshot = client.fetch_quota().await.unwrap();
    // Then
    assert_eq!(snapshot.source, QuotaSource::Wham);
    assert_eq!(snapshot.quota.primary.unwrap().remaining_percent, 75.0);
    assert_eq!(snapshot.quota.code_review.unwrap().remaining_percent, 90.0);
}

#[tokio::test]
async fn stale_snapshot_served_after_failure() {
    // Given: prime the cache, then move past its refresh deadline without sleeping.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(usage()))
        .mount(&server)
        .await;
    let mut client = CodexQuotaClient::new(missing_config(server.uri()), store()).unwrap();
    let first = client.fetch_quota().await.unwrap();
    server.reset().await;
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(61)).await;
    tokio::time::resume();
    // When
    let stale = client.fetch_quota().await.unwrap();
    // Then
    assert!(stale.stale);
    assert_eq!(stale.quota, first.quota);
    assert_eq!(stale.fetched_at, first.fetched_at);
    assert!(matches!(stale.last_error, Some(QuotaError::Sources { .. })));
    assert!(client.fetch_quota().await.unwrap().stale);
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[test]
fn poll_backoff_clamped_30s_to_10min() {
    // Given/When/Then: configuration boundaries and saturating exponential backoff.
    for (seconds, expected) in [
        (0, 30),
        (29, 30),
        (60, 60),
        (600, 600),
        (601, 600),
        (u64::MAX, 600),
    ] {
        let config = QuotaConfig {
            refresh_interval: Duration::from_secs(seconds),
            ..QuotaConfig::default()
        };
        let client = CodexQuotaClient::new(config, store()).unwrap();
        assert_eq!(client.poll_interval(), Duration::from_secs(expected));
    }
    assert_eq!(
        QuotaConfig::default().retry_delay(0),
        Duration::from_secs(60)
    );
    assert_eq!(
        QuotaConfig::default().retry_delay(1),
        Duration::from_secs(120)
    );
    assert_eq!(
        QuotaConfig::default().retry_delay(u32::MAX),
        Duration::from_secs(600)
    );
}

#[tokio::test]
async fn rpc_timeout_and_error_fall_back_without_leaking_server_messages() {
    // Given: timeout, malformed JSON, RPC error, and EOF fixtures.
    let cases = [
        ("IFS= read -r r; IFS= read -r r", "timeout"),
        ("printf '%s\\n' invalid", "protocol"),
        (
            "printf '%s\\n' '{\"id\":1,\"error\":{\"code\":-32000,\"message\":\"SECRET\"}}'",
            "rpc",
        ),
        ("exit 1", "protocol"),
    ];
    for (script, kind) in cases {
        let server = MockServer::start().await;
        let config = QuotaConfig {
            app_server_program: "sh".into(),
            app_server_args: vec!["-c".into(), script.into()],
            timeout: Duration::from_millis(100),
            wham_endpoint: server.uri(),
            ..QuotaConfig::default()
        };
        let mut client = CodexQuotaClient::new(config, store()).unwrap();
        // When
        let error = client.fetch_quota().await.unwrap_err();
        // Then
        assert!(!format!("{error:?}").contains("SECRET"));
        let QuotaError::Sources { app_server, wham } = error else {
            panic!("expected both sources")
        };
        assert!(matches!(*wham, QuotaError::HttpStatus(404)));
        match kind {
            "timeout" => assert!(matches!(*app_server, QuotaError::Timeout)),
            "rpc" => assert!(matches!(*app_server, QuotaError::Rpc(-32000))),
            _ => assert!(matches!(*app_server, QuotaError::Protocol(_))),
        }
    }
}

#[tokio::test]
async fn fresh_cache_avoids_both_transports() {
    // Given
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(usage()))
        .expect(1)
        .mount(&server)
        .await;
    let mut client = CodexQuotaClient::new(missing_config(server.uri()), store()).unwrap();
    let first = client.fetch_quota().await.unwrap();
    // When
    let second = client.fetch_quota().await.unwrap();
    // Then
    assert_eq!(second.quota, first.quota);
    assert_eq!(second.fetched_at, first.fetched_at);
    assert!(!second.stale);
}
