//! OpenAI 互換 provider 実装を提供します。

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use event_bus::EventBus;

use super::openai::{ChatCompletionsClient, ChatCompletionsConfig};
use crate::auth::ProviderAuth;
use crate::client::ProviderClient;
use crate::error::ProviderError;
use crate::http::stream::{FrameInterpretation, WireStreamInterpreter};
use crate::message::{ChatRequest, ChatResponse, FinishReason, ProviderCapabilities};
use crate::sse::SseFrame;
use crate::stream::DeltaStream;
use crate::wire::openai::OpenAiStreamInterpreter;

/// OpenAI Chat Completions 互換 API を呼び出す provider クライアント。
pub struct OpenAiCompatibleClient {
    inner: ChatCompletionsClient,
}

impl OpenAiCompatibleClient {
    /// 接続先とイベント用ラベルを指定して互換クライアントを構築する。
    ///
    /// reasoning 対応は接続先に依存するため、機能フラグは既定で無効にする。
    ///
    /// # Errors
    /// HTTP クライアントを構築できない場合 [`ProviderError`] を返す。
    pub fn new(
        base_url: impl Into<String>,
        provider_label: impl Into<String>,
        timeout: Duration,
        event_bus: Option<Arc<EventBus>>,
    ) -> Result<Self, ProviderError> {
        Ok(Self {
            inner: ChatCompletionsClient::new(ChatCompletionsConfig {
                base_url: base_url.into(),
                provider_label: provider_label.into(),
                timeout,
                event_bus,
            })?,
        })
    }
}

#[async_trait]
impl ProviderClient for OpenAiCompatibleClient {
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            streaming: true,
            tool_use: true,
            reasoning: false,
        }
    }

    async fn send(
        &self,
        auth: &ProviderAuth,
        request: &ChatRequest,
    ) -> Result<ChatResponse, ProviderError> {
        self.inner.send(auth, request).await
    }

    async fn stream(
        &self,
        auth: &ProviderAuth,
        request: &ChatRequest,
    ) -> Result<DeltaStream, ProviderError> {
        self.inner
            .stream(
                auth,
                request,
                OpenAiCompatibleInterpreterAdapter(OpenAiStreamInterpreter::new()),
            )
            .await
    }
}

struct OpenAiCompatibleInterpreterAdapter(OpenAiStreamInterpreter);

impl WireStreamInterpreter for OpenAiCompatibleInterpreterAdapter {
    fn interpret(&mut self, frame: SseFrame) -> Result<FrameInterpretation, ProviderError> {
        let events = self.0.interpret(&frame)?;
        let completion = if self.0.is_done() {
            let (usage, reason) = self.0.take_result();
            Some((
                usage.unwrap_or_default(),
                reason.unwrap_or(FinishReason::Stop),
            ))
        } else {
            None
        };
        Ok(FrameInterpretation { events, completion })
    }

    fn finish(&mut self) -> Result<FrameInterpretation, ProviderError> {
        Ok(FrameInterpretation::default())
    }
}
