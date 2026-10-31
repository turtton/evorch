//! OpenAI Codex Responses API の wire 形式を扱います。
//!
//! 変換と SSE 解釈は [`super::responses`] が担い、ここは Codex dialect を固定する
//! facade です。Codex backend での契約が未確定なため、canonical `max_tokens` は
//! `max_output_tokens` へ変換せず常に省略します。

mod sse;

use super::responses::{self, ResponsesDialect, ResponsesRequest, ResponsesRequestOptions};
use crate::message::ChatRequest;

pub use sse::CodexStreamInterpreter;

/// Codex Responses API に送信するリクエスト本文。
pub type CodexResponsesRequest = ResponsesRequest;

/// canonical request を Codex Responses API のリクエストへ変換します。
#[must_use]
pub fn to_wire_request(request: &ChatRequest) -> CodexResponsesRequest {
    responses::to_wire_request(
        request,
        &ResponsesRequestOptions {
            dialect: ResponsesDialect::Codex,
            prompt_cache_key: None,
            codex_model: None,
        },
    )
}

/// 公式 compaction を要求するトリガーを入力末尾に追加します。
#[must_use]
pub fn to_wire_compaction_request(request: &ChatRequest) -> CodexResponsesRequest {
    to_wire_request(request).with_compaction_trigger()
}

#[cfg(test)]
mod tests;
