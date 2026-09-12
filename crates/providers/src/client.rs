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

    /// transport 系失敗の bounded retry 方針 (issue #108、retry 責務は provider 層)。
    fn retry_policy(&self) -> crate::retry::RetryPolicy {
        crate::retry::RetryPolicy::default()
    }

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

    /// Delivers text/reasoning deltas live and returns the provider's accumulated response.
    /// Non-streaming clients retain their `send` behavior; premature EOF is an error.
    /// transport 系失敗と早期 EOF は既定で計3回まで指数 backoff で再試行する。
    /// 認証系は即失敗し、再試行対象の連敗時は `RetriesExhausted` を返す。
    async fn send_streaming(
        &self,
        auth: &ProviderAuth,
        request: &ChatRequest,
        bus: &event_bus::EventBus,
    ) -> Result<ChatResponse, ProviderError> {
        use crate::stream::StreamEvent;
        use event_bus::{Event, MessageEvent};
        use futures_util::StreamExt;

        if !self.capabilities().streaming {
            return self.send(auth, request).await;
        }
        let policy = self.retry_policy();
        // 状態は全てローカルに保持し、agent_loop が future を drop すれば待機・再試行も停止する。
        let mut dedup = crate::dedup::ReplayDeduper::new();
        let mut attempts_used = 0;
        loop {
            attempts_used += 1;
            let (error, premature_eof) = match self.stream(auth, request).await {
                Err(error) => (error, false),
                Ok(mut stream) => loop {
                    let event = match stream.next().await {
                        Some(Ok(event)) => event,
                        Some(Err(error)) => break (error, false),
                        None => {
                            break (
                                ProviderError::Request(
                                    "stream ended without completion signal".to_owned(),
                                ),
                                true,
                            );
                        }
                    };
                    let run_id = request
                        .observation
                        .as_ref()
                        .map(|context| context.run_id.clone());
                    match event {
                        StreamEvent::TextDelta { text } => {
                            if let Some(delta) = dedup.filter_text(&text) {
                                bus.emit(Event::new(MessageEvent::MessageDelta { delta, run_id }));
                            }
                        }
                        StreamEvent::ReasoningDelta { text } => {
                            if let Some(delta) = dedup.filter_reasoning(&text) {
                                bus.emit(Event::new(MessageEvent::ReasoningDelta {
                                    delta,
                                    run_id,
                                }));
                            }
                        }
                        StreamEvent::ToolCallDelta { .. } => {}
                        StreamEvent::Completed { response } => return Ok(response),
                    }
                },
            };
            if !premature_eof && !crate::retry::is_retryable(&error) {
                return Err(error);
            }
            if attempts_used >= policy.max_attempts {
                return Err(ProviderError::RetriesExhausted {
                    attempts: attempts_used,
                    last: Box::new(error),
                });
            }
            dedup.on_new_attempt();
            tokio::time::sleep(policy.backoff_for(attempts_used - 1)).await;
        }
    }
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
    mod divergence;

    use super::*;
    use crate::message::{ContentBlock, FinishReason, Message, Role, Usage};
    use crate::stream::StreamEvent;
    use event_bus::{EventBus, EventKind, EventReceiver, MessageEvent};
    use futures_util::{FutureExt, StreamExt};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    type Attempt = Result<Vec<Result<StreamEvent, ProviderError>>, ProviderError>;

    struct ScriptedClient {
        scripts: Vec<Attempt>,
        attempts: AtomicUsize,
    }

    impl ScriptedClient {
        fn new(scripts: Vec<Attempt>) -> Self {
            Self {
                scripts,
                attempts: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait]
    impl ProviderClient for ScriptedClient {
        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                streaming: true,
                tool_use: false,
                reasoning: true,
            }
        }

        fn retry_policy(&self) -> crate::retry::RetryPolicy {
            crate::retry::RetryPolicy {
                max_attempts: 3,
                initial_delay: Duration::ZERO,
                backoff_factor: 1,
                max_delay: Duration::ZERO,
            }
        }

        async fn send(
            &self,
            _: &ProviderAuth,
            _: &ChatRequest,
        ) -> Result<ChatResponse, ProviderError> {
            unreachable!("streaming クライアントは send を使わない")
        }

        async fn stream(
            &self,
            _: &ProviderAuth,
            _: &ChatRequest,
        ) -> Result<DeltaStream, ProviderError> {
            let attempt = self.attempts.fetch_add(1, Ordering::SeqCst);
            let events = self
                .scripts
                .get(attempt)
                .expect("試行上限を越えない")
                .clone()?;
            Ok(Box::pin(futures_util::stream::iter(events)))
        }
    }

    fn text(text: &str) -> Result<StreamEvent, ProviderError> {
        Ok(StreamEvent::TextDelta { text: text.into() })
    }

    fn completed() -> Result<StreamEvent, ProviderError> {
        Ok(StreamEvent::Completed {
            response: sample_response(),
        })
    }

    fn text_deltas(receiver: &mut EventReceiver) -> Vec<String> {
        let mut deltas = Vec::new();
        while let Some(event) = receiver.recv().now_or_never() {
            let event = event.expect("イベントを受信できる");
            match event.kind {
                EventKind::Message(MessageEvent::MessageDelta { delta, run_id }) => {
                    assert_eq!(run_id, None);
                    deltas.push(delta);
                }
                other => panic!("想定外のイベント: {other:?}"),
            }
        }
        deltas
    }

    #[tokio::test]
    async fn streaming_recovers_when_start_times_out() {
        // Given: 初回は Timeout、二回目は完全な応答。
        let client = ScriptedClient::new(vec![
            Err(ProviderError::Timeout),
            Ok(vec![text("hi"), completed()]),
        ]);
        let bus = EventBus::new(16);
        let mut receiver = bus.subscribe();
        // When: 共通のストリーミング経路で送信する。
        let result = client
            .send_streaming(&ProviderAuth::new("key"), &sample_request(), &bus)
            .await;
        // Then: 二回目の確定応答を返し、hi は一度だけ表示する。
        assert_eq!(result, Ok(sample_response()));
        assert_eq!(text_deltas(&mut receiver), ["hi"]);
        assert_eq!(client.attempts.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn streaming_deduplicates_when_premature_eof_is_replayed() {
        // Given: Hello 表示後に EOF、二回目は接頭辞から再生する。
        let client = ScriptedClient::new(vec![
            Ok(vec![text("Hello")]),
            Ok(vec![text("Hello"), text(" world"), completed()]),
        ]);
        let bus = EventBus::new(16);
        let mut receiver = bus.subscribe();
        // When: 共通のストリーミング経路で送信する。
        let result = client
            .send_streaming(&ProviderAuth::new("key"), &sample_request(), &bus)
            .await;
        // Then: 部分表示を撤回せず、再生された接頭辞だけ吸収する。
        assert_eq!(result, Ok(sample_response()));
        let deltas = text_deltas(&mut receiver);
        assert_eq!(deltas.concat(), "Hello world");
        assert_eq!(deltas, ["Hello", " world"]);
        assert_eq!(client.attempts.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn streaming_exhausts_when_every_attempt_times_out() {
        // Given: 全三回が Timeout。
        let client = ScriptedClient::new(vec![Err(ProviderError::Timeout); 3]);
        // When: 共通のストリーミング経路で送信する。
        let result = client
            .send_streaming(
                &ProviderAuth::new("key"),
                &sample_request(),
                &EventBus::new(16),
            )
            .await;
        // Then: 三回で停止し、最後の原因を保持する。
        assert_eq!(
            result,
            Err(ProviderError::RetriesExhausted {
                attempts: 3,
                last: Box::new(ProviderError::Timeout)
            })
        );
        assert_eq!(client.attempts.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn streaming_exhaustion_display_contains_attempts_and_cause() {
        // Given: 全三回が Timeout。
        let client = ScriptedClient::new(vec![Err(ProviderError::Timeout); 3]);
        // When: 実際に返されたエラーを GUI 向けに文字列化する。
        let error = client
            .send_streaming(
                &ProviderAuth::new("key"),
                &sample_request(),
                &EventBus::new(16),
            )
            .await
            .expect_err("連敗する");
        // Then: 試行回数と最終原因が含まれる。
        assert!(error.to_string().contains('3'), "{error}");
        assert!(
            error
                .to_string()
                .contains(&ProviderError::Timeout.to_string())
        );
    }

    #[tokio::test]
    async fn streaming_returns_original_error_when_auth_is_rejected() {
        // Given: 初回が認証エラー。
        let error = ProviderError::Http {
            status: 401,
            body: "unauthorized".into(),
        };
        let client = ScriptedClient::new(vec![Err(error.clone())]);
        // When: 共通のストリーミング経路で送信する。
        let result = client
            .send_streaming(
                &ProviderAuth::new("key"),
                &sample_request(),
                &EventBus::new(16),
            )
            .await;
        // Then: ラップも再試行もせず元のエラーを返す。
        assert_eq!(result, Err(error));
        assert_eq!(client.attempts.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn streaming_returns_original_error_when_item_is_invalid_json() {
        // Given: ストリーム内で JSON 解析エラーを受信する。
        let error = ProviderError::InvalidJson {
            detail: "invalid item".into(),
        };
        let client = ScriptedClient::new(vec![Ok(vec![Err(error.clone())])]);
        // When: 共通のストリーミング経路で送信する。
        let result = client
            .send_streaming(
                &ProviderAuth::new("key"),
                &sample_request(),
                &EventBus::new(16),
            )
            .await;
        // Then: ラップも再試行もせず元のエラーを返す。
        assert_eq!(result, Err(error));
        assert_eq!(client.attempts.load(Ordering::SeqCst), 1);
    }

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
