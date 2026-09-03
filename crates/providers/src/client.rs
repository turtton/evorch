//! provider クライアントの共通抽象を提供します。

use async_trait::async_trait;

use crate::auth::ProviderAuth;
use crate::error::ProviderError;
use crate::message::{ChatRequest, ChatResponse, ProviderCapabilities};
use crate::stream::DeltaStream;

/// 全プロバイダ実装が従うチャット完了クライアントの抽象。
///
/// 実装は `Box<dyn ProviderClient>` として扱えるよう dyn 互換でなければ
/// ならない (コンパイル時検証は本モジュール末尾の定数アサーションが担う)。
/// 認証情報は各メソッドの引数としてリクエストごとに注入され、
/// クライアントの状態として保持しない。
#[async_trait]
pub trait ProviderClient: Send + Sync {
    /// このクライアントが対応する機能フラグを返す。
    fn capabilities(&self) -> ProviderCapabilities;

    /// 非ストリーミングのチャット完了を送信する。
    ///
    /// # Errors
    /// リクエスト送信または応答解析に失敗した場合 [`ProviderError`] を返す。
    async fn send(
        &self,
        auth: &ProviderAuth,
        request: &ChatRequest,
    ) -> Result<ChatResponse, ProviderError>;

    /// ストリーミングのチャット完了を開始し、差分イベント列を返す。
    ///
    /// # Errors
    /// リクエスト送信に失敗した場合 [`ProviderError`] を返す。
    /// ストリーム途中の失敗は [`DeltaStream`] のアイテムとして通知される。
    async fn stream(
        &self,
        auth: &ProviderAuth,
        request: &ChatRequest,
    ) -> Result<DeltaStream, ProviderError>;
}

// dyn 互換性 (object safety) のコンパイル時検証。
// ProviderClient が dyn 互換でなくなった場合、`dyn ProviderClient` 型の
// 構築自体がコンパイルエラーとなる。
const _: () = {
    fn assert_dyn_compatible(_: &dyn ProviderClient) {}
    let _ = assert_dyn_compatible as fn(&dyn ProviderClient);
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{ContentBlock, FinishReason, Message, Role, Usage};
    use crate::stream::StreamEvent;
    use futures_util::StreamExt;

    /// 常に固定応答を返す偽クライアント。
    struct FakeClient {
        fail: bool,
    }

    impl FakeClient {
        fn succeeding() -> Self {
            Self { fail: false }
        }

        fn failing() -> Self {
            Self { fail: true }
        }
    }

    fn sample_response() -> ChatResponse {
        ChatResponse {
            message: Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: "応答".to_string(),
                }],
            },
            usage: Usage::default(),
            finish_reason: FinishReason::Stop,
        }
    }

    fn sample_request() -> ChatRequest {
        ChatRequest {
            model: "test-model".to_string(),
            messages: vec![Message {
                role: Role::User,
                content: vec![ContentBlock::Text {
                    text: "こんにちは".to_string(),
                }],
            }],
            tools: Vec::new(),
            temperature: None,
            max_tokens: None,
            observation: None,
        }
    }

    #[async_trait]
    impl ProviderClient for FakeClient {
        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                streaming: true,
                tool_use: false,
                reasoning: false,
            }
        }

        async fn send(
            &self,
            _auth: &ProviderAuth,
            _request: &ChatRequest,
        ) -> Result<ChatResponse, ProviderError> {
            if self.fail {
                Err(ProviderError::Timeout)
            } else {
                Ok(sample_response())
            }
        }

        async fn stream(
            &self,
            _auth: &ProviderAuth,
            _request: &ChatRequest,
        ) -> Result<DeltaStream, ProviderError> {
            let events: Vec<Result<StreamEvent, ProviderError>> = vec![
                Ok(StreamEvent::TextDelta {
                    text: "こ".to_string(),
                }),
                Ok(StreamEvent::Completed {
                    response: sample_response(),
                }),
            ];
            Ok(Box::pin(futures_util::stream::iter(events)))
        }
    }

    // Given: dyn ProviderClient として格納した偽クライアント / When: capabilities を呼ぶ / Then: 実装の値が動的ディスパッチで返る
    #[test]
    fn trait_object_dispatches_capabilities() {
        let client: Box<dyn ProviderClient> = Box::new(FakeClient::succeeding());

        assert_eq!(
            client.capabilities(),
            ProviderCapabilities {
                streaming: true,
                tool_use: false,
                reasoning: false
            }
        );
    }

    // Given: dyn ProviderClient / When: send を呼ぶ / Then: 固定応答が返る
    #[tokio::test]
    async fn trait_object_sends_chat_request() {
        let client: Box<dyn ProviderClient> = Box::new(FakeClient::succeeding());
        let auth = ProviderAuth::new("sk-test");

        let response = client
            .send(&auth, &sample_request())
            .await
            .expect("send は成功する");

        assert_eq!(response, sample_response());
    }

    // Given: 失敗する偽クライアント / When: send を呼ぶ / Then: エラーがそのまま伝播する
    #[tokio::test]
    async fn send_error_propagates_through_trait_object() {
        let client: Box<dyn ProviderClient> = Box::new(FakeClient::failing());
        let auth = ProviderAuth::new("sk-test");

        let err = client
            .send(&auth, &sample_request())
            .await
            .expect_err("send は失敗する");

        assert_eq!(err, ProviderError::Timeout);
    }

    // Given: dyn ProviderClient / When: stream を呼んで 1 イベント受信 / Then: 差分イベント列が得られる
    #[tokio::test]
    async fn trait_object_streams_delta_events() {
        let client: Box<dyn ProviderClient> = Box::new(FakeClient::succeeding());
        let auth = ProviderAuth::new("sk-test");
        let mut stream = client
            .stream(&auth, &sample_request())
            .await
            .expect("stream は成功する");

        let first = stream
            .next()
            .await
            .expect("イベントを受信できる")
            .expect("イベントは Ok");

        assert_eq!(
            first,
            StreamEvent::TextDelta {
                text: "こ".to_string()
            }
        );
    }
}
