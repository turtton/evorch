use std::collections::BTreeMap;

use serde::Deserialize;

use crate::error::ProviderError;
use crate::http::stream::{FrameInterpretation, WireStreamInterpreter};
use crate::message::{FinishReason, Usage};
use crate::sse::SseFrame;
use crate::stream::StreamEvent;

/// Codex Responses API の SSE フレームを canonical 差分へ変換する状態機械。
#[derive(Debug, Clone, Default)]
pub struct CodexStreamInterpreter {
    tool_indices: BTreeMap<String, usize>,
    tool_arguments: BTreeMap<usize, String>,
    saw_tool_call: bool,
    done: bool,
}

impl CodexStreamInterpreter {
    /// 空の interpreter を生成します。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn final_arguments_delta(
        &mut self,
        index: usize,
        arguments: &str,
    ) -> Result<String, ProviderError> {
        let accumulated = self.tool_arguments.entry(index).or_default();
        // done イベントは差分でなく全文を含む。同じ全文を二重連結せず、
        // 未受信の末尾だけを補う。矛盾する全文から呼び出しを実行しない。
        let suffix = arguments
            .strip_prefix(accumulated.as_str())
            .ok_or_else(|| ProviderError::InvalidSse {
                detail: format!(
                    "Codex tool arguments の確定値が受信済み差分と一致しません (output_index={index})"
                ),
            })?
            .to_owned();
        accumulated.push_str(&suffix);
        Ok(suffix)
    }
}

impl WireStreamInterpreter for CodexStreamInterpreter {
    fn interpret(&mut self, frame: SseFrame) -> Result<FrameInterpretation, ProviderError> {
        if self.done || frame.data.trim() == "[DONE]" {
            self.done = true;
            return Ok(FrameInterpretation::default());
        }
        let value: serde_json::Value =
            serde_json::from_str(&frame.data).map_err(|error| ProviderError::InvalidJson {
                detail: format!("Codex Responses event の解析に失敗しました: {error}"),
            })?;
        let json_type = value.get("type").and_then(serde_json::Value::as_str);
        let event_type =
            frame
                .event
                .as_deref()
                .or(json_type)
                .ok_or_else(|| ProviderError::InvalidSse {
                    detail: "Codex Responses event 名がありません".to_string(),
                })?;
        match event_type {
            "response.created"
            | "response.output_text.done"
            | "response.content_part.added"
            | "response.reasoning_summary_part.added"
            | "response.reasoning_summary_part.done"
            | "response.reasoning_summary_text.done" => Ok(FrameInterpretation::default()),
            "response.reasoning_summary_text.delta" => {
                let event: TextDelta = parse(value)?;
                if event.delta.is_empty() {
                    return Ok(FrameInterpretation::default());
                }
                Ok(FrameInterpretation {
                    events: vec![StreamEvent::ReasoningDelta { text: event.delta }],
                    completion: None,
                })
            }
            "response.output_text.delta" => {
                let event: TextDelta = parse(value)?;
                if event.delta.is_empty() {
                    return Ok(FrameInterpretation::default());
                }
                Ok(FrameInterpretation {
                    events: vec![StreamEvent::TextDelta { text: event.delta }],
                    completion: None,
                })
            }
            "response.output_item.added" | "response.output_item.done" => {
                let event: OutputItemEvent = parse(value)?;
                match event.item {
                    OutputItem::FunctionCall {
                        id,
                        call_id,
                        name,
                        arguments,
                    } => {
                        self.saw_tool_call = true;
                        let index = *self.tool_indices.entry(id).or_insert(event.output_index);
                        let arguments_delta = self.final_arguments_delta(index, &arguments)?;
                        Ok(FrameInterpretation {
                            events: vec![StreamEvent::ToolCallDelta {
                                index,
                                id: Some(call_id),
                                name: Some(name),
                                arguments_delta,
                            }],
                            completion: None,
                        })
                    }
                    OutputItem::Other => Ok(FrameInterpretation::default()),
                }
            }
            "response.function_call_arguments.delta" => {
                let event: FunctionArgumentsDelta = parse(value)?;
                let index = self
                    .tool_indices
                    .get(&event.item_id)
                    .copied()
                    .unwrap_or(event.output_index);
                self.tool_arguments
                    .entry(index)
                    .or_default()
                    .push_str(&event.delta);
                Ok(FrameInterpretation {
                    events: vec![StreamEvent::ToolCallDelta {
                        index,
                        id: None,
                        name: None,
                        arguments_delta: event.delta,
                    }],
                    completion: None,
                })
            }
            "response.function_call_arguments.done" => {
                let event: FunctionArgumentsDone = parse(value)?;
                let index = self
                    .tool_indices
                    .get(&event.item_id)
                    .copied()
                    .unwrap_or(event.output_index);
                let arguments_delta = self.final_arguments_delta(index, &event.arguments)?;
                Ok(FrameInterpretation {
                    events: vec![StreamEvent::ToolCallDelta {
                        index,
                        id: None,
                        name: None,
                        arguments_delta,
                    }],
                    completion: None,
                })
            }
            "response.completed" => {
                let event: ResponseCompleted = parse(value)?;
                self.done = true;
                Ok(FrameInterpretation {
                    events: Vec::new(),
                    completion: Some((
                        event.response.usage.into(),
                        if self.saw_tool_call {
                            FinishReason::ToolUse
                        } else {
                            FinishReason::Stop
                        },
                    )),
                })
            }
            "response.failed" => {
                let event: ResponseFailed = parse(value)?;
                self.done = true;
                Err(ProviderError::Http {
                    status: 400,
                    body: format!(
                        "{}: {}",
                        event.response.error.code, event.response.error.message
                    ),
                })
            }
            // backend が新イベントを追加しても既存 client を壊さないよう、未知イベントは無視する
            _ => Ok(FrameInterpretation::default()),
        }
    }

    fn finish(&mut self) -> Result<FrameInterpretation, ProviderError> {
        Ok(FrameInterpretation::default())
    }
}

fn parse<T: for<'de> Deserialize<'de>>(value: serde_json::Value) -> Result<T, ProviderError> {
    serde_json::from_value(value).map_err(|error| ProviderError::InvalidSse {
        detail: format!("Codex Responses event の形状が不正です: {error}"),
    })
}

#[derive(Deserialize)]
struct TextDelta {
    delta: String,
}

#[derive(Deserialize)]
struct OutputItemEvent {
    output_index: usize,
    item: OutputItem,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum OutputItem {
    FunctionCall {
        id: String,
        call_id: String,
        name: String,
        #[serde(default)]
        arguments: String,
    },
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
struct FunctionArgumentsDelta {
    item_id: String,
    output_index: usize,
    delta: String,
}

#[derive(Deserialize)]
struct FunctionArgumentsDone {
    item_id: String,
    output_index: usize,
    arguments: String,
}

#[derive(Deserialize)]
struct ResponseCompleted {
    response: CompletedResponse,
}

#[derive(Deserialize)]
struct CompletedResponse {
    usage: ResponseUsage,
}

#[derive(Deserialize)]
struct ResponseUsage {
    input_tokens: u64,
    output_tokens: u64,
    #[serde(default)]
    input_tokens_details: InputTokenDetails,
    #[serde(default)]
    output_tokens_details: OutputTokenDetails,
}

#[derive(Deserialize, Default)]
struct InputTokenDetails {
    #[serde(default)]
    cached_tokens: u64,
}

#[derive(Deserialize, Default)]
struct OutputTokenDetails {
    #[serde(default)]
    reasoning_tokens: Option<u64>,
}

impl From<ResponseUsage> for Usage {
    fn from(value: ResponseUsage) -> Self {
        Self {
            input_tokens: value.input_tokens,
            output_tokens: value.output_tokens,
            cache_read_tokens: value.input_tokens_details.cached_tokens,
            cache_write_tokens: 0,
            reasoning_tokens: value.output_tokens_details.reasoning_tokens,
        }
    }
}

#[derive(Deserialize)]
struct ResponseFailed {
    response: FailedResponse,
}

#[derive(Deserialize)]
struct FailedResponse {
    error: ErrorDetail,
}

#[derive(Deserialize)]
struct ErrorDetail {
    code: String,
    message: String,
}

#[cfg(test)]
mod tests;
