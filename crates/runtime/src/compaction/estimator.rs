use providers::{ContentBlock, Message, ToolResultContent, ToolSpec, Usage};

const BYTES_PER_TOKEN: u64 = 4;
/// Providers bill images by pixel area, not base64 length; this covers a
/// full-size image on the common vision APIs.
const IMAGE_TOKENS: u64 = 1600;

/// Estimate serialized UTF-8 bytes / 4. Using bytes avoids undercounting
/// non-ASCII tool output as severely as a character-count heuristic.
pub(crate) fn estimate_tokens(messages: &[Message]) -> u64 {
    if messages.is_empty() {
        return 0;
    }
    let serialized_bytes = serde_json::to_string(messages).map_or(u64::MAX, |serialized| {
        u64::try_from(serialized.len()).unwrap_or(u64::MAX)
    });
    let (image_bytes, images) = image_data(messages);
    let text_tokens = serialized_bytes
        .saturating_sub(image_bytes)
        .div_ceil(BYTES_PER_TOKEN);
    text_tokens.saturating_add(images * IMAGE_TOKENS)
}

fn image_data(messages: &[Message]) -> (u64, u64) {
    let data = messages
        .iter()
        .flat_map(|message| &message.content)
        .flat_map(|block| match block {
            ContentBlock::Image { data, .. } => vec![data],
            ContentBlock::ToolResult { content, .. } => content
                .iter()
                .filter_map(|part| match part {
                    ToolResultContent::Image { data, .. } => Some(data),
                    ToolResultContent::Text { .. } => None,
                })
                .collect(),
            _ => Vec::new(),
        });
    data.fold((0, 0), |(bytes, count), data| {
        (bytes + data.len() as u64, count + 1)
    })
}

/// Include tool definitions in the serialized estimate. Provider input usage already
/// includes them, so compare the two totals instead of adding schemas to usage.
pub(crate) fn estimate_request(
    messages: &[Message],
    tools: &[ToolSpec],
    last_usage: Option<&Usage>,
    baseline_estimate: Option<u64>,
) -> u64 {
    estimate_projected(messages, last_usage, baseline_estimate)
        .max(estimate_tokens(messages).saturating_add(estimate_tool_tokens(tools)))
}

pub(crate) fn estimate_tool_tokens(tools: &[ToolSpec]) -> u64 {
    if tools.is_empty() {
        return 0;
    }
    serde_json::to_vec(tools).map_or(u64::MAX, |bytes| (bytes.len() as u64).div_ceil(4))
}

/// Add newly appended tool results and injected messages to the last reported
/// usage. The baseline must be cleared when old history is replaced/compacted.
pub(crate) fn estimate_projected(
    messages: &[Message],
    last_usage: Option<&Usage>,
    baseline_estimate: Option<u64>,
) -> u64 {
    let current_estimate = estimate_tokens(messages);
    let usage_tokens = last_usage.map_or(0, |usage| {
        usage
            .input_tokens
            .saturating_add(usage.output_tokens)
            .saturating_add(
                baseline_estimate.map_or(0, |baseline| current_estimate.saturating_sub(baseline)),
            )
    });
    current_estimate.max(usage_tokens)
}

#[cfg(test)]
fn estimate_visible(messages: &[Message], last_usage: Option<&Usage>) -> u64 {
    estimate_projected(messages, last_usage, None)
}

#[cfg(test)]
mod tests {
    use providers::{ContentBlock, Message, Role, ToolResultContent, Usage};
    use serde_json::json;

    use super::{estimate_tokens, estimate_visible};

    fn message(role: Role, content: Vec<ContentBlock>) -> Message {
        Message { role, content }
    }

    #[test]
    fn new_tool_output_is_added_to_reported_usage() {
        let mut messages = vec![message(
            Role::User,
            vec![ContentBlock::Text {
                text: "短い".into(),
            }],
        )];
        let baseline = estimate_tokens(&messages);
        let usage = Usage {
            input_tokens: 1000,
            output_tokens: 100,
            cache_read_tokens: 900,
            ..Usage::default()
        };
        messages.push(message(
            Role::User,
            vec![ContentBlock::ToolResult {
                tool_call_id: "call-1".into(),
                content: vec![providers::ToolResultContent::Text {
                    text: "長い出力".repeat(100),
                }],
                is_error: false,
            }],
        ));
        assert_eq!(
            super::estimate_projected(&messages, Some(&usage), Some(baseline)),
            1100 + estimate_tokens(&messages) - baseline
        );
        // Once the history is replaced, neither old usage nor its baseline applies.
        assert_eq!(
            super::estimate_projected(&messages[..1], None, None),
            baseline
        );
    }

    #[test]
    fn schemas_count_once_when_combining_usage_and_serialized_estimates() {
        let messages = [message(
            Role::User,
            vec![ContentBlock::Text {
                text: "hello".into(),
            }],
        )];
        let tools = [providers::ToolSpec {
            name: "read".into(),
            description: "Read a file with bounded output".into(),
            input_schema: serde_json::json!({"type": "object", "properties": {"path": {"type": "string"}}}),
        }];
        let serialized = super::estimate_tokens(&messages) + super::estimate_tool_tokens(&tools);
        assert_eq!(
            super::estimate_request(&messages, &tools, None, None),
            serialized
        );
        let usage = Usage {
            input_tokens: serialized * 2,
            output_tokens: 100,
            ..Usage::default()
        };
        assert_eq!(
            super::estimate_request(
                &messages,
                &tools,
                Some(&usage),
                Some(super::estimate_tokens(&messages))
            ),
            usage.input_tokens + usage.output_tokens
        );
        assert_eq!(super::estimate_tool_tokens(&[]), 0);
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

    // Given: base64 サイズの異なる画像 / When: トークン数を推定 / Then: 画像は base64 長ではなく固定量で数える
    #[test]
    fn images_count_a_fixed_budget_regardless_of_encoded_size() {
        let image = |data: &str| {
            [message(
                Role::User,
                vec![ContentBlock::ToolResult {
                    tool_call_id: "call-1".to_string(),
                    content: vec![ToolResultContent::Image {
                        media_type: "image/png".to_string(),
                        data: data.to_string(),
                        source: None,
                    }],
                    is_error: false,
                }],
            )]
        };

        let small = estimate_tokens(&image("AAAA"));
        assert_eq!(small, estimate_tokens(&image(&"A".repeat(1_000_000))));
        assert!(small >= super::IMAGE_TOKENS);
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
            reasoning_tokens: None,
        };

        assert_eq!(estimate_visible(&messages, Some(&usage)), 130);
    }
}
