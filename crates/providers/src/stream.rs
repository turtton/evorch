//! ストリーミング応答を扱います。

use std::collections::BTreeMap;

use futures_core::Stream;
use serde::{Deserialize, Serialize};

use crate::error::ProviderError;
use crate::message::{ChatResponse, ContentBlock, FinishReason, Message, Role, Usage};

/// ストリーミング途中に届く canonical 差分イベント。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamEvent {
    /// テキストの追記差分。
    TextDelta {
        /// 追加されるテキスト。
        text: String,
    },
    /// 思考 (reasoning) 内容の追記差分。
    ReasoningDelta {
        /// 追加される思考テキスト。
        text: String,
    },
    /// ツール呼び出し引数の追記差分。
    ///
    /// `id` と `name` はプロバイダが最初の断片でしか送信しないため `Option`。
    ToolCallDelta {
        /// ツール呼び出しの並び順インデックス。
        index: usize,
        /// 呼び出し識別子 (最初の断片にのみ現れる)。
        id: Option<String>,
        /// ツール名 (最初の断片にのみ現れる)。
        name: Option<String>,
        /// 引数 JSON への追記断片。
        arguments_delta: String,
    },
    /// ストリーム完了。累積済みの応答を伴う。
    Completed {
        /// 完全な応答。
        response: ChatResponse,
    },
}

/// canonical ストリーミングの差分イベント列。
pub type DeltaStream =
    std::pin::Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>;

/// [`StreamEvent`] の差分を累積し、最終応答を組み立てる状態機械。
#[derive(Debug, Clone, Default)]
pub struct StreamAccumulator {
    text: String,
    reasoning: String,
    tool_calls: BTreeMap<usize, ToolCallFragment>,
}

/// インデックスごとに合算したツール呼び出し断片。
///
/// `id` はどの断片でも送られなかった場合に備えて既定値を空文字列とする。
#[derive(Debug, Clone, Default, PartialEq)]
struct ToolCallFragment {
    id: String,
    name: String,
    arguments: String,
}

impl StreamAccumulator {
    /// 差分イベントを累積する。[`StreamEvent::Completed`] は無視される。
    pub fn feed(&mut self, event: &StreamEvent) {
        match event {
            StreamEvent::TextDelta { text } => self.text.push_str(text),
            StreamEvent::ReasoningDelta { text } => self.reasoning.push_str(text),
            StreamEvent::ToolCallDelta {
                index,
                id,
                name,
                arguments_delta,
            } => {
                let fragment = self.tool_calls.entry(*index).or_default();
                if let Some(id) = id {
                    fragment.id.clone_from(id);
                }
                if let Some(name) = name {
                    fragment.name.clone_from(name);
                }
                fragment.arguments.push_str(arguments_delta);
            }
            StreamEvent::Completed { .. } => {}
        }
    }

    /// 累積結果から最終応答を組み立てる。
    ///
    /// `role` は既定で [`Role::Assistant`]。内容ブロックは
    /// Reasoning → Text → ToolUse (index 昇順) の順で並ぶ。
    /// 引数が空のツール呼び出しは空オブジェクト入力として保持され、
    /// 引数文字列が JSON として解釈できない場合は `null` になる。
    pub fn finish(self, usage: Usage, finish_reason: FinishReason) -> ChatResponse {
        let mut content = Vec::new();
        if !self.reasoning.is_empty() {
            content.push(ContentBlock::Reasoning {
                text: self.reasoning,
            });
        }
        if !self.text.is_empty() {
            content.push(ContentBlock::Text { text: self.text });
        }
        for fragment in self.tool_calls.into_values() {
            let input = if fragment.arguments.trim().is_empty() {
                serde_json::Value::Object(serde_json::Map::new())
            } else {
                serde_json::from_str(&fragment.arguments).unwrap_or(serde_json::Value::Null)
            };
            content.push(ContentBlock::ToolUse {
                id: fragment.id,
                name: fragment.name,
                input,
            });
        }
        ChatResponse {
            message: Message {
                role: Role::Assistant,
                content,
            },
            usage,
            finish_reason,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::ContentBlock;
    use futures_util::StreamExt;
    use serde_json::json;

    fn usage() -> Usage {
        Usage {
            input_tokens: 1,
            output_tokens: 2,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
        }
    }

    fn text_delta(text: &str) -> StreamEvent {
        StreamEvent::TextDelta {
            text: text.to_string(),
        }
    }

    fn tool_delta(index: usize, id: Option<&str>, name: Option<&str>, args: &str) -> StreamEvent {
        StreamEvent::ToolCallDelta {
            index,
            id: id.map(str::to_string),
            name: name.map(str::to_string),
            arguments_delta: args.to_string(),
        }
    }

    fn tool_use(id: &str, name: &str, input: serde_json::Value) -> ContentBlock {
        ContentBlock::ToolUse {
            id: id.to_string(),
            name: name.to_string(),
            input,
        }
    }

    fn completed(finish_reason: FinishReason) -> StreamEvent {
        StreamEvent::Completed {
            response: ChatResponse {
                message: Message {
                    role: Role::Assistant,
                    content: Vec::new(),
                },
                usage: Usage::default(),
                finish_reason,
            },
        }
    }

    // Given: 空のアキュムレータ / When: TextDelta を 2 回 feed して finish / Then: 単一 Text ブロックに結合され Assistant 応答になる
    #[test]
    fn text_deltas_merge_into_single_text_block() {
        let mut accumulator = StreamAccumulator::default();

        accumulator.feed(&text_delta("こん"));
        accumulator.feed(&text_delta("ちは"));
        let response = accumulator.finish(usage(), FinishReason::Stop);

        assert_eq!(
            response.message.content,
            vec![ContentBlock::Text {
                text: "こんちは".to_string()
            }]
        );
        assert_eq!(response.message.role, Role::Assistant);
        assert_eq!(response.usage, usage());
        assert_eq!(response.finish_reason, FinishReason::Stop);
    }

    // Given: 2 つのツール呼び出し断片が交互に到着 / When: feed して finish / Then: index 昇順に引数が再組立される
    #[test]
    fn interleaved_tool_call_fragments_reassemble_by_index() {
        let mut accumulator = StreamAccumulator::default();

        accumulator.feed(&tool_delta(
            0,
            Some("call_0"),
            Some("get_weather"),
            "{\"city\":",
        ));
        accumulator.feed(&tool_delta(1, Some("call_1"), Some("get_time"), "{\"tz\":"));
        accumulator.feed(&tool_delta(0, None, None, "\"tokyo\"}"));
        accumulator.feed(&tool_delta(1, None, None, "\"jst\"}"));
        let response = accumulator.finish(usage(), FinishReason::ToolUse);

        assert_eq!(
            response.message.content,
            vec![
                tool_use("call_0", "get_weather", json!({ "city": "tokyo" })),
                tool_use("call_1", "get_time", json!({ "tz": "jst" })),
            ]
        );
        assert_eq!(response.finish_reason, FinishReason::ToolUse);
    }

    // Given: id 未送信・引数空のツール呼び出し / When: finish / Then: id は空文字列、input は空オブジェクトで保持される
    #[test]
    fn tool_call_without_id_and_arguments_is_preserved() {
        let mut accumulator = StreamAccumulator::default();

        accumulator.feed(&tool_delta(0, None, Some("ping"), ""));
        let response = accumulator.finish(usage(), FinishReason::ToolUse);

        assert_eq!(
            response.message.content,
            vec![tool_use("", "ping", json!({}))]
        );
    }

    // Given: ReasoningDelta と TextDelta / When: feed して finish / Then: Reasoning → Text の順にブロック化される
    #[test]
    fn reasoning_and_text_merge_in_block_order() {
        let mut accumulator = StreamAccumulator::default();

        accumulator.feed(&StreamEvent::ReasoningDelta {
            text: "考え".to_string(),
        });
        accumulator.feed(&StreamEvent::ReasoningDelta {
            text: "中".to_string(),
        });
        accumulator.feed(&text_delta("答え"));
        let response = accumulator.finish(usage(), FinishReason::Stop);

        assert_eq!(
            response.message.content,
            vec![
                ContentBlock::Reasoning {
                    text: "考え中".to_string()
                },
                ContentBlock::Text {
                    text: "答え".to_string()
                }
            ]
        );
    }

    // Given: Completed イベント / When: feed / Then: 無視され累積結果に影響しない
    #[test]
    fn completed_event_is_ignored_by_feed() {
        let mut accumulator = StreamAccumulator::default();

        accumulator.feed(&text_delta("a"));
        accumulator.feed(&completed(FinishReason::Length));
        let response = accumulator.finish(usage(), FinishReason::Stop);

        assert_eq!(
            response.message.content,
            vec![ContentBlock::Text {
                text: "a".to_string()
            }]
        );
        assert_eq!(response.finish_reason, FinishReason::Stop);
    }

    // Given: ToolCallDelta イベント / When: JSON 化して復元 / Then: type タグとフィールドが保存される
    #[test]
    fn stream_event_round_trips_with_type_tag() {
        let event = tool_delta(2, Some("c"), None, "{}");

        let json = serde_json::to_value(&event).unwrap();

        assert_eq!(json["type"], "tool_call_delta");
        assert_eq!(json["index"], 2);
        let restored: StreamEvent = serde_json::from_value(json).unwrap();
        assert_eq!(restored, event);
    }

    // Given: イベント列 / When: DeltaStream として収集 / Then: 同一列が得られる (Send 境界を満たす)
    #[tokio::test]
    async fn delta_stream_collects_events() {
        let events: Vec<Result<StreamEvent, ProviderError>> =
            vec![Ok(text_delta("hi")), Ok(completed(FinishReason::Stop))];

        let stream: DeltaStream = Box::pin(futures_util::stream::iter(events));
        let collected: Vec<StreamEvent> = stream
            .map(|result| result.expect("stream failed"))
            .collect()
            .await;

        assert_eq!(collected.len(), 2);
        assert_eq!(collected[0], text_delta("hi"));
    }
}
