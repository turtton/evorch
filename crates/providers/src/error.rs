//! provider クライアントのエラー型を定義します。

mod structured_output;

/// プロバイダ呼び出しで発生しうるエラー。
///
/// [`std::error::Error`] は thiserror により自動実装される。
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ProviderError {
    /// Re-enter boundary preparation before sending; never retry unchanged.
    #[error("Responses replay scope changed; request preparation is required")]
    ResponsesScopeChanged {
        actual: Box<crate::responses_replay::ResolvedResponsesScope>,
    },
    /// HTTP 429 (レート制限)。
    #[error(
        "rate limited (429){}",
        retry_after
            .as_ref()
            .map(|d| format!("; retry after {d:?}"))
            .unwrap_or_else(|| "; retry-after unknown".to_string())
    )]
    RateLimited {
        /// `Retry-After` が示す待機時間。ヘッダ未提供なら `None`。
        retry_after: Option<std::time::Duration>,
    },
    /// 429 以外の 4xx/5xx HTTP エラー。
    #[error("http error (status {status}): {body}")]
    Http {
        /// HTTP ステータスコード。
        status: u16,
        /// エラーレスポンス本文。
        body: String,
    },
    /// リクエストがタイムアウトした。
    #[error("request timed out")]
    Timeout,
    /// SSE ストリームが不正な形式だった。
    #[error("invalid SSE stream: {detail}")]
    InvalidSse {
        /// 不正の詳細。
        detail: String,
    },
    /// レスポンス本文の JSON 解析に失敗した。
    #[error("invalid JSON: {detail}")]
    InvalidJson {
        /// 失敗の詳細。
        detail: String,
    },
    /// その他のトランスポート層 (reqwest) 失敗。
    #[error("request failed: {0}")]
    Request(String),
    /// reqwest による通信失敗。
    #[error("transport error: {message}")]
    Transport {
        /// 通信失敗の詳細。
        message: String,
    },
    /// ストリーム再試行を上限まで使い切った。
    #[error("stream failed after {attempts} attempts: {last}")]
    RetriesExhausted {
        /// 実行した試行回数。
        attempts: u32,
        /// 最後に発生したエラー。
        last: Box<ProviderError>,
    },
}

impl ProviderError {
    /// 接続先が Structured Outputs を明示的に未対応としたエラーか。
    /// スキーマ不正や通信失敗を通常形式への再試行で隠さない。
    pub fn is_structured_output_unsupported(&self) -> bool {
        match self {
            Self::Http {
                status: 400 | 422 | 501,
                body,
            } => structured_output::is_unsupported(body),
            Self::RetriesExhausted { last, .. } => last.is_structured_output_unsupported(),
            _ => false,
        }
    }

    /// 入力がモデルの文脈窓を超えたことを示すエラーか。
    ///
    /// Responses API の `context_length_exceeded` を、JSON 本文と SSE 失敗イベントが
    /// 平坦化した `code: message` 形式の HTTP 400 の両方から判定します。
    /// 圧縮などの回復動作は呼び出し側が決めます。
    pub fn is_context_window_exceeded(&self) -> bool {
        match self {
            Self::Http { status: 400, body } => body_error_code_is(body, "context_length_exceeded"),
            Self::RetriesExhausted { last, .. } => last.is_context_window_exceeded(),
            _ => false,
        }
    }

    /// HTTP ステータスが対応するエラーではそのコード、それ以外では `None` を返す。
    ///
    /// [`ProviderError::RateLimited`] は常に `Some(429)`。
    pub fn status(&self) -> Option<u16> {
        match self {
            Self::RateLimited { .. } => Some(429),
            Self::Http { status, .. } => Some(*status),
            Self::ResponsesScopeChanged { .. }
            | Self::Timeout
            | Self::InvalidSse { .. }
            | Self::InvalidJson { .. }
            | Self::Request(_)
            | Self::Transport { .. }
            | Self::RetriesExhausted { .. } => None,
        }
    }
}

/// エラー本文の code が `code` か。JSON (`error.code` またはトップレベル `code`) と、
/// SSE 失敗イベントが平坦化した `code: message` の先頭を同じ規則で見る。
pub(crate) fn body_error_code_is(body: &str, code: &str) -> bool {
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(body) {
        let error = value.get("error").unwrap_or(&value);
        return error.get("code").and_then(serde_json::Value::as_str) == Some(code);
    }
    body.trim().split(':').next() == Some(code)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    // Given: retry_after 付き 429 / When: Display / Then: retry-after 待機時間が言及される
    #[test]
    fn rate_limited_display_mentions_retry_after_duration() {
        let error = ProviderError::RateLimited {
            retry_after: Some(Duration::from_secs(5)),
        };

        assert_eq!(error.to_string(), "rate limited (429); retry after 5s");
    }

    // Given: retry_after なし 429 / When: Display / Then: retry-after 不明が言及される
    #[test]
    fn rate_limited_display_mentions_unknown_retry_after() {
        let error = ProviderError::RateLimited { retry_after: None };

        assert_eq!(error.to_string(), "rate limited (429); retry-after unknown");
    }

    // Given: HTTP 500 / When: Display / Then: ステータスと本文が含まれる
    #[test]
    fn http_display_includes_status_and_body() {
        let error = ProviderError::Http {
            status: 500,
            body: "boom".to_string(),
        };

        assert_eq!(error.to_string(), "http error (status 500): boom");
    }

    // Given: 残りの各エラー値 / When: Display / Then: 対応するメッセージになる
    #[test]
    fn other_variants_display_messages() {
        assert_eq!(ProviderError::Timeout.to_string(), "request timed out");
        assert_eq!(
            ProviderError::InvalidSse {
                detail: "malformed event".to_string()
            }
            .to_string(),
            "invalid SSE stream: malformed event"
        );
        assert_eq!(
            ProviderError::InvalidJson {
                detail: "unexpected token".to_string()
            }
            .to_string(),
            "invalid JSON: unexpected token"
        );
        assert_eq!(
            ProviderError::Request("connection reset".to_string()).to_string(),
            "request failed: connection reset"
        );
    }

    // Given: 通信失敗と再試行枯渇エラー / When: Display / Then: 契約どおりのメッセージになる
    #[test]
    fn transport_and_retries_exhausted_display_messages() {
        assert_eq!(
            ProviderError::Transport {
                message: "connection reset".to_string(),
            }
            .to_string(),
            "transport error: connection reset"
        );
        assert_eq!(
            ProviderError::RetriesExhausted {
                attempts: 3,
                last: Box::new(ProviderError::Transport {
                    message: "connection reset".to_string(),
                }),
            }
            .to_string(),
            "stream failed after 3 attempts: transport error: connection reset"
        );
    }

    // Given: 各エラー値 / When: status() / Then: RateLimited は 429、Http はステータス、他は None
    #[test]
    fn status_maps_rate_limited_to_429_and_http_to_its_status() {
        assert_eq!(
            ProviderError::RateLimited { retry_after: None }.status(),
            Some(429)
        );
        assert_eq!(
            ProviderError::Http {
                status: 503,
                body: String::new()
            }
            .status(),
            Some(503)
        );
        assert_eq!(ProviderError::Timeout.status(), None);
        assert_eq!(
            ProviderError::InvalidSse {
                detail: String::new()
            }
            .status(),
            None
        );
        assert_eq!(
            ProviderError::InvalidJson {
                detail: String::new()
            }
            .status(),
            None
        );
        assert_eq!(ProviderError::Request("x".to_string()).status(), None);
        assert_eq!(
            ProviderError::Transport {
                message: String::new(),
            }
            .status(),
            None
        );
        assert_eq!(
            ProviderError::RetriesExhausted {
                attempts: 1,
                last: Box::new(ProviderError::Timeout),
            }
            .status(),
            None
        );
    }

    // Given: 平坦化・JSON・他 code・他ステータス・retry 済みの 400 / When: 文脈窓超過を判定
    // Then: HTTP 400 の context_length_exceeded だけが真になる
    #[test]
    fn context_window_exceeded_is_detected_from_flattened_and_json_bodies() {
        let http = |status, body: &str| ProviderError::Http {
            status,
            body: body.into(),
        };

        for body in [
            "context_length_exceeded: Your input exceeds the context window.",
            r#"{"error":{"code":"context_length_exceeded","message":"too long"}}"#,
        ] {
            assert!(http(400, body).is_context_window_exceeded(), "{body}");
            assert!(
                ProviderError::RetriesExhausted {
                    attempts: 2,
                    last: Box::new(http(400, body)),
                }
                .is_context_window_exceeded()
            );
        }
        assert!(!http(400, "invalid_prompt: context_length_exceeded").is_context_window_exceeded());
        assert!(!http(500, "context_length_exceeded: x").is_context_window_exceeded());
        assert!(!ProviderError::Timeout.is_context_window_exceeded());
    }

    // Given: ProviderError / When: trait 境界で確認 / Then: std::error::Error を実装する
    #[test]
    fn provider_error_implements_std_error() {
        fn assert_std_error<E: std::error::Error>(_: &E) {}
        let error = ProviderError::Timeout;
        assert_std_error(&error);
    }
}
