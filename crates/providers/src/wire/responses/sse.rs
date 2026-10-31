mod custom;
mod failure;
mod ordering;

use std::collections::BTreeMap;

use custom::CustomToolState;

use serde::Deserialize;

use crate::ResponsesDialect;
use crate::error::ProviderError;
use crate::http::stream::{FrameInterpretation, WireStreamInterpreter};
use crate::message::{ContentBlock, FinishReason, Message, Usage};
use crate::sse::SseFrame;
use crate::stream::StreamEvent;

/// Responses API の SSE フレームを canonical 差分へ変換する状態機械。
///
/// Codex backend と公式 OpenAI API で共有します。
#[derive(Debug, Clone)]
pub struct ResponsesStreamInterpreter {
    origin: Option<crate::ResponsesReplayOrigin>,
    /// 失敗イベントの分類規則を選ぶ。`new()` は公式 OpenAI、`with_origin` は scope に従う。
    dialect: ResponsesDialect,
    ordering: ordering::OutputOrdering,
    tool_indices: BTreeMap<String, usize>,
    tool_arguments: BTreeMap<usize, String>,
    custom_tools: BTreeMap<String, CustomToolState>,
    saw_tool_call: bool,
    discard_tool_calls: bool,
    /// 終端 response が usage を持っていたか。欠落時は呼び出し側が使用量を発行しない。
    usage_reported: bool,
    done: bool,
}

impl ResponsesStreamInterpreter {
    /// 空の interpreter を生成します。
    #[must_use]
    pub fn new() -> Self {
        Self::with_dialect(None, ResponsesDialect::OpenAi)
    }

    /// Bind confirmed items to the scope used by this HTTP attempt.
    ///
    /// 失敗イベントの分類は `origin.dialect` に従います。
    pub fn with_origin(origin: crate::ResponsesReplayOrigin) -> Self {
        let dialect = origin.dialect;
        Self::with_dialect(Some(origin), dialect)
    }

    fn with_dialect(
        origin: Option<crate::ResponsesReplayOrigin>,
        dialect: ResponsesDialect,
    ) -> Self {
        Self {
            origin,
            dialect,
            ordering: ordering::OutputOrdering::default(),
            tool_indices: BTreeMap::new(),
            tool_arguments: BTreeMap::new(),
            custom_tools: BTreeMap::new(),
            saw_tool_call: false,
            discard_tool_calls: false,
            usage_reported: true,
            done: false,
        }
    }

    fn tool_index(&mut self, item_id: &str, output_index: usize) -> usize {
        *self
            .tool_indices
            .entry(item_id.to_owned())
            .or_insert(output_index)
    }

    fn custom_tool(&mut self, item_id: &str, output_index: usize) -> &mut CustomToolState {
        self.saw_tool_call = true;
        let index = self.tool_index(item_id, output_index);
        self.custom_tools
            .entry(item_id.to_owned())
            .or_insert_with(|| CustomToolState::new(index))
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
                    "Responses tool arguments の確定値が受信済み差分と一致しません (output_index={index})"
                ),
            })?
            .to_owned();
        accumulated.push_str(&suffix);
        Ok(suffix)
    }

    /// 終端 response の usage を canonical へ写す。
    ///
    /// Codex backend は usage を省略しうる (null / キー欠落)。欠落を黙ってゼロと
    /// 報告せず、`usage_reported` を落として呼び出し側に使用量の未報告を伝える。
    /// 返す `Usage::default()` は実測値ではなく、未報告時の代替値に過ぎない。
    fn terminal_usage(
        &mut self,
        event: &str,
        response_id: Option<&str>,
        reason: Option<&str>,
        usage: Option<ResponseUsage>,
    ) -> Usage {
        self.usage_reported = usage.is_some();
        if usage.is_none() {
            tracing::warn!(
                event,
                response_id,
                reason,
                "Responses terminal event に usage がありません。使用量は未報告として扱います"
            );
        }
        usage.map(Usage::from).unwrap_or_default()
    }

    /// tool 呼び出しの有無で自然終了の理由を決めます。
    fn natural_finish_reason(&self) -> FinishReason {
        if self.saw_tool_call {
            FinishReason::ToolUse
        } else {
            FinishReason::Stop
        }
    }
}

impl Default for ResponsesStreamInterpreter {
    fn default() -> Self {
        Self::new()
    }
}

impl WireStreamInterpreter for ResponsesStreamInterpreter {
    fn interpret(&mut self, frame: SseFrame) -> Result<FrameInterpretation, ProviderError> {
        if self.done || frame.data.trim() == "[DONE]" {
            self.done = true;
            return Ok(FrameInterpretation::default());
        }
        let value: serde_json::Value =
            serde_json::from_str(&frame.data).map_err(|error| ProviderError::InvalidJson {
                detail: format!("Responses event の解析に失敗しました: {error}"),
            })?;
        let json_type = value.get("type").and_then(serde_json::Value::as_str);
        let event_type =
            frame
                .event
                .as_deref()
                .or(json_type)
                .ok_or_else(|| ProviderError::InvalidSse {
                    detail: "Responses event 名がありません".to_string(),
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
                self.ordering.text(&value)?;
                let event: TextDelta = parse(value)?;
                if event.delta.is_empty() {
                    return Ok(FrameInterpretation::default());
                }
                tracing::trace!(
                    target: "providers::responses::text_delta_consumed",
                    text_bytes = event.delta.len(),
                    "decoded Responses text delta"
                );
                Ok(FrameInterpretation {
                    events: vec![StreamEvent::TextDelta { text: event.delta }],
                    completion: None,
                })
            }
            "response.output_item.added" | "response.output_item.done" => {
                let done = event_type == "response.output_item.done";
                self.ordering.item(&value, done)?;
                if !matches!(
                    value["item"]["type"].as_str(),
                    Some("function_call" | "custom_tool_call")
                ) {
                    return Ok(FrameInterpretation::default());
                }
                let event: OutputItemEvent = parse(value)?;
                match event.item {
                    OutputItem::FunctionCall {
                        id,
                        call_id,
                        name,
                        arguments,
                    } => {
                        self.saw_tool_call = true;
                        let index = self.tool_index(&id, event.output_index);
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
                    OutputItem::CustomToolCall {
                        id,
                        call_id,
                        name,
                        input,
                    } => {
                        let event = self
                            .custom_tool(&id, event.output_index)
                            .item(call_id, name, &input, done)?;
                        Ok(FrameInterpretation {
                            events: vec![event],
                            completion: None,
                        })
                    }
                    OutputItem::Other => Ok(FrameInterpretation::default()),
                }
            }
            "response.function_call_arguments.delta" => {
                let event: FunctionArgumentsDelta = parse(value)?;
                let index = self.tool_index(&event.item_id, event.output_index);
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
                let index = self.tool_index(&event.item_id, event.output_index);
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
            "response.custom_tool_call_input.delta" => {
                let event: FunctionArgumentsDelta = parse(value)?;
                let event = self
                    .custom_tool(&event.item_id, event.output_index)
                    .delta(&event.delta)?;
                Ok(FrameInterpretation {
                    events: vec![event],
                    completion: None,
                })
            }
            "response.custom_tool_call_input.done" => {
                let event: CustomInputDone = parse(value)?;
                let event = self
                    .custom_tool(&event.item_id, event.output_index)
                    .done(&event.input)?;
                Ok(FrameInterpretation {
                    events: event.into_iter().collect(),
                    completion: None,
                })
            }
            "response.completed" => {
                for (item_id, state) in &self.custom_tools {
                    if state.is_open() {
                        tracing::warn!(%item_id, output_index = state.index,
                            "Responses custom tool input が未完了のため JSON を閉じません");
                    }
                }
                let event: ResponseCompleted = parse(value)?;
                self.done = true;
                let usage = self.terminal_usage(
                    "response.completed",
                    event.response.id.as_deref(),
                    None,
                    event.response.usage,
                );
                Ok(FrameInterpretation {
                    events: Vec::new(),
                    completion: Some((usage, self.natural_finish_reason())),
                })
            }
            "response.incomplete" => {
                let event: ResponseIncomplete = parse(value)?;
                self.done = true;
                let reason = event
                    .response
                    .incomplete_details
                    .and_then(|details| details.reason);
                let usage = self.terminal_usage(
                    "response.incomplete",
                    event.response.id.as_deref(),
                    Some(reason.as_deref().unwrap_or("(missing)")),
                    event.response.usage,
                );
                // 未知・欠落の理由も打ち切りであり、正常終了 (Stop/ToolUse) として
                // runtime に採用させない。部分出力と usage は Other でも保持する。
                self.discard_tool_calls = !matches!(
                    reason.as_deref(),
                    Some("max_output_tokens" | "content_filter")
                );
                let finish_reason = match reason.as_deref() {
                    Some("max_output_tokens") => FinishReason::Length,
                    Some("content_filter") => FinishReason::ContentFilter,
                    Some(reason) => FinishReason::Other(format!("incomplete: {reason}")),
                    None => FinishReason::Other("incomplete".to_owned()),
                };
                Ok(FrameInterpretation {
                    events: Vec::new(),
                    completion: Some((usage, finish_reason)),
                })
            }
            "response.failed" => {
                self.done = true;
                Err(failure::Failure::from_failed_event(&value).into_error(self.dialect))
            }
            // 公式 API は stream 途中の失敗を response を伴わないトップレベル error で通知する
            "error" => {
                self.done = true;
                Err(failure::Failure::from_error_event(&value).into_error(self.dialect))
            }
            // backend が新イベントを追加しても既存 client を壊さないよう、未知イベントは無視する
            _ => Ok(FrameInterpretation::default()),
        }
    }

    fn finish(&mut self) -> Result<FrameInterpretation, ProviderError> {
        Ok(FrameInterpretation::default())
    }

    fn usage_reported(&self) -> bool {
        self.usage_reported
    }

    fn finalize_message(&mut self, message: &mut Message) {
        message
            .content
            .retain(|block| !matches!(block, ContentBlock::ToolUse { id, .. } if id.is_empty()));
        if self.discard_tool_calls {
            // Deltas already left this interpreter, so filter the assembled message,
            // not just the local maps. Text/reasoning and completion metadata survive.
            message.content.retain(|block| {
                if let ContentBlock::ToolUse { id, name, .. } = block {
                    tracing::warn!(
                        tool_call_id = %id,
                        tool_name = %name,
                        "discarded Responses tool call after unknown or missing incomplete reason"
                    );
                    false
                } else {
                    true
                }
            });
        }
        self.ordering.finalize(message, self.origin.as_ref());
    }
}

fn parse<T: for<'de> Deserialize<'de>>(value: serde_json::Value) -> Result<T, ProviderError> {
    serde_json::from_value(value).map_err(|error| ProviderError::InvalidSse {
        detail: format!("Responses event の形状が不正です: {error}"),
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

/// Typed tool parsing preserves existing argument validation. Native reasoning and
/// message ordering are captured separately from raw confirmed output items.
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum OutputItem {
    CustomToolCall {
        id: String,
        call_id: String,
        name: String,
        #[serde(default)]
        input: String,
    },
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
struct CustomInputDone {
    item_id: String,
    output_index: usize,
    input: String,
}

#[derive(Deserialize)]
struct ResponseCompleted {
    response: CompletedResponse,
}

#[derive(Deserialize)]
struct CompletedResponse {
    /// Codex backend は必須とするが、evorch は欠落しても応答を捨てない (ログ相関用)。
    #[serde(default)]
    id: Option<String>,
    /// Codex と同じく任意。`null` もキー欠落も未報告として扱う。
    #[serde(default)]
    usage: Option<ResponseUsage>,
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
            context_tokens: None,
            input_tokens: value.input_tokens,
            output_tokens: value.output_tokens,
            cache_read_tokens: value.input_tokens_details.cached_tokens,
            cache_write_tokens: 0,
            reasoning_tokens: value.output_tokens_details.reasoning_tokens,
        }
    }
}

#[derive(Deserialize)]
struct ResponseIncomplete {
    response: IncompleteResponse,
}

/// 打ち切り時は内訳が欠けても、受信済みの部分応答を失わないよう寛容に解釈する。
#[derive(Deserialize)]
struct IncompleteResponse {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    incomplete_details: Option<IncompleteDetails>,
    #[serde(default)]
    usage: Option<ResponseUsage>,
}

#[derive(Deserialize)]
struct IncompleteDetails {
    #[serde(default)]
    reason: Option<String>,
}

#[cfg(test)]
mod tests;
