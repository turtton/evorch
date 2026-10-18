use super::*;
use serde_json::{Value, json};

#[test]
fn compatible_client_uses_completion_limit_only_for_the_official_https_origin() {
    for (endpoint, modern) in [
        ("https://api.openai.com/v1", true),
        ("https://api.openai.com/v1/", true),
        ("https://API.OPENAI.COM:443/v1", true),
        ("http://api.openai.com/v1", false),
        ("https://api.openai.com:8443/v1", false),
        ("https://api.openai.com.evil/v1", false),
        ("https://api.openai.com@evil.test/v1", false),
        ("https://evil.test/api.openai.com/v1", false),
        ("http://localhost:8080/v1", false),
        ("https://api.kimi.com/coding/v1", false),
        ("invalid-url", false),
    ] {
        let client = OpenAiCompatibleClient::new(
            endpoint,
            "configured-profile",
            Duration::from_secs(60),
            None,
        )
        .unwrap();
        for model in ["o1", "gpt-4o"] {
            for streaming in [false, true] {
                for limit in [Some(128_000_u64), None] {
                    let request: ChatRequest = serde_json::from_value(json!({
                        "model": model, "max_tokens": limit,
                        "messages": [{"role": "user", "content": [{"type": "text", "text": "Hello"}]}]
                    })).unwrap();
                    // Inspect the exact shared conversion used immediately before HTTP send.
                    let body = serde_json::to_value(client.inner.wire_request(&request, streaming))
                        .unwrap();
                    let (field, absent) = if modern {
                        ("max_completion_tokens", "max_tokens")
                    } else {
                        ("max_tokens", "max_completion_tokens")
                    };
                    assert!(body.get(absent).is_none(), "{endpoint}: {body}");
                    assert_eq!(
                        body.get(field).and_then(Value::as_u64),
                        limit,
                        "{endpoint}: {body}"
                    );
                    if limit.is_none() {
                        assert!(body.get(field).is_none(), "{endpoint}: {body}");
                    }
                }
            }
        }
    }
}
