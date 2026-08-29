//! provider クライアントのエラー型を定義します。

/// プロバイダ呼び出しで発生しうるエラー。
///
/// [`std::error::Error`] は thiserror により自動実装される。
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ProviderError {
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
}

impl ProviderError {
    /// HTTP ステータスが対応するエラーではそのコード、それ以外では `None` を返す。
    ///
    /// [`ProviderError::RateLimited`] は常に `Some(429)`。
    pub fn status(&self) -> Option<u16> {
        match self {
            Self::RateLimited { .. } => Some(429),
            Self::Http { status, .. } => Some(*status),
            Self::Timeout
            | Self::InvalidSse { .. }
            | Self::InvalidJson { .. }
            | Self::Request(_) => None,
        }
    }
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
    }

    // Given: ProviderError / When: trait 境界で確認 / Then: std::error::Error を実装する
    #[test]
    fn provider_error_implements_std_error() {
        fn assert_std_error<E: std::error::Error>(_: &E) {}
        let error = ProviderError::Timeout;
        assert_std_error(&error);
    }
}
