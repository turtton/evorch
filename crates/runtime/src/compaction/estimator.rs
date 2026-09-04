use providers::{Message, Usage};

const CHARS_PER_TOKEN: u64 = 4;

/// Estimates visible context using the conservative chars/4 heuristic used by
/// opencode/senpi (and mirrored by runtime rule budgeting's four bytes/token).
pub(crate) fn estimate_tokens(messages: &[Message]) -> u64 {
    if messages.is_empty() {
        return 0;
    }
    let serialized_chars = serde_json::to_string(messages).map_or(u64::MAX, |serialized| {
        u64::try_from(serialized.chars().count()).unwrap_or(u64::MAX)
    });
    serialized_chars.saturating_add(CHARS_PER_TOKEN - 1) / CHARS_PER_TOKEN
}

pub(crate) fn estimate_visible(messages: &[Message], last_usage: Option<&Usage>) -> u64 {
    let usage_tokens = last_usage.map_or(0, |usage| {
        usage
            .input_tokens
            .saturating_add(usage.cache_read_tokens)
            .saturating_add(usage.output_tokens)
    });
    estimate_tokens(messages).max(usage_tokens)
}

#[cfg(test)]
mod tests {
    use providers::{ContentBlock, Message, Role, Usage};
    use serde_json::json;

    use super::{estimate_tokens, estimate_visible};

    fn message(role: Role, content: Vec<ContentBlock>) -> Message {
        Message { role, content }
    }

    // Given: 4 文字の平文メッセージ / When: トークン数を推定 / Then: serialized representation の 4 文字単位切り上げになる
    #[test]
    fn estimates_serialized_plain_text() {
        let messages = [message(
            Role::User,
            vec![ContentBlock::Text {
                text: "abcd".to_string(),
            }],
        )];

        assert_eq!(estimate_tokens(&messages), 15);
    }

    // Given: JSON 引数を持つ tool call / When: トークン数を推定 / Then: tool 名と serialized JSON 引数が推定量へ含まれる
    #[test]
    fn tool_call_estimate_counts_name_and_json_input() {
        let small = [message(
            Role::Assistant,
            vec![ContentBlock::ToolUse {
                id: "call-1".to_string(),
                name: "read".to_string(),
                input: json!({}),
            }],
        )];
        let large = [message(
            Role::Assistant,
            vec![ContentBlock::ToolUse {
                id: "call-1".to_string(),
                name: "read_a_very_long_file".to_string(),
                input: json!({ "path": "/a/considerably/longer/path/to/input.txt" }),
            }],
        )];

        assert!(estimate_tokens(&large) > estimate_tokens(&small));
    }

    // Given: 空の履歴 / When: トークン数を推定 / Then: 0 を返す
    #[test]
    fn empty_messages_estimate_zero() {
        assert_eq!(estimate_tokens(&[]), 0);
    }

    // Given: 推定値より大きい provider usage / When: visible token 数を計算 / Then: usage 合計を下限として使う
    #[test]
    fn provider_usage_wins_when_larger_than_estimate() {
        let messages = [message(
            Role::User,
            vec![ContentBlock::Text {
                text: "short".to_string(),
            }],
        )];
        let usage = Usage {
            input_tokens: 100,
            output_tokens: 30,
            cache_read_tokens: 20,
            cache_write_tokens: u64::MAX,
        };

        assert_eq!(estimate_visible(&messages, Some(&usage)), 150);
    }
}
