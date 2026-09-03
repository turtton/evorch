#[path = "codex_oauth_contract/support.rs"]
mod oauth_support;
mod support;

use std::time::Duration;

use oauth_support::{client, current_tokens, make_dummy_jwt, mount_pending, user_code_response};
use providers::ProviderError;
use providers::provider::codex::oauth::{
    CODEX_CLIENT_ID, DEVICE_REDIRECT_URI, DEVICE_VERIFICATION_URL, PollOptions,
};
use serde_json::json;
use support::{fixture, json_response};
use wiremock::matchers::{body_json, body_string, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test(flavor = "multi_thread")]
async fn device_flow_issues_usercode_then_polls_to_token() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/accounts/deviceauth/usercode"))
        .and(body_json(json!({ "client_id": CODEX_CLIENT_ID })))
        .respond_with(json_response(
            200,
            &fixture("codex", "device_usercode_response.json"),
        ))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/accounts/deviceauth/token"))
        .and(body_json(json!({
            "device_auth_id": "da-1",
            "user_code": "ABCD-1234"
        })))
        .respond_with(json_response(
            403,
            &fixture("codex", "device_token_pending.json"),
        ))
        .up_to_n_times(2)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/accounts/deviceauth/token"))
        .and(body_json(json!({
            "device_auth_id": "da-1",
            "user_code": "ABCD-1234"
        })))
        .respond_with(json_response(
            200,
            r#"{"authorization_code":"code-1","code_challenge":"cc","code_verifier":"cv-srv"}"#,
        ))
        .expect(1)
        .mount(&server)
        .await;
    let id_token = make_dummy_jwt("acct-1");
    let exchange_body = format!(
        "grant_type=authorization_code&code=code-1&redirect_uri={}&client_id={CODEX_CLIENT_ID}&code_verifier=cv-srv",
        DEVICE_REDIRECT_URI.replace(':', "%3A").replace('/', "%2F")
    );
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .and(body_string(exchange_body))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id_token": id_token,
            "access_token": "access-tok-1",
            "refresh_token": "refresh-tok-1"
        })))
        .expect(1)
        .mount(&server)
        .await;

    let client = client(&server);
    let user_code = client.request_user_code().await.expect("user code");
    let code = client
        .poll_agent_code(
            &user_code,
            &PollOptions {
                interval_override: Some(Duration::from_millis(10)),
                timeout: Duration::from_millis(150),
            },
        )
        .await
        .expect("agent code");
    let tokens = client.exchange_code(&code).await.expect("token exchange");

    assert_eq!(user_code.verification_url, DEVICE_VERIFICATION_URL);
    assert_eq!(tokens.access_token, "access-tok-1");
    assert_eq!(tokens.refresh_token, "refresh-tok-1");
    assert_eq!(tokens.id_token, id_token);
}

#[tokio::test(flavor = "multi_thread")]
async fn device_poll_respects_interval_and_times_out() {
    let server = MockServer::start().await;
    mount_pending(&server).await;
    let response = user_code_response();

    let error = client(&server)
        .poll_agent_code(
            &response,
            &PollOptions {
                interval_override: Some(Duration::from_millis(10)),
                timeout: Duration::from_millis(120),
            },
        )
        .await
        .expect_err("poll times out");
    let count = server.received_requests().await.expect("requests").len();

    assert_eq!(error, ProviderError::Timeout);
    assert!((5..=20).contains(&count), "request count: {count}");
}

#[tokio::test(flavor = "multi_thread")]
async fn device_poll_abort_via_drop() {
    let server = MockServer::start().await;
    mount_pending(&server).await;
    let client = client(&server);
    let response = user_code_response();
    let options = PollOptions {
        interval_override: Some(Duration::from_millis(10)),
        timeout: Duration::from_secs(5),
    };

    tokio::select! {
        result = client.poll_agent_code(&response, &options) => panic!("poll ended: {result:?}"),
        () = tokio::time::sleep(Duration::from_millis(50)) => {}
    }
    let count_at_abort = server.received_requests().await.expect("requests").len();
    tokio::time::sleep(Duration::from_millis(100)).await;
    let count_after_wait = server.received_requests().await.expect("requests").len();

    assert!(count_at_abort >= 2);
    assert_eq!(count_after_wait, count_at_abort);
}

#[tokio::test(flavor = "multi_thread")]
async fn refresh_rotates_tokens() {
    let server = MockServer::start().await;
    let new_id_token = make_dummy_jwt("acct-new");
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .and(body_json(json!({
            "client_id": CODEX_CLIENT_ID,
            "grant_type": "refresh_token",
            "refresh_token": "old-refresh"
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "access-new",
            "refresh_token": "refresh-new",
            "id_token": new_id_token
        })))
        .expect(1)
        .mount(&server)
        .await;
    let rotated = client(&server)
        .refresh(&current_tokens())
        .await
        .expect("full rotation");
    assert_eq!(rotated.access_token, "access-new");
    assert_eq!(rotated.refresh_token, "refresh-new");
    assert_eq!(rotated.id_token, new_id_token);

    let partial_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "access-newer"
        })))
        .expect(1)
        .mount(&partial_server)
        .await;
    let current = current_tokens();
    let partial = client(&partial_server)
        .refresh(&current)
        .await
        .expect("partial rotation");
    assert_eq!(partial.access_token, "access-newer");
    assert_eq!(partial.refresh_token, current.refresh_token);
    assert_eq!(partial.id_token, current.id_token);
}

#[tokio::test(flavor = "multi_thread")]
async fn refresh_error_maps_to_provider_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(ResponseTemplate::new(401).set_body_string("expired refresh token"))
        .mount(&server)
        .await;

    let error = client(&server)
        .refresh(&current_tokens())
        .await
        .expect_err("refresh fails");

    assert!(
        matches!(error, ProviderError::Http { status: 401, body } if body.contains("expired refresh token"))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn exchange_rejects_server_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(ResponseTemplate::new(500).set_body_string("oauth unavailable"))
        .mount(&server)
        .await;
    let code = providers::provider::codex::oauth::AgentCodeBundle {
        authorization_code: "code-1".to_string(),
        code_verifier: "cv-srv".to_string(),
    };

    let error = client(&server)
        .exchange_code(&code)
        .await
        .expect_err("exchange fails");

    assert!(matches!(error, ProviderError::Http { status: 500, .. }));
}
