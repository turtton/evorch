use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use providers::provider::codex::tokens::{CodexTokenStore, InMemoryTokenStore, TokenBundle};
use serde_json::json;
use wiremock::{Match, Request};

use crate::codex_support::MODEL;

/// セッション・ターン識別子の形式と同一リクエスト内の対応を検証する。
pub struct CodexIdMatcher;

impl Match for CodexIdMatcher {
    fn matches(&self, request: &Request) -> bool {
        ["session-id", "thread-id", "x-client-request-id"]
            .iter()
            .all(|name| {
                request.headers.get(*name).is_some_and(|value| {
                    let bytes = value.as_bytes();
                    bytes.len() == 36
                        && bytes.iter().enumerate().all(|(index, byte)| match index {
                            8 | 13 | 18 | 23 => *byte == b'-',
                            _ => matches!(byte, b'0'..=b'9' | b'a'..=b'f'),
                        })
                })
            })
            && request.headers.get("thread-id") == request.headers.get("x-client-request-id")
    }
}

#[derive(Debug)]
pub struct CodexBodyMatcher;

impl Match for CodexBodyMatcher {
    fn matches(&self, request: &Request) -> bool {
        let body: serde_json::Value = match serde_json::from_slice(&request.body) {
            Ok(body) => body,
            Err(_) => return false,
        };
        body["model"] == MODEL
            && body["store"] == false
            && body["stream"] == true
            && body.get("max_output_tokens").is_none()
            && body.get("service_tier").is_none()
            && body["tool_choice"] == "auto"
            && body["parallel_tool_calls"] == true
            && body["reasoning"].is_object()
            && body["include"].is_array()
            && body["instructions"].is_string()
            && body["input"].is_array()
    }
}

fn make_dummy_jwt(exp: u64, account_id: &str) -> String {
    let payload = json!({
        "exp": exp,
        "https://api.openai.com/auth": {"chatgpt_account_id": account_id}
    });
    format!(
        "e30.{}.signature",
        URL_SAFE_NO_PAD.encode(payload.to_string())
    )
}

pub fn seeded_store() -> Arc<InMemoryTokenStore> {
    let store = Arc::new(InMemoryTokenStore::new());
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time is after epoch")
        .as_secs();
    store
        .save(&TokenBundle {
            access_token: "access-tok-1".to_string(),
            refresh_token: "refresh-tok-1".to_string(),
            id_token: make_dummy_jwt(now + 3_600, "acc-123"),
        })
        .expect("token bundle can be seeded");
    store
}
