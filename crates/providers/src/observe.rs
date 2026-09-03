//! プロバイダリクエスト attempt の観測イベントを発行する。
// allow: SIZE_OK — observer 本体と指定された単体契約テストを同一モジュールに集約する。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::{SystemTime, UNIX_EPOCH};

use event_bus::{Event, EventBus, ProviderEvent, ProviderFailureKind};

use crate::error::ProviderError;
use crate::message::{FinishReason, ObservationContext, Usage};
use crate::stream::StreamEvent;

/// プロセス内で一意な request ID を生成する。
pub(crate) fn next_request_id() -> String {
    static PROCESS_STARTED_AT_MS: LazyLock<u128> = LazyLock::new(|| {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
    });
    static NEXT_REQUEST_NUMBER: AtomicU64 = AtomicU64::new(1);

    let request_number = NEXT_REQUEST_NUMBER.fetch_add(1, Ordering::Relaxed);
    format!("req-{}-{request_number}", *PROCESS_STARTED_AT_MS)
}

/// 1 回のプロバイダリクエスト attempt を観測する。
pub(crate) struct AttemptObserver {
    bus: Option<Arc<EventBus>>,
    request_id: String,
    provider: String,
    profile: Option<String>,
    protocol: &'static str,
    model: String,
    streaming: bool,
    observation: Option<ObservationContext>,
    started_at: tokio::time::Instant,
    started_emitted: bool,
    first_token_emitted: bool,
    terminal_emitted: bool,
}

impl AttemptObserver {
    /// attempt の観測器を生成する。
    pub(crate) fn new(
        bus: Option<Arc<EventBus>>,
        provider: impl Into<String>,
        profile: Option<String>,
        protocol: &'static str,
        model: impl Into<String>,
        streaming: bool,
        observation: Option<ObservationContext>,
    ) -> Self {
        Self {
            bus,
            request_id: next_request_id(),
            provider: provider.into(),
            profile,
            protocol,
            model: model.into(),
            streaming,
            observation,
            started_at: tokio::time::Instant::now(),
            started_emitted: false,
            first_token_emitted: false,
            terminal_emitted: false,
        }
    }

    /// attempt 開始を発行し、計測時計をこの時点へ合わせる。
    ///
    /// 呼び出し時点が「HTTP 送信直前」の開始点になるため、呼び出し元は
    /// ワイヤーリクエストの構築に成功した直後・送信直前に呼ぶこと。
    pub(crate) fn emit_started(&mut self) {
        self.started_at = tokio::time::Instant::now();
        self.started_emitted = true;
        self.emit(ProviderEvent::RequestStarted {
            request_id: self.request_id.clone(),
            provider: self.provider.clone(),
            profile: self.profile.clone(),
            protocol: self.protocol.to_string(),
            model: self.model.clone(),
            streaming: self.streaming,
            run_id: self.observation_run_id(),
        });
    }

    /// canonical 差分を TTFT 観測へ反映する。
    pub(crate) fn note_delta(&mut self, event: &StreamEvent) {
        if !self.streaming || self.first_token_emitted {
            return;
        }
        let is_first_token = match event {
            StreamEvent::TextDelta { text } => !text.is_empty(),
            StreamEvent::ToolCallDelta { .. } => true,
            StreamEvent::ReasoningDelta { .. } | StreamEvent::Completed { .. } => false,
        };
        if !is_first_token {
            return;
        }

        self.emit(ProviderEvent::FirstTokenObserved {
            request_id: self.request_id.clone(),
            provider: self.provider.clone(),
            profile: self.profile.clone(),
            protocol: self.protocol.to_string(),
            model: self.model.clone(),
            ttft_ms: self.elapsed_ms(),
            run_id: self.observation_run_id(),
        });
        self.first_token_emitted = true;
    }

    /// attempt 正常終了を発行する。
    pub(crate) fn emit_completed(&mut self, usage: &Usage, finish_reason: FinishReason) {
        if self.terminal_emitted {
            return;
        }
        let finish_reason = match finish_reason {
            FinishReason::Stop => "stop".to_string(),
            FinishReason::Length => "length".to_string(),
            FinishReason::ToolUse => "tool_use".to_string(),
            FinishReason::ContentFilter => "content_filter".to_string(),
            FinishReason::Other(reason) => reason,
        };
        self.emit(ProviderEvent::RequestCompleted {
            request_id: self.request_id.clone(),
            provider: self.provider.clone(),
            profile: self.profile.clone(),
            protocol: self.protocol.to_string(),
            model: self.model.clone(),
            streaming: self.streaming,
            duration_ms: self.elapsed_ms(),
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            cache_read_tokens: usage.cache_read_tokens,
            cache_write_tokens: usage.cache_write_tokens,
            finish_reason,
            run_id: self.observation_run_id(),
        });
        self.terminal_emitted = true;
    }

    /// attempt 異常終了を発行する。
    pub(crate) fn emit_failed(&mut self, error: &ProviderError) {
        if self.terminal_emitted {
            return;
        }
        let failure = match error {
            ProviderError::RateLimited { .. } => ProviderFailureKind::RateLimited,
            ProviderError::Http { status, .. } => ProviderFailureKind::Http { status: *status },
            ProviderError::Timeout => ProviderFailureKind::Timeout,
            ProviderError::InvalidSse { .. } | ProviderError::InvalidJson { .. } => {
                ProviderFailureKind::InvalidResponse
            }
            ProviderError::Request(_) => ProviderFailureKind::Transport,
        };
        self.emit_failed_kind(failure);
    }

    fn emit_failed_kind(&mut self, failure: ProviderFailureKind) {
        self.emit(ProviderEvent::RequestFailed {
            request_id: self.request_id.clone(),
            provider: self.provider.clone(),
            profile: self.profile.clone(),
            protocol: self.protocol.to_string(),
            model: self.model.clone(),
            streaming: self.streaming,
            duration_ms: self.elapsed_ms(),
            failure,
            run_id: self.observation_run_id(),
        });
        self.terminal_emitted = true;
    }

    fn elapsed_ms(&self) -> u64 {
        self.started_at.elapsed().as_millis() as u64
    }

    /// 観測相関用の run ID を observation context から写す。
    ///
    /// context 未設定の構築経路 (routing 未接続の直接利用等) では
    /// `None` を許容する。
    fn observation_run_id(&self) -> Option<String> {
        let run_id = self
            .observation
            .as_ref()
            .map(|context| context.run_id.clone());
        debug_assert!(
            self.observation.is_none() || run_id.is_some(),
            "observation context が設定されている場合、emit される全 attempt イベントに run_id を載せる"
        );
        run_id
    }

    fn emit(&self, event: ProviderEvent) {
        if let Some(bus) = self.bus.as_ref() {
            bus.emit(Event::new(event));
        }
    }
}

impl Drop for AttemptObserver {
    fn drop(&mut self) {
        if self.started_emitted && !self.terminal_emitted && self.bus.is_some() {
            self.emit_failed_kind(ProviderFailureKind::Other);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use event_bus::{EventKind, ProviderEvent, ProviderFailureKind};

    use super::*;

    fn observer(bus: Option<Arc<EventBus>>, streaming: bool) -> AttemptObserver {
        AttemptObserver::new(
            bus,
            "provider-a",
            Some("profile-a".to_string()),
            "protocol-a",
            "model-a",
            streaming,
            None,
        )
    }

    async fn next_provider_event(rx: &mut event_bus::EventReceiver) -> ProviderEvent {
        let event = rx.recv().await.expect("観測イベントを受信できる");
        match event.kind {
            EventKind::Provider(event) => event,
            other => panic!("Provider 以外のイベントを受信しました: {other:?}"),
        }
    }

    // Given: 同一プロセス内の連続呼び出し / When: request ID を生成 / Then: req prefix と単調増加する一意な counter を持つ
    #[test]
    fn request_id_has_prefix_and_unique_increasing_counter() {
        let first = next_request_id();
        let second = next_request_id();

        let first_parts = first.split('-').collect::<Vec<_>>();
        let second_parts = second.split('-').collect::<Vec<_>>();
        assert_eq!(first_parts.len(), 3);
        assert_eq!(first_parts[0], "req");
        assert_eq!(first_parts[1], second_parts[1]);
        assert_eq!(
            first_parts[2].parse::<u64>().expect("counter は数値") + 1,
            second_parts[2].parse::<u64>().expect("counter は数値")
        );
    }

    // Given: ProviderError の全 variant / When: attempt 失敗を発行 / Then: 観測用 failure 分類へ写像される
    #[tokio::test]
    async fn provider_errors_map_to_failure_kinds() {
        let cases = [
            (
                ProviderError::RateLimited { retry_after: None },
                ProviderFailureKind::RateLimited,
            ),
            (
                ProviderError::Http {
                    status: 503,
                    body: String::new(),
                },
                ProviderFailureKind::Http { status: 503 },
            ),
            (ProviderError::Timeout, ProviderFailureKind::Timeout),
            (
                ProviderError::InvalidSse {
                    detail: String::new(),
                },
                ProviderFailureKind::InvalidResponse,
            ),
            (
                ProviderError::InvalidJson {
                    detail: String::new(),
                },
                ProviderFailureKind::InvalidResponse,
            ),
            (
                ProviderError::Request("reset".to_string()),
                ProviderFailureKind::Transport,
            ),
        ];

        for (error, expected) in cases {
            let bus = Arc::new(EventBus::new(4));
            let mut rx = bus.subscribe();
            let mut observer = observer(Some(bus), false);
            observer.emit_started();
            let _ = next_provider_event(&mut rx).await;

            observer.emit_failed(&error);

            assert!(matches!(
                next_provider_event(&mut rx).await,
                ProviderEvent::RequestFailed { failure, .. } if failure == expected
            ));
        }
    }

    // Given: paused time 上の streaming attempt / When: reasoning・空 text・有効 text を時間を進めながら記録 / Then: 有効 text 到達時だけ累積時間で TTFT を発行する
    #[tokio::test(start_paused = true)]
    async fn first_token_uses_tokio_elapsed_and_ignores_non_visible_deltas() {
        let bus = Arc::new(EventBus::new(8));
        let mut rx = bus.subscribe();
        let mut observer = observer(Some(bus), true);
        observer.emit_started();
        let _ = next_provider_event(&mut rx).await;
        tokio::time::advance(Duration::from_millis(7)).await;
        observer.note_delta(&StreamEvent::ReasoningDelta {
            text: "考察".to_string(),
        });
        observer.note_delta(&StreamEvent::TextDelta {
            text: String::new(),
        });
        assert!(
            tokio::time::timeout(Duration::from_millis(1), rx.recv())
                .await
                .is_err()
        );
        tokio::time::advance(Duration::from_millis(5)).await;

        observer.note_delta(&StreamEvent::TextDelta {
            text: "答え".to_string(),
        });

        assert!(matches!(
            next_provider_event(&mut rx).await,
            ProviderEvent::FirstTokenObserved { ttft_ms: 13, .. }
        ));
    }

    // Given: tool call delta が複数届く attempt / When: 全 delta を記録 / Then: FirstTokenObserved は最初の1回だけ発行する
    #[tokio::test]
    async fn tool_delta_emits_first_token_only_once() {
        let bus = Arc::new(EventBus::new(8));
        let mut rx = bus.subscribe();
        let mut observer = observer(Some(bus), true);
        observer.emit_started();
        let _ = next_provider_event(&mut rx).await;
        let delta = StreamEvent::ToolCallDelta {
            index: 0,
            id: Some("call-1".to_string()),
            name: Some("tool".to_string()),
            arguments_delta: String::new(),
        };

        observer.note_delta(&delta);
        observer.note_delta(&delta);

        assert!(matches!(
            next_provider_event(&mut rx).await,
            ProviderEvent::FirstTokenObserved { .. }
        ));
        assert!(
            tokio::time::timeout(Duration::from_millis(10), rx.recv())
                .await
                .is_err()
        );
    }

    // Given: paused time 上の attempt / When: 完了を二度発行 / Then: 最初の duration・usage・finish reason だけが終端イベントになる
    #[tokio::test(start_paused = true)]
    async fn completed_is_terminal_and_emitted_once() {
        let bus = Arc::new(EventBus::new(8));
        let mut rx = bus.subscribe();
        let mut observer = observer(Some(bus), false);
        let usage = Usage {
            input_tokens: 2,
            output_tokens: 3,
            cache_read_tokens: 4,
            cache_write_tokens: 5,
        };
        observer.emit_started();
        let started = next_provider_event(&mut rx).await;
        tokio::time::advance(Duration::from_millis(21)).await;

        observer.emit_completed(&usage, FinishReason::ToolUse);
        observer.emit_failed(&ProviderError::Timeout);

        let ProviderEvent::RequestStarted { request_id, .. } = started else {
            panic!("started を期待")
        };
        assert!(matches!(
            next_provider_event(&mut rx).await,
            ProviderEvent::RequestCompleted {
                request_id: completed_id,
                duration_ms: 21,
                input_tokens: 2,
                output_tokens: 3,
                cache_read_tokens: 4,
                cache_write_tokens: 5,
                finish_reason,
                ..
            } if completed_id == request_id && finish_reason == "tool_use"
        ));
        assert!(
            tokio::time::timeout(Duration::from_millis(10), rx.recv())
                .await
                .is_err()
        );
    }

    // Given: 観測器を生成したが attempt 開始を発行していない / When: observer を drop / Then: 終端イベントを発行しない
    #[tokio::test]
    async fn drop_without_started_emits_nothing() {
        let bus = Arc::new(EventBus::new(8));
        let mut rx = bus.subscribe();
        let observer = observer(Some(bus), false);

        drop(observer);

        assert!(
            tokio::time::timeout(Duration::from_millis(10), rx.recv())
                .await
                .is_err()
        );
    }

    // Given: started 後に終端を発行しない attempt / When: observer を drop / Then: Other の失敗を1件発行する
    #[tokio::test]
    async fn drop_without_terminal_emits_other_failure() {
        let bus = Arc::new(EventBus::new(8));
        let mut rx = bus.subscribe();
        let started = {
            let mut observer = observer(Some(bus), true);
            observer.emit_started();
            let started = next_provider_event(&mut rx).await;
            drop(observer);
            started
        };

        let ProviderEvent::RequestStarted { request_id, .. } = started else {
            panic!("started を期待")
        };
        assert!(matches!(
            next_provider_event(&mut rx).await,
            ProviderEvent::RequestFailed { request_id: failed_id, failure: ProviderFailureKind::Other, .. }
                if failed_id == request_id
        ));
    }

    // Given: 正常完了済み attempt / When: observer を drop / Then: 追加の終端イベントを発行しない
    #[tokio::test]
    async fn drop_after_completed_emits_nothing_more() {
        let bus = Arc::new(EventBus::new(8));
        let mut rx = bus.subscribe();
        let mut observer = observer(Some(bus), false);
        observer.emit_started();
        let _ = next_provider_event(&mut rx).await;
        observer.emit_completed(&Usage::default(), FinishReason::Stop);
        let _ = next_provider_event(&mut rx).await;

        drop(observer);

        assert!(
            tokio::time::timeout(Duration::from_millis(10), rx.recv())
                .await
                .is_err()
        );
    }

    // Given: EventBus 未設定の attempt / When: 全観測操作と drop / Then: panic せず no-op になる
    #[test]
    fn observer_without_bus_is_noop() {
        let mut observer = observer(None, true);
        observer.emit_started();
        observer.note_delta(&StreamEvent::TextDelta {
            text: "x".to_string(),
        });
        observer.emit_failed(&ProviderError::Timeout);
    }
}
