//! HTTP 通信の共通処理を提供します。
//!
//! 全プロバイダ実装が共有する reqwest クライアント構築、reqwest エラーから
//! [`ProviderError`] への変換、usage イベント発行 ([`UsageEmitter`]) の
//! 基盤を集約する。SSE バイトストリームの変換は子モジュール `stream` が担う。

use std::sync::Arc;
use std::time::Duration;

use event_bus::{Event, EventBus, UsageEvent};

use crate::error::ProviderError;
use crate::message::Usage;

pub(crate) mod stream;

/// 接続確立のタイムアウト。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// 1 回の読み込み操作のタイムアウト。ストリーミング中の無応答検出に使う。
const READ_TIMEOUT: Duration = Duration::from_secs(60);

/// 全プロバイダ実装で共有する reqwest クライアントを構築する。
///
/// - 接続タイムアウト 5 秒と読み込みタイムアウト 60 秒は常に設定する。
///   読み込みタイムアウトは「1 回の読み込み操作」に対するものであり、
///   ストリーミング応答の合計時間を制限しない。
/// - `timeout` が `Some` の場合のみリクエスト全体のタイムアウトを設定する。
///   これは `send` (非ストリーミング) 専用である。ストリーミングで
///   リクエスト全体のタイムアウトを使うと、長時間に及ぶ正当な SSE 応答が
///   途中で切断されてしまうため、`stream` では必ず `None` を渡し、
///   無応答検出は読み込みタイムアウトに任せること。
///
/// # Errors
/// reqwest クライアントの構築に失敗した場合 [`ProviderError::Request`] を返す。
#[allow(dead_code)] // TODO(T5/T6): provider 実装が利用するまでの一時許可
pub(crate) fn build_http_client(
    timeout: Option<Duration>,
) -> Result<reqwest::Client, ProviderError> {
    let mut builder = reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .read_timeout(READ_TIMEOUT);
    if let Some(timeout) = timeout {
        builder = builder.timeout(timeout);
    }
    builder.build().map_err(map_request_error)
}

/// エラーレスポンスを [`ProviderError`] へ変換する。
///
/// 429 は [`ProviderError::RateLimited`] (`Retry-After` ヘッダを解析)、
/// その他のステータスは [`ProviderError::Http`] へ変換する。
/// 成功レスポンス (2xx/3xx) に対しては呼び出さないこと。
/// 本文の読み取りに失敗した場合はその詳細を `body` に含める。
#[allow(dead_code)] // TODO(T5/T6): provider 実装が利用するまでの一時許可
pub(crate) async fn map_response_error(response: reqwest::Response) -> ProviderError {
    let status = response.status().as_u16();
    let retry_after = if status == 429 {
        parse_retry_after(response.headers())
    } else {
        None
    };
    let body = response
        .text()
        .await
        .unwrap_or_else(|err| format!("エラーレスポンス本文の読み取りに失敗しました: {err}"));
    http_error_from_parts(status, body, retry_after)
}

/// ステータス・本文・`Retry-After` の解析結果から HTTP 系エラーを組み立てる。
///
/// 429 なら [`ProviderError::RateLimited`]、それ以外なら
/// [`ProviderError::Http`] を返す。
fn http_error_from_parts(
    status: u16,
    body: String,
    retry_after: Option<Duration>,
) -> ProviderError {
    if status == 429 {
        ProviderError::RateLimited { retry_after }
    } else {
        ProviderError::Http { status, body }
    }
}

/// `Retry-After` ヘッダを秒数形式として解析する。
///
/// ヘッダが欠落している場合、HTTP-date 形式など u64 として解釈できない
/// 場合は `None` を返す ([`ProviderError::RateLimited`] の `retry_after: None`
/// に対応)。
fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
        .map(Duration::from_secs)
}

/// reqwest の送信系エラーを [`ProviderError`] へ変換する。
///
/// タイムアウトは [`ProviderError::Timeout`]、それ以外は
/// [`ProviderError::Request`] へ変換する。
#[allow(dead_code)] // TODO(T5/T6): provider 実装が利用するまでの一時許可
pub(crate) fn map_request_error(err: reqwest::Error) -> ProviderError {
    if err.is_timeout() {
        ProviderError::Timeout
    } else {
        ProviderError::Request(err.to_string())
    }
}

/// usage イベントをイベントバスへ発行する発行器。
///
/// 呼び出し側 (send / stream の実装) は 1 リクエストにつき
/// [`UsageEmitter::emit_usage`] をちょうど 1 回呼び出すこと。
/// バスが未設定の場合は発行が no-op になる。
///
/// usage 発行の所有権は完了した provider attempt の経路にのみ属する。
/// 非ストリーミングでは各 provider client の send 成功末尾
/// (`ChatCompletionsClient::send` など) が、
/// ストリーミングでは SSE ポンプ (`adapt_sse_stream`) の完了シグナル受理が
/// 発行地点であり、完了した attempt が usage をちょうど 1 回発行する。
/// 失敗・エラー・タイムアウト・JSON パース不正・完了シグナル不落・
/// コンシューマ中断となった attempt では usage を 1 件も発行しない。
/// リトライ / フォールバックのコーディネータは usage を発行・再発行しない
/// (発行所有権は各 attempt に留まる)。このためリトライやフォールバックを
/// 経て成功した論理リクエストでも usage イベントはちょうど 1 件だけ発行
/// され、勝者 attempt のプロバイダラベルとモデルを載せる。
pub struct UsageEmitter {
    bus: Option<Arc<EventBus>>,
    provider: String,
}

impl UsageEmitter {
    /// イベントバスとプロバイダ識別子から発行器を生成する。
    ///
    /// バスが不要な場合は `None` を渡す (発行が no-op になる)。
    pub fn new(bus: Option<Arc<EventBus>>, provider: impl Into<String>) -> Self {
        Self {
            bus,
            provider: provider.into(),
        }
    }

    /// 1 リクエストのトークン使用量をバスへ発行する。
    ///
    /// 完了した attempt につきちょうど 1 回呼び出すこと。失敗・中断された
    /// attempt では呼び出さないこと。バスが未設定の場合は何もしない。
    pub fn emit_usage(&self, model: &str, usage: &Usage) {
        let Some(bus) = self.bus.as_ref() else {
            return;
        };
        bus.emit(Event::new(UsageEvent::Usage {
            provider: self.provider.clone(),
            model: model.to_string(),
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            cache_read_tokens: usage.cache_read_tokens,
            cache_write_tokens: usage.cache_write_tokens,
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use event_bus::EventKind;

    // Given: 秒数形式の Retry-After ヘッダ / When: parse_retry_after / Then: 秒の Duration になる
    #[test]
    fn retry_after_seconds_header_parses_to_duration() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(reqwest::header::RETRY_AFTER, "120".parse().unwrap());

        assert_eq!(parse_retry_after(&headers), Some(Duration::from_secs(120)));
    }

    // Given: HTTP-date 形式の Retry-After ヘッダ / When: parse_retry_after / Then: None (秒数形式ではない)
    #[test]
    fn retry_after_http_date_header_is_none() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::RETRY_AFTER,
            "Wed, 21 Oct 2015 07:28:00 GMT".parse().unwrap(),
        );

        assert_eq!(parse_retry_after(&headers), None);
    }

    // Given: Retry-After ヘッダが無い / When: parse_retry_after / Then: None
    #[test]
    fn missing_retry_after_header_is_none() {
        let headers = reqwest::header::HeaderMap::new();

        assert_eq!(parse_retry_after(&headers), None);
    }

    // Given: 非数値の Retry-After ヘッダ / When: parse_retry_after / Then: None
    #[test]
    fn non_numeric_retry_after_header_is_none() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(reqwest::header::RETRY_AFTER, "soon".parse().unwrap());

        assert_eq!(parse_retry_after(&headers), None);
    }

    // Given: 429 と Retry-After / When: http_error_from_parts / Then: retry_after 付き RateLimited になる
    #[test]
    fn status_429_with_retry_after_becomes_rate_limited() {
        let error = http_error_from_parts(429, "body".to_string(), Some(Duration::from_secs(7)));

        assert_eq!(
            error,
            ProviderError::RateLimited {
                retry_after: Some(Duration::from_secs(7))
            }
        );
    }

    // Given: 429 と Retry-After 無し / When: http_error_from_parts / Then: retry_after None の RateLimited になる
    #[test]
    fn status_429_without_retry_after_becomes_rate_limited_with_none() {
        let error = http_error_from_parts(429, "body".to_string(), None);

        assert_eq!(error, ProviderError::RateLimited { retry_after: None });
    }

    // Given: 429 以外のステータス / When: http_error_from_parts / Then: ステータスと本文を保持する Http になる
    #[test]
    fn other_status_becomes_http_error_with_body() {
        let error =
            http_error_from_parts(503, "unavailable".to_string(), Some(Duration::from_secs(1)));

        assert_eq!(
            error,
            ProviderError::Http {
                status: 503,
                body: "unavailable".to_string()
            }
        );
    }

    // Given: URL 不正による reqwest エラー (タイムアウト以外) / When: map_request_error / Then: Request に変換される
    #[test]
    fn non_timeout_request_error_maps_to_request_variant() {
        let err = reqwest::Client::new()
            .get("not a url")
            .build()
            .expect_err("不正な URL はビルド時に失敗する");

        assert!(matches!(map_request_error(err), ProviderError::Request(_)));
    }

    // Given: タイムアウト有り / 無しの指定 / When: build_http_client / Then: いずれも構築に成功する
    #[test]
    fn http_client_builds_with_and_without_whole_request_timeout() {
        assert!(build_http_client(None).is_ok());
        assert!(build_http_client(Some(Duration::from_secs(30))).is_ok());
    }

    // Given: バス接続済みの UsageEmitter / When: emit_usage を 1 回呼ぶ / Then: 全フィールドを保持する Usage イベントが 1 件だけ届く
    #[tokio::test]
    async fn emit_usage_publishes_single_event_with_all_fields() {
        let bus = Arc::new(EventBus::new(8));
        let mut rx = bus.subscribe();
        let emitter = UsageEmitter::new(Some(bus), "anthropic");
        let usage = Usage {
            input_tokens: 10,
            output_tokens: 5,
            cache_read_tokens: 3,
            cache_write_tokens: 1,
        };

        emitter.emit_usage("kimi-k3", &usage);

        let event = rx.recv().await.expect("usage イベントを受信できる");
        match event.kind {
            EventKind::Usage(UsageEvent::Usage {
                provider,
                model,
                input_tokens,
                output_tokens,
                cache_read_tokens,
                cache_write_tokens,
            }) => {
                assert_eq!(provider, "anthropic");
                assert_eq!(model, "kimi-k3");
                assert_eq!(
                    (
                        input_tokens,
                        output_tokens,
                        cache_read_tokens,
                        cache_write_tokens
                    ),
                    (10, 5, 3, 1)
                );
            }
            other => panic!("Usage 以外のイベントを受信しました: {other:?}"),
        }
        let second = tokio::time::timeout(Duration::from_millis(50), rx.recv()).await;
        assert!(
            second.is_err(),
            "usage イベントは 1 リクエスト 1 回のみ発行される"
        );
    }

    // Given: バス未設定の UsageEmitter / When: emit_usage を呼ぶ / Then: no-op として何も起きない
    #[test]
    fn emitter_without_bus_is_noop() {
        let emitter = UsageEmitter::new(None, "anthropic");

        emitter.emit_usage("model", &Usage::default());
    }
}
