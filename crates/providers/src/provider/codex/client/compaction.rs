//! 公式 compaction 専用 SSE パス。通常生成の StreamEvent には暗号文を流さない。

use async_trait::async_trait;
use futures_util::StreamExt;
use serde::Deserialize;

use super::{CodexClient, PROVIDER_LABEL};
use crate::http::{UsageEmitter, map_request_error};
use crate::sse::{SseFrame, SseParser};
use crate::wire::codex::to_wire_compaction_request;
use crate::{ChatRequest, CompactionResult, Compactor, FinishReason, ProviderError, Usage};

#[async_trait]
impl Compactor for CodexClient {
    async fn compact(&self, request: &ChatRequest) -> Result<CompactionResult, ProviderError> {
        let wire = to_wire_compaction_request(request);
        let mut observer = self.observer(request, true);
        let result = async {
            // 専用パスにもリクエスト全体のタイムアウトを適用する。
            let response = self.send_response(&wire, false, &mut observer).await?;
            let mut stream = response.bytes_stream();
            let mut parser = SseParser::new();
            let mut compaction = CompactionStream::default();
            while let Some(chunk) = stream.next().await {
                let frames = parser
                    .feed(&chunk.map_err(map_request_error)?)
                    .map_err(|_| invalid("Codex compaction SSE の解析に失敗しました"))?;
                for frame in frames {
                    if let Some(result) = compaction.interpret(frame)? {
                        return Ok(result);
                    }
                }
            }
            for frame in parser
                .finish()
                .map_err(|_| invalid("Codex compaction SSE 終端が不正です"))?
            {
                if let Some(result) = compaction.interpret(frame)? {
                    return Ok(result);
                }
            }
            Err(invalid(
                "Codex compaction response ended without completion",
            ))
        }
        .await;
        match &result {
            Ok(result) => {
                UsageEmitter::new(self.event_bus.clone(), PROVIDER_LABEL)
                    .emit_usage(&request.model, &result.usage);
                observer.emit_completed(&result.usage, FinishReason::Stop);
            }
            Err(error) => observer.emit_failed(error),
        }
        result
    }
}

#[derive(Default)]
struct CompactionStream {
    encrypted_content: Option<String>,
}

impl CompactionStream {
    fn interpret(&mut self, frame: SseFrame) -> Result<Option<CompactionResult>, ProviderError> {
        if frame.data.trim() == "[DONE]" {
            return Err(invalid(
                "Codex compaction response ended without completion",
            ));
        }
        // サーバーの本文や暗号文を診断・UI へ漏らさない。
        let value: serde_json::Value = serde_json::from_str(&frame.data)
            .map_err(|_| invalid("Codex compaction event の JSON が不正です"))?;
        let event = frame
            .event
            .as_deref()
            .or_else(|| value["type"].as_str())
            .ok_or_else(|| invalid("Codex compaction event 名がありません"))?;
        match event {
            "response.output_item.done" if value["item"]["type"] == "compaction" => {
                let blob = value["item"]["encrypted_content"]
                    .as_str()
                    .filter(|blob| !blob.is_empty())
                    .ok_or_else(|| invalid("Codex compaction item に暗号化状態がありません"))?;
                if self.encrypted_content.is_some() {
                    return Err(invalid("Codex compaction item が重複しています"));
                }
                self.encrypted_content = Some(blob.to_owned());
            }
            "response.completed" => {
                let usage: ResponseUsage =
                    serde_json::from_value(value["response"]["usage"].clone())
                        .map_err(|_| invalid("Codex compaction usage が不正です"))?;
                return Ok(Some(CompactionResult {
                    encrypted_content: self
                        .encrypted_content
                        .take()
                        .ok_or_else(|| invalid("Codex response に compaction item がありません"))?,
                    usage: Usage {
                        input_tokens: usage.input_tokens,
                        output_tokens: usage.output_tokens,
                        cache_read_tokens: usage.input_tokens_details.cached_tokens,
                        cache_write_tokens: 0,
                    },
                }));
            }
            "response.failed" | "response.incomplete" | "error" => {
                return Err(invalid("Codex compaction が完了せず失敗しました"));
            }
            // message / reasoning / function_call などは通常生成として扱わない。
            _ => {}
        }
        Ok(None)
    }
}

fn invalid(detail: &str) -> ProviderError {
    ProviderError::InvalidSse {
        detail: detail.to_owned(),
    }
}

#[derive(Deserialize)]
struct ResponseUsage {
    input_tokens: u64,
    output_tokens: u64,
    #[serde(default)]
    input_tokens_details: InputTokenDetails,
}

#[derive(Deserialize, Default)]
struct InputTokenDetails {
    #[serde(default)]
    cached_tokens: u64,
}

#[cfg(test)]
mod tests;
