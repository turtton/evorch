use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use providers::provider::codex::oauth::{
    DEVICE_VERIFICATION_URL, DeviceAuthClient, UserCodeResponse,
};
use providers::provider::codex::tokens::TokenBundle;
use serde_json::json;
use wiremock::matchers::{body_json, method, path};
use wiremock::{Mock, MockServer};

use crate::support::{fixture, json_response};

pub fn make_dummy_jwt(account_id: &str) -> String {
    let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"none"}"#);
    let exp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time is after epoch")
        .as_secs()
        + 3_600;
    let payload = URL_SAFE_NO_PAD.encode(
        json!({
            "exp": exp,
            "https://api.openai.com/auth": { "chatgpt_account_id": account_id }
        })
        .to_string(),
    );
    format!("{header}.{payload}.sig")
}

pub fn client(server: &MockServer) -> DeviceAuthClient {
    let _ = crate::support::sse_response;
    let _ = crate::support::next_usage_event;
    let _ = crate::support::next_provider_event;
    let _ = crate::support::next_event;
    DeviceAuthClient::new(server.uri(), reqwest::Client::new())
}

pub fn current_tokens() -> TokenBundle {
    TokenBundle {
        access_token: "access-old".to_string(),
        refresh_token: "old-refresh".to_string(),
        id_token: make_dummy_jwt("acct-old"),
    }
}

pub fn user_code_response() -> UserCodeResponse {
    UserCodeResponse {
        device_auth_id: "da-1".to_string(),
        user_code: "ABCD-1234".to_string(),
        interval: Duration::from_secs(5),
        verification_url: DEVICE_VERIFICATION_URL,
    }
}

pub async fn mount_pending(server: &MockServer) {
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
        .mount(server)
        .await;
}
