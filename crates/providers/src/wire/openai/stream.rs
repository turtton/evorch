use crate::error::ProviderError;
use crate::message::{FinishReason, Usage};
use crate::sse::SseFrame;
use crate::stream::StreamEvent;

use super::response::{to_finish_reason, to_usage};
use super::response_types::WireStreamChunk;

/// OpenAI Chat Completions の SSE chunk を canonical 差分へ変換する状態機械。
///
/// HTTP adapter は各 [`SseFrame`] を順番に [`Self::interpret`] へ渡し、返された
/// [`StreamEvent`] を配送します。`[DONE]` 到着後に [`Self::take_result`] から usage と
/// finish reason を取り出し、別途累積した差分と合わせて `StreamEvent::Completed` を
/// 構築します。
#[derive(Debug, Clone, Default)]
pub struct OpenAiStreamInterpreter {
    usage: Option<Usage>,
    finish_reason: Option<FinishReason>,
    done: bool,
}

impl OpenAiStreamInterpreter {
    /// 空の interpreter を生成します。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// SSE frame を 0 個以上の canonical 差分イベントへ変換します。
    ///
    /// usage-only chunk は内部へ保存し、`[DONE]` は完了状態だけを更新するため、
    /// いずれも空のイベント列を返します。
    ///
    /// # Errors
    /// frame の data が Chat Completions chunk JSON として解析できない場合に
    /// [`ProviderError::InvalidJson`] を返します。
    pub fn interpret(&mut self, frame: &SseFrame) -> Result<Vec<StreamEvent>, ProviderError> {
        if frame.data.trim() == "[DONE]" {
            self.done = true;
            return Ok(Vec::new());
        }
        let chunk: WireStreamChunk =
            serde_json::from_str(&frame.data).map_err(|error| ProviderError::InvalidJson {
                detail: format!("OpenAI stream chunk の解析に失敗しました: {error}"),
            })?;
        if chunk.choices.is_empty() && chunk.usage.is_none() {
            return Err(ProviderError::InvalidJson {
                detail: "OpenAI stream chunk に choices と usage のどちらもありません".to_string(),
            });
        }
        if let Some(usage) = chunk.usage.as_ref() {
            self.usage = Some(to_usage(usage));
        }
        let mut events = Vec::new();
        for choice in chunk.choices {
            if let Some(reason) = choice.finish_reason.as_deref() {
                self.finish_reason = Some(to_finish_reason(reason));
            }
            if let Some(text) = choice.delta.content
                && !text.is_empty()
            {
                events.push(StreamEvent::TextDelta { text });
            }
            events.extend(choice.delta.tool_calls.into_iter().map(|call| {
                StreamEvent::ToolCallDelta {
                    index: call.index,
                    id: call.id,
                    name: call.function.name,
                    arguments_delta: call.function.arguments,
                }
            }));
        }
        Ok(events)
    }

    /// `[DONE]` を受信済みかを返します。
    #[must_use]
    pub const fn is_done(&self) -> bool {
        self.done
    }

    /// 保存済みの usage と finish reason を取り出します。
    ///
    /// 呼び出すと保存値は `None` に戻ります。OpenAI は usage-only chunk を
    /// `[DONE]` より前に送るため、通常は [`Self::is_done`] が真になってから呼びます。
    pub fn take_result(&mut self) -> (Option<Usage>, Option<FinishReason>) {
        (self.usage.take(), self.finish_reason.take())
    }
}
