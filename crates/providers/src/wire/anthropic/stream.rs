use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::Value;

use crate::error::ProviderError;
use crate::message::{FinishReason, Message, Role, Usage};
use crate::sse::SseFrame;
use crate::stream::StreamEvent;

use super::convert::{from_raw_block, from_wire_usage, to_finish_reason};
use super::types::{WireOutputTokensDetails, WireUsage, validate_block};

/// Anthropic Messages API の SSE フレームを canonical 差分へ変換する状態機械。
#[derive(Debug, Clone, Default)]
pub struct AnthropicStreamInterpreter {
    message_id: Option<String>,
    model: Option<String>,
    usage: WireUsage,
    finish_reason: Option<FinishReason>,
    blocks: BTreeMap<usize, BlockState>,
    done: bool,
}

#[derive(Debug, Clone)]
struct BlockState {
    block: Value,
    input_json: Option<String>,
}

impl AnthropicStreamInterpreter {
    /// 空の interpreter を生成します。
    pub fn new() -> Self {
        Self::default()
    }

    /// Interpret known events strictly, ignoring future event and delta types.
    /// Complete provider blocks are retained separately from display deltas.
    ///
    /// # Errors
    /// Invalid JSON, malformed known events, and API error events fail the stream.
    pub fn interpret(&mut self, frame: &SseFrame) -> Result<Vec<StreamEvent>, ProviderError> {
        let value: Value =
            serde_json::from_str(&frame.data).map_err(|error| ProviderError::InvalidJson {
                detail: error.to_string(),
            })?;
        let json_type = value.get("type").and_then(Value::as_str);
        let event_type = frame
            .event
            .as_deref()
            .or(json_type)
            .ok_or_else(|| invalid("event 名と JSON type の両方がありません"))?;
        match event_type {
            "message_start" => {
                let event: MessageStart = parse(value)?;
                self.message_id = event.message.id;
                self.model = event.message.model;
                self.usage = event.message.usage;
                Ok(Vec::new())
            }
            "content_block_start" => {
                let event: ContentBlockStart = parse(value)?;
                validate_block(&event.content_block).map_err(invalid)?;
                let events = match event.content_block["type"].as_str() {
                    Some("tool_use") => vec![StreamEvent::ToolCallDelta {
                        index: event.index,
                        id: event.content_block["id"].as_str().map(str::to_owned),
                        name: event.content_block["name"].as_str().map(str::to_owned),
                        arguments_delta: String::new(),
                    }],
                    Some("text") => nonempty_delta(&event.content_block, "text", false),
                    Some("thinking") => nonempty_delta(&event.content_block, "thinking", true),
                    _ => Vec::new(),
                };
                self.blocks.insert(
                    event.index,
                    BlockState {
                        block: event.content_block,
                        input_json: None,
                    },
                );
                Ok(events)
            }
            "content_block_delta" => {
                let event: ContentBlockDelta = parse(value)?;
                self.delta(event)
            }
            "content_block_stop" => {
                let event: ContentBlockStop = parse(value)?;
                if let Some(state) = self.blocks.get_mut(&event.index) {
                    finish_input(state);
                }
                Ok(Vec::new())
            }
            "ping" => Ok(Vec::new()),
            "message_delta" => {
                let event: MessageDelta = parse(value)?;
                if let Some(reason) = event.delta.stop_reason {
                    self.finish_reason = Some(to_finish_reason(Some(&reason)));
                }
                event.usage.apply(&mut self.usage);
                Ok(Vec::new())
            }
            "message_stop" => {
                for state in self.blocks.values_mut() {
                    finish_input(state);
                }
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
            unknown => {
                tracing::debug!(
                    event_type = unknown,
                    "未知の Anthropic stream event を無視します"
                );
                Ok(Vec::new())
            }
        }
    }

    fn delta(&mut self, event: ContentBlockDelta) -> Result<Vec<StreamEvent>, ProviderError> {
        let kind = event
            .delta
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("delta type must be a string"))?;
        let field = match kind {
            "text_delta" => "text",
            "thinking_delta" => "thinking",
            "signature_delta" => "signature",
            "input_json_delta" => "partial_json",
            "citations_delta" => "citation",
            "compaction_delta" => "content",
            unknown => {
                tracing::debug!(
                    delta_type = unknown,
                    "未知の Anthropic content delta を無視します"
                );
                return Ok(Vec::new());
            }
        };
        // Preserve the legacy tolerance of deltas arriving without a start frame.
        let state = self
            .blocks
            .entry(event.index)
            .or_insert_with(|| BlockState {
                block: match kind {
                    "text_delta" | "citations_delta" => {
                        serde_json::json!({"type":"text","text":""})
                    }
                    "thinking_delta" | "signature_delta" => {
                        serde_json::json!({"type":"thinking","thinking":""})
                    }
                    "compaction_delta" => serde_json::json!({"type":"compaction","content":""}),
                    _ => serde_json::json!({"type":"unknown"}),
                },
                input_json: None,
            });
        if kind == "citations_delta" {
            let citation = event
                .delta
                .get(field)
                .filter(|v| v.is_object())
                .ok_or_else(|| invalid("citations_delta.citation must be an object"))?;
            if state.block["type"] == "text" {
                if state.block.get("citations").is_none_or(Value::is_null) {
                    state.block["citations"] = serde_json::json!([]);
                }
                let citations = state.block["citations"]
                    .as_array_mut()
                    .ok_or_else(|| invalid("text.citations must be an array"))?;
                citations.push(citation.clone());
            }
            return Ok(Vec::new());
        }
        if kind == "compaction_delta" {
            for field in ["content", "encrypted_content"] {
                let value = event.delta.get(field).cloned().unwrap_or(Value::Null);
                if !value.is_null() && !value.is_string() {
                    return Err(invalid(format!(
                        "compaction_delta.{field} must be a string or null"
                    )));
                }
                state.block[field] = value;
            }
            return Ok(Vec::new());
        }
        let text = event
            .delta
            .get(field)
            .and_then(Value::as_str)
            .ok_or_else(|| invalid(format!("{kind}.{field} must be a string")))?;
        match kind {
            "signature_delta" => {
                // The SDK treats each signature delta as the current complete signature.
                state.block["signature"] = Value::String(text.into());
                Ok(Vec::new())
            }
            "input_json_delta" => {
                state.input_json.get_or_insert_default().push_str(text);
                if state.block["type"] == "tool_use" {
                    Ok(vec![StreamEvent::ToolCallDelta {
                        index: event.index,
                        id: None,
                        name: None,
                        arguments_delta: text.into(),
                    }])
                } else {
                    Ok(Vec::new())
                }
            }
            "text_delta" => {
                append_string(&mut state.block, "text", text)?;
                Ok(vec![StreamEvent::TextDelta { text: text.into() }])
            }
            "thinking_delta" => {
                append_string(&mut state.block, "thinking", text)?;
                Ok(vec![StreamEvent::ReasoningDelta { text: text.into() }])
            }
            _ => unreachable!(),
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

    /// Take the ordered complete blocks, without losing signatures or provider metadata.
    pub fn take_message(&mut self) -> Message {
        Message {
            role: Role::Assistant,
            content: std::mem::take(&mut self.blocks)
                .into_values()
                .flat_map(|state| from_raw_block(state.block))
                .collect(),
        }
    }

    /// 累積 usage と終了理由を取り出します。
    pub fn take_result(&mut self) -> (Usage, FinishReason) {
        (
            from_wire_usage(std::mem::take(&mut self.usage)),
            self.finish_reason.take().unwrap_or(FinishReason::Stop),
        )
    }
}

fn finish_input(state: &mut BlockState) {
    if let Some(input) = state
        .input_json
        .take()
        .filter(|text| !text.trim().is_empty())
    {
        // The runtime already rejects null tool arguments and the pump reports malformed JSON.
        state.block["input"] = serde_json::from_str(&input).unwrap_or(Value::Null);
    }
}

fn append_string(block: &mut Value, field: &str, text: &str) -> Result<(), ProviderError> {
    if block.get(field).is_none_or(Value::is_null) {
        block[field] = Value::String(String::new());
    }
    let Some(Value::String(current)) = block.get_mut(field) else {
        return Err(invalid(format!("block.{field} must be a string")));
    };
    current.push_str(text);
    Ok(())
}

fn nonempty_delta(block: &Value, field: &str, reasoning: bool) -> Vec<StreamEvent> {
    block[field]
        .as_str()
        .filter(|text| !text.is_empty())
        .map(|text| {
            if reasoning {
                StreamEvent::ReasoningDelta { text: text.into() }
            } else {
                StreamEvent::TextDelta { text: text.into() }
            }
        })
        .into_iter()
        .collect()
}

fn invalid(detail: impl Into<String>) -> ProviderError {
    ProviderError::InvalidSse {
        detail: detail.into(),
    }
}

fn parse<T: for<'de> Deserialize<'de>>(value: Value) -> Result<T, ProviderError> {
    serde_json::from_value(value).map_err(|error| invalid(error.to_string()))
}

#[derive(Deserialize)]
struct MessageStart {
    message: StartMessage,
}

#[derive(Deserialize)]
struct StartMessage {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    usage: WireUsage,
}

#[derive(Deserialize)]
struct ContentBlockStart {
    index: usize,
    content_block: Value,
}

#[derive(Deserialize)]
struct ContentBlockDelta {
    index: usize,
    delta: Value,
}

#[derive(Deserialize)]
struct ContentBlockStop {
    index: usize,
}

#[derive(Deserialize)]
struct MessageDelta {
    delta: StopDelta,
    #[serde(default)]
    usage: DeltaUsage,
}

#[derive(Deserialize)]
struct StopDelta {
    #[serde(default)]
    stop_reason: Option<String>,
}

#[derive(Default, Deserialize)]
struct DeltaUsage {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cache_creation_input_tokens: Option<u64>,
    cache_read_input_tokens: Option<u64>,
    output_tokens_details: Option<WireOutputTokensDetails>,
}

impl DeltaUsage {
    fn apply(self, usage: &mut WireUsage) {
        if let Some(value) = self.input_tokens {
            usage.input_tokens = value;
        }
        if let Some(value) = self.output_tokens {
            usage.output_tokens = value;
        }
        if let Some(value) = self.cache_creation_input_tokens {
            usage.cache_creation_input_tokens = Some(value);
        }
        if let Some(value) = self.cache_read_input_tokens {
            usage.cache_read_input_tokens = Some(value);
        }
        if let Some(value) = self.output_tokens_details {
            usage.output_tokens_details = Some(value);
        }
    }
}

#[derive(Deserialize)]
struct ErrorEvent {
    error: ErrorDetail,
}

#[derive(Deserialize)]
struct ErrorDetail {
    #[serde(rename = "type")]
    kind: String,
    message: String,
}
