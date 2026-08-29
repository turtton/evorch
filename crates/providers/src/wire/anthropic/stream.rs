use serde::{Deserialize, Serialize};

use crate::error::ProviderError;
use crate::message::{FinishReason, Usage};
use crate::sse::SseFrame;
use crate::stream::StreamEvent;

use super::convert::{from_wire_usage, to_finish_reason};
use super::types::WireUsage;

/// Anthropic Messages API の SSE フレームを canonical 差分へ変換する状態機械。
#[derive(Debug, Clone, Default)]
pub struct AnthropicStreamInterpreter {
    message_id: Option<String>,
    model: Option<String>,
    usage: Usage,
    finish_reason: Option<FinishReason>,
    done: bool,
}

impl AnthropicStreamInterpreter {
    /// 空の interpreter を生成します。
    pub fn new() -> Self {
        Self::default()
    }

    /// 1 つの SSE フレームを 0 個以上の canonical 差分へ変換します。
    ///
    /// `event:` と JSON の `type` の両方を受け入れ、`event:` が無い場合は JSON 側で
    /// dispatch する。未知イベントは [`ProviderError::InvalidSse`]、Anthropic の
    /// `error` イベントはストリーム内エラーとして HTTP 400 に写像する。
    ///
    /// # Errors
    /// JSON が不正なら [`ProviderError::InvalidJson`]、イベント形状や種別が不正なら
    /// [`ProviderError::InvalidSse`]、`error` イベントなら [`ProviderError::Http`] を返す。
    pub fn interpret(&mut self, frame: &SseFrame) -> Result<Vec<StreamEvent>, ProviderError> {
        let value: serde_json::Value =
            serde_json::from_str(&frame.data).map_err(|error| ProviderError::InvalidJson {
                detail: error.to_string(),
            })?;
        let json_type = value.get("type").and_then(serde_json::Value::as_str);
        let event_type =
            frame
                .event
                .as_deref()
                .or(json_type)
                .ok_or_else(|| ProviderError::InvalidSse {
                    detail: "event 名と JSON type の両方がありません".to_string(),
                })?;
        match event_type {
            "message_start" => {
                let event: MessageStart = parse(value)?;
                self.message_id = event.message.id;
                self.model = event.message.model;
                self.usage = from_wire_usage(event.message.usage);
                Ok(Vec::new())
            }
            "content_block_start" => {
                let event: ContentBlockStart = parse(value)?;
                match event.content_block {
                    StartBlock::ToolUse { id, name } => Ok(vec![StreamEvent::ToolCallDelta {
                        index: event.index,
                        id: Some(id),
                        name: Some(name),
                        arguments_delta: String::new(),
                    }]),
                    StartBlock::Text | StartBlock::Thinking => Ok(Vec::new()),
                }
            }
            "content_block_delta" => {
                let event: ContentBlockDelta = parse(value)?;
                Ok(vec![match event.delta {
                    Delta::Text { text } => StreamEvent::TextDelta { text },
                    Delta::Thinking { thinking } => StreamEvent::ReasoningDelta { text: thinking },
                    Delta::InputJson { partial_json } => StreamEvent::ToolCallDelta {
                        index: event.index,
                        id: None,
                        name: None,
                        arguments_delta: partial_json,
                    },
                }])
            }
            "content_block_stop" | "ping" => Ok(Vec::new()),
            "message_delta" => {
                let event: MessageDelta = parse(value)?;
                self.finish_reason = Some(to_finish_reason(event.delta.stop_reason.as_deref()));
                self.usage.output_tokens = event.usage.output_tokens;
                Ok(Vec::new())
            }
            "message_stop" => {
                self.done = true;
                Ok(Vec::new())
            }
            "error" => {
                let event: ErrorEvent = parse(value)?;
                Err(ProviderError::Http {
                    status: 400,
                    body: format!("{}: {}", event.error.kind, event.error.message),
                })
            }
            unknown => Err(ProviderError::InvalidSse {
                detail: format!("未知の Anthropic SSE event: {unknown}"),
            }),
        }
    }

    /// `message_stop` を受信済みなら `true` を返します。
    pub const fn is_done(&self) -> bool {
        self.done
    }

    /// `message_start` で取得したメッセージ識別子を返します。
    pub fn message_id(&self) -> Option<&str> {
        self.message_id.as_deref()
    }

    /// `message_start` で取得したモデル識別子を返します。
    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    /// 累積 usage と終了理由を取り出し、内部の結果状態を既定値へ戻します。
    ///
    /// 未受信フィールドは usage では 0、終了理由では [`FinishReason::Stop`] とする。
    pub fn take_result(&mut self) -> (Usage, FinishReason) {
        (
            std::mem::take(&mut self.usage),
            self.finish_reason.take().unwrap_or(FinishReason::Stop),
        )
    }
}

fn parse<T: for<'de> Deserialize<'de>>(value: serde_json::Value) -> Result<T, ProviderError> {
    serde_json::from_value(value).map_err(|error| ProviderError::InvalidSse {
        detail: error.to_string(),
    })
}

#[derive(Serialize, Deserialize)]
struct MessageStart {
    message: StartMessage,
}

#[derive(Serialize, Deserialize)]
struct StartMessage {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    usage: WireUsage,
}

#[derive(Serialize, Deserialize)]
struct ContentBlockStart {
    index: usize,
    content_block: StartBlock,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum StartBlock {
    Text,
    Thinking,
    ToolUse { id: String, name: String },
}

#[derive(Serialize, Deserialize)]
struct ContentBlockDelta {
    index: usize,
    delta: Delta,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Delta {
    #[serde(rename = "text_delta")]
    Text { text: String },
    #[serde(rename = "thinking_delta")]
    Thinking { thinking: String },
    #[serde(rename = "input_json_delta")]
    InputJson { partial_json: String },
}

#[derive(Serialize, Deserialize)]
struct MessageDelta {
    delta: StopDelta,
    usage: DeltaUsage,
}

#[derive(Serialize, Deserialize)]
struct StopDelta {
    #[serde(default)]
    stop_reason: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct DeltaUsage {
    #[serde(default)]
    output_tokens: u64,
}

#[derive(Serialize, Deserialize)]
struct ErrorEvent {
    error: ErrorDetail,
}

#[derive(Serialize, Deserialize)]
struct ErrorDetail {
    #[serde(rename = "type")]
    kind: String,
    message: String,
}
