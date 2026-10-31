//! プロバイダリクエスト attempt の観測イベントを発行する。
// allow: SIZE_OK — observer 本体と指定された単体契約テストを同一モジュールに集約する。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::{SystemTime, UNIX_EPOCH};

use event_bus::{
    DiagnosticEvent, DiagnosticSeverity, Event, EventBus, ProviderEvent, ProviderFailureKind,
    REQUEST_FAILED_DETAIL_MAX_BYTES, ServiceTier,
};

use crate::error::ProviderError;
use crate::message::{
    AttemptLog, AttemptOutcome, AttemptRecord, FinishReason, ObservationContext, Usage,
};
use crate::stream::StreamEvent;

mod cache;
#[cfg(test)]
mod cache_tests;
mod tool_arguments;

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
    purpose_override: Option<event_bus::RequestPurpose>,
    service_tier: Option<ServiceTier>,
    reasoning_effort: Option<String>,
    started_at: tokio::time::Instant,
    started_emitted: bool,
    first_token_emitted: bool,
    terminal_emitted: bool,
    /// provider が終端で usage を報告しなかった。ゼロを実測として cache 観測へ流さない。
    usage_missing: bool,
    cache: Option<cache::CacheObservation>,
    /// 失敗詳細から伏せる、この attempt の credential。
    known_secret: Option<String>,
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
            purpose_override: None,
            service_tier: None,
            reasoning_effort: None,
            started_at: tokio::time::Instant::now(),
            started_emitted: false,
            first_token_emitted: false,
            terminal_emitted: false,
            usage_missing: false,
            cache: None,
            known_secret: None,
        }
    }

    /// Dedicated capabilities attest their purpose even without a run context.
    pub(crate) fn with_purpose(mut self, purpose: event_bus::RequestPurpose) -> Self {
        self.purpose_override = Some(purpose);
        self
    }

    /// 失敗詳細から伏せる credential を登録する。空文字列は無視する。
    pub(crate) fn with_known_secret(mut self, secret: &str) -> Self {
        self.known_secret = (!secret.is_empty()).then(|| secret.to_owned());
        self
    }

    /// wire へ実際に載せるサービス階層を完了イベントへ記録する。
    pub(crate) fn with_service_tier(mut self, service_tier: Option<ServiceTier>) -> Self {
        self.service_tier = service_tier;
        self
    }

    /// wire へ実際に載せる reasoning effort を開始イベントへ記録する。
    pub(crate) fn with_reasoning_effort(mut self, effort: Option<&str>) -> Self {
        self.reasoning_effort = effort.map(str::to_owned);
        self
    }

    pub(crate) fn with_cache_observation(mut self, wire_request: &impl serde::Serialize) -> Self {
        self.cache = self.observe_cache_request(wire_request);
        self
    }

    /// attempt 開始を発行し、計測時計をこの時点へ合わせる。
    ///
    /// 呼び出し時点が「HTTP 送信直前」の開始点になるため、呼び出し元は
    /// ワイヤーリクエストの構築に成功した直後・送信直前に呼ぶこと。
    pub(crate) fn emit_started(&mut self) {
        self.started_at = tokio::time::Instant::now();
        self.started_emitted = true;
        if let Some(log) = self.attempt_log() {
            log.started(AttemptRecord {
                request_id: self.request_id.clone(),
                provider: self.provider.clone(),
                profile: self.profile.clone(),
                protocol: self.protocol.to_string(),
                model: self.model.clone(),
                streaming: self.streaming,
                outcome: AttemptOutcome::Pending,
            });
        }
        self.emit(ProviderEvent::RequestStarted {
            request_id: self.request_id.clone(),
            provider: self.provider.clone(),
            profile: self.profile.clone(),
            protocol: self.protocol.to_string(),
            model: self.model.clone(),
            streaming: self.streaming,
            run_id: self.observation_run_id(),
            reasoning_effort: self.reasoning_effort.clone(),
            purpose: self.observation_purpose(),
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
        if self.usage_missing {
            self.emit_usage_missing();
        } else {
            self.record_cache(usage);
        }
        let finish_reason = match finish_reason {
            FinishReason::Stop => "stop".to_string(),
            FinishReason::Length => "length".to_string(),
            FinishReason::ToolUse => "tool_use".to_string(),
            FinishReason::ContentFilter => "content_filter".to_string(),
            FinishReason::Other(reason) => reason,
        };
        let finish_reason_label = finish_reason.clone();
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
            reasoning_tokens: usage.reasoning_tokens,
            purpose: self.observation_purpose(),
            service_tier: self.service_tier,
            usage_reported: !self.usage_missing,
        });
        self.terminal_emitted = true;
        if let Some(log) = self.attempt_log() {
            log.finished(
                &self.request_id,
                AttemptOutcome::Completed {
                    finish_reason: finish_reason_label,
                },
            );
        }
    }

    /// 次の [`Self::emit_completed`] の usage は provider 未報告の代替値であると記録する。
    ///
    /// 完了時に cache 観測 (`CacheReuseObserved`) を抑止し、診断を 1 件発行する。
    pub(crate) fn mark_usage_missing(&mut self) {
        self.usage_missing = true;
    }

    fn emit_usage_missing(&self) {
        tracing::warn!(
            request_id = %self.request_id,
            provider = %self.provider,
            model = %self.model,
            "provider did not report usage; usage and cache observation skipped"
        );
        let Some(bus) = self.bus.as_ref() else {
            return;
        };
        bus.emit(Event::new(DiagnosticEvent {
            source: "providers.usage".into(),
            severity: DiagnosticSeverity::Warning,
            code: event_bus::event::diagnostic_codes::PROVIDER_USAGE_MISSING.into(),
            detail: format!(
                "provider={} profile={:?} protocol={} model={} streaming={}: the terminal response carried no usage; token usage was not recorded and context size falls back to the serialized estimate",
                self.provider, self.profile, self.protocol, self.model, self.streaming,
            ),
            run_id: self.observation_run_id(),
            thread_id: None,
            call_id: Some(self.request_id.clone()),
        }));
    }

    /// attempt 異常終了を発行する。
    pub(crate) fn emit_failed(&mut self, error: &ProviderError) {
        if self.terminal_emitted {
            return;
        }
        let detail = failure_detail(error, self.known_secret.as_deref());
        self.emit_failed_kind(failure_kind(error), detail);
    }

    /// 401 で拒否された attempt を `Auth` 失敗として閉じる。
    pub(crate) fn emit_failed_auth(&mut self, error: &ProviderError) {
        if self.terminal_emitted {
            return;
        }
        let detail = failure_detail(error, self.known_secret.as_deref());
        self.emit_failed_kind(ProviderFailureKind::Auth, detail);
    }

    /// 閉じた attempt の後に同じ呼び出しの再送 attempt を始める。
    ///
    /// 新しい request_id を採番し、計測時計と終端・開始状態を初期化する。
    /// 呼び出し元は再送の直前に [`Self::emit_started`] を呼ぶこと。
    pub(crate) fn restart_attempt(&mut self) {
        self.request_id = next_request_id();
        self.started_at = tokio::time::Instant::now();
        self.started_emitted = false;
        self.first_token_emitted = false;
        self.terminal_emitted = false;
        self.usage_missing = false;
    }

    fn emit_failed_kind(&mut self, failure: ProviderFailureKind, detail: Option<String>) {
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
            purpose: self.observation_purpose(),
            detail,
        });
        self.terminal_emitted = true;
        if let Some(log) = self.attempt_log() {
            log.finished(&self.request_id, AttemptOutcome::Failed { failure });
        }
    }

    fn attempt_log(&self) -> Option<&AttemptLog> {
        self.observation
            .as_ref()
            .and_then(|context| context.attempts.as_ref())
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

    fn observation_purpose(&self) -> Option<event_bus::RequestPurpose> {
        self.purpose_override
            .or_else(|| self.observation.as_ref().map(|context| context.purpose))
    }

    fn emit(&self, event: ProviderEvent) {
        if let Some(bus) = self.bus.as_ref() {
            bus.emit(Event::new(event));
        }
    }
}

/// 失敗の表示文字列を、credential を伏せて [`REQUEST_FAILED_DETAIL_MAX_BYTES`] 以下に切り詰める。
fn failure_detail(error: &ProviderError, known_secret: Option<&str>) -> Option<String> {
    let mut text = error.to_string();
    if let Some(secret) = known_secret {
        text = text.replace(secret, "***");
    }
    let text = secret_guard::SecretRedactor::from_env().redact(&text).text;
    let mut end = text.len().min(REQUEST_FAILED_DETAIL_MAX_BYTES);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let detail = text[..end].trim();
    (!detail.is_empty()).then(|| detail.to_owned())
}

/// プロバイダエラーを観測イベント用の障害種別へ分類する。
fn failure_kind(error: &ProviderError) -> ProviderFailureKind {
    match error {
        ProviderError::RateLimited { .. } => ProviderFailureKind::RateLimited,
        ProviderError::Http { status, .. } => ProviderFailureKind::Http { status: *status },
        ProviderError::Timeout => ProviderFailureKind::Timeout,
        ProviderError::InvalidSse { .. } | ProviderError::InvalidJson { .. } => {
            ProviderFailureKind::InvalidResponse
        }
        ProviderError::ResponsesScopeChanged { .. }
        | ProviderError::Request(_)
        | ProviderError::Transport { .. } => ProviderFailureKind::Transport,
        ProviderError::RetriesExhausted { last, .. } => failure_kind(last),
    }
}

impl Drop for AttemptObserver {
    fn drop(&mut self) {
        if self.started_emitted
            && !self.terminal_emitted
            && (self.bus.is_some() || self.attempt_log().is_some())
        {
            self.emit_failed_kind(ProviderFailureKind::Other, None);
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

    // Given: 他スレッドも採番する同一プロセス / When: request ID を生成 / Then: req prefix と呼出元ごとの単調増加・全体の一意性を保つ
    #[test]
    fn request_id_has_prefix_and_unique_increasing_counter() {
        let first = next_request_id();
        let second = next_request_id();

        let first_parts = first.split('-').collect::<Vec<_>>();
        let second_parts = second.split('-').collect::<Vec<_>>();
        assert_eq!(first_parts.len(), 3);
        assert_eq!(first_parts[0], "req");
        assert_eq!(first_parts[1], second_parts[1]);
        // Other callers may allocate IDs between these two calls.
        assert!(
            first_parts[2].parse::<u64>().expect("counter は数値")
                < second_parts[2].parse::<u64>().expect("counter は数値")
        );
        let batches = std::thread::scope(|scope| {
            let threads = (0..8)
                .map(|_| scope.spawn(|| (0..64).map(|_| next_request_id()).collect::<Vec<_>>()))
                .collect::<Vec<_>>();
            threads
                .into_iter()
                .map(|thread| thread.join().unwrap())
                .collect::<Vec<_>>()
        });
        let mut unique = std::collections::HashSet::from([first.clone(), second.clone()]);
        for batch in batches {
            let mut previous = second_parts[2].parse::<u64>().unwrap();
            for id in batch {
                let parts = id.split('-').collect::<Vec<_>>();
                assert_eq!(parts.len(), 3);
                assert_eq!(parts[0], "req");
                assert_eq!(parts[1], first_parts[1]);
                let current = parts[2].parse::<u64>().expect("counter は数値");
                assert!(current > previous, "each caller's IDs must increase");
                assert!(
                    unique.insert(id),
                    "concurrent callers must never share an ID"
                );
                previous = current;
            }
        }
        assert_eq!(unique.len(), 2 + 8 * 64);
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
            (
                ProviderError::Transport {
                    message: "reset".to_string(),
                },
                ProviderFailureKind::Transport,
            ),
            (
                ProviderError::RetriesExhausted {
                    attempts: 3,
                    last: Box::new(ProviderError::Timeout),
                },
                ProviderFailureKind::Timeout,
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

    // Given: credential を含む長い多バイトの HTTP エラー本文 / When: attempt 失敗を発行 / Then: detail は credential を伏せ、UTF-8 境界で上限以下に切り詰められる
    #[tokio::test]
    async fn failure_detail_hides_credential_and_fits_limit() {
        let secret = "profile-secret-value-1234";
        let body = format!("quota exceeded for key {secret}; {}", "上限".repeat(600));
        let bus = Arc::new(EventBus::new(4));
        let mut rx = bus.subscribe();
        let mut observer = observer(Some(bus), false).with_known_secret(secret);
        observer.emit_started();
        let _ = next_provider_event(&mut rx).await;

        observer.emit_failed(&ProviderError::Http { status: 403, body });

        let ProviderEvent::RequestFailed {
            detail: Some(detail),
            ..
        } = next_provider_event(&mut rx).await
        else {
            panic!("RequestFailed に detail が載る");
        };
        assert!(detail.starts_with("http error (status 403): quota exceeded for key ***"));
        assert!(!detail.contains(secret));
        assert!(detail.len() <= REQUEST_FAILED_DETAIL_MAX_BYTES);
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
            context_tokens: None,
            input_tokens: 2,
            output_tokens: 3,
            cache_read_tokens: 4,
            cache_write_tokens: 5,
            reasoning_tokens: None,
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
