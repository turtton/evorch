//! Turn a rejected Claude response body into a short, credential-free detail.
//!
//! The body is attacker-influenced text (it can echo request content), so only
//! the string `error.type` and `error.message` fields are ever shown. Each is
//! masked, flattened to one line and bounded here, at the source, because the
//! agent loop forwards `ProviderError::to_string()` to logs and run reasons.

use event_bus::REQUEST_FAILED_DETAIL_MAX_BYTES;
use secret_guard::SecretRedactor;
use serde_json::Value;

/// Shown when the body carries nothing safe to surface.
pub(super) const FALLBACK: &str = "Claude request rejected";
const MASK: &str = "***";
const ELLIPSIS: &str = "…";
/// Keeps `error.type` from crowding out `error.message`.
const TYPE_MAX_BYTES: usize = 64;
/// Room for the `http error (status NNN): ` prefix added by `Display` so the
/// whole text still fits the failure-event detail limit.
const DISPLAY_PREFIX_RESERVE: usize = 48;
const BODY_MAX_BYTES: usize = REQUEST_FAILED_DETAIL_MAX_BYTES - DISPLAY_PREFIX_RESERVE;

/// `Claude request rejected: <type>: <message>`, or [`FALLBACK`] when neither
/// field is a usable string.
///
/// `secret` is the credential in use for this request. It need not be in the
/// environment, so it is masked explicitly in addition to the shape-based and
/// environment-based [`SecretRedactor`].
pub(super) fn rejection_detail(value: &Value, secret: &str) -> String {
    let Some(error) = value.get("error").unwrap_or(value).as_object() else {
        return FALLBACK.into();
    };
    let field = |name: &str| error.get(name).and_then(Value::as_str);
    let kind = field("type").and_then(|text| clean(text, secret, TYPE_MAX_BYTES));
    let prefix_len = FALLBACK.len() + 2 + kind.as_ref().map_or(0, |kind| kind.len() + 2);
    let message = field("message").and_then(|text| {
        let limit = BODY_MAX_BYTES.saturating_sub(prefix_len);
        clean(text, secret, limit)
    });
    let mut detail = FALLBACK.to_owned();
    for part in [kind, message].into_iter().flatten() {
        detail.push_str(": ");
        detail.push_str(&part);
    }
    // A final pass over the joined text covers a credential split across fields.
    let detail = mask(&detail, secret);
    truncate(&detail, BODY_MAX_BYTES)
}

fn clean(text: &str, secret: &str, limit: usize) -> Option<String> {
    let flat = flatten(&mask(text, secret));
    (!flat.is_empty()).then(|| truncate(&flat, limit))
}

fn mask(text: &str, secret: &str) -> String {
    let text = if secret.is_empty() {
        text.to_owned()
    } else {
        text.replace(secret, MASK)
    };
    SecretRedactor::from_env().redact(&text).text
}

/// One line: control characters become spaces, invisible/bidi formatting
/// characters are dropped and runs of whitespace collapse.
fn flatten(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut pending_space = false;
    for character in text.chars() {
        if is_invisible_format(character) {
            continue;
        }
        if character.is_control() || character.is_whitespace() {
            pending_space = !out.is_empty();
            continue;
        }
        if pending_space {
            out.push(' ');
            pending_space = false;
        }
        out.push(character);
    }
    out
}

fn is_invisible_format(character: char) -> bool {
    matches!(
        character,
        '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{2069}' | '\u{FEFF}'
    )
}

/// Cut at a UTF-8 boundary so the result, including the ellipsis, fits `limit`.
fn truncate(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_owned();
    }
    let mut end = limit.saturating_sub(ELLIPSIS.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{ELLIPSIS}", text[..end].trim_end())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const TOKEN: &str = "tok-short";

    fn body(error_type: &str, message: &str) -> Value {
        json!({"type": "error", "error": {"type": error_type, "message": message}})
    }

    // Given: Claude の 400 本文 / When: 詳細を作る / Then: type と message が表出する
    #[test]
    fn type_and_message_are_shown() {
        let value = body(
            "invalid_request_error",
            "messages.1.content.0.tool_use.input: Input should be an object",
        );

        assert_eq!(
            rejection_detail(&value, TOKEN),
            "Claude request rejected: invalid_request_error: messages.1.content.0.tool_use.input: Input should be an object"
        );
    }

    // Given: 使用中 token (環境変数にない・短い) を含む message / When: 詳細を作る / Then: token が表出しない
    #[test]
    fn in_use_token_is_masked_even_when_short() {
        let value = body("invalid_request_error", &format!("bad {TOKEN} and {TOKEN}"));

        let detail = rejection_detail(&value, TOKEN);

        assert!(!detail.contains(TOKEN), "{detail}");
        assert!(detail.contains("bad *** and ***"), "{detail}");
    }

    // Given: 既知の credential 形式を含む message / When: 詳細を作る / Then: 形式ベースで伏せられる
    #[test]
    fn credential_shaped_text_is_masked() {
        let key = format!("sk-ant-api03-{}", "A1b2C3d4".repeat(8));
        let value = body("authentication_error", &format!("invalid key {key}"));

        let detail = rejection_detail(&value, TOKEN);

        assert!(!detail.contains(&key), "{detail}");
    }

    // Given: 改行・制御文字・bidi 制御を含む値 / When: 詳細を作る / Then: 1 行へ整形される
    #[test]
    fn control_characters_are_flattened() {
        let value = body(
            "invalid\u{0}_request",
            "line1\r\nline2\u{1b}[31m\t\u{202E}red\u{7f}   end\n",
        );

        let detail = rejection_detail(&value, TOKEN);

        assert_eq!(
            detail,
            "Claude request rejected: invalid _request: line1 line2 [31m red end"
        );
        assert!(!detail.chars().any(char::is_control));
    }

    // Given: 非常に長い type と多バイト message / When: 詳細を作る / Then: 境界を守り message が type に押し出されない
    #[test]
    fn long_unicode_is_bounded_and_type_cannot_push_message_out() {
        let value = body(&"t".repeat(5000), &"上限".repeat(5000));

        let detail = rejection_detail(&value, TOKEN);

        assert!(detail.len() <= BODY_MAX_BYTES, "{}", detail.len());
        assert!(detail.ends_with(ELLIPSIS));
        let message = detail.split(": ").nth(2).expect("message part");
        assert!(message.starts_with("上限上限"), "{detail}");
        assert!(message.len() > 500);
    }

    // Given: Display 接頭辞付きの全文 / When: 詳細を作る / Then: failure event の上限以内に収まる
    #[test]
    fn display_text_fits_failure_detail_limit() {
        let value = body("invalid_request_error", &"x".repeat(5000));

        let text = crate::ProviderError::Http {
            status: 400,
            body: rejection_detail(&value, TOKEN),
        }
        .to_string();

        assert!(
            text.len() <= REQUEST_FAILED_DETAIL_MAX_BYTES,
            "{}",
            text.len()
        );
    }

    // Given: 文字列でない / 欠落した type・message / When: 詳細を作る / Then: 固定文言へ fallback し raw 値を出さない
    #[test]
    fn malformed_shapes_fall_back_to_fixed_text() {
        for value in [
            json!(null),
            json!("raw body secret"),
            json!([1, 2]),
            json!({"error": "raw string secret"}),
            json!({"error": {"type": 7, "message": {"nested": "secret"}}}),
            json!({"error": {"type": "  ", "message": "\u{0}\n"}}),
            json!({"error": {}}),
        ] {
            let detail = rejection_detail(&value, TOKEN);
            assert_eq!(detail, FALLBACK, "{value}");
        }
    }

    // Given: type だけ / message だけ / When: 詳細を作る / Then: ある方だけを表出する
    #[test]
    fn partial_fields_are_shown() {
        assert_eq!(
            rejection_detail(&json!({"error": {"type": "overloaded_error"}}), TOKEN),
            "Claude request rejected: overloaded_error"
        );
        assert_eq!(
            rejection_detail(&json!({"error": {"message": "nope"}}), TOKEN),
            "Claude request rejected: nope"
        );
    }

    // Given: 他の field に secret を持つ body / When: 詳細を作る / Then: 他 field は表出しない
    #[test]
    fn other_fields_are_never_shown() {
        let value = json!({"error": {"type": "e", "message": "m", "param": "leak-field"},
            "request_id": "leak-id", "extra": "leak-extra"});

        let detail = rejection_detail(&value, TOKEN);

        assert!(!detail.contains("leak"), "{detail}");
    }
}
