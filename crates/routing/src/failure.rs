/// プロバイダ障害をルーティング判断に用いる種別へ正規化した値です。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    /// レート制限。
    RateLimited,
    /// プロバイダ側のサーバーエラー。
    Server,
    /// リクエストのタイムアウト。
    Timeout,
    /// 支払いまたは割り当て量の上限。
    Quota,
    /// 認証または認可の失敗。
    Auth,
    /// ほかの分類不能な障害。
    Other,
}

impl From<&providers::ProviderError> for FailureKind {
    fn from(value: &providers::ProviderError) -> Self {
        match value {
            providers::ProviderError::RateLimited { .. } => Self::RateLimited,
            providers::ProviderError::Http { status, .. } => match status {
                429 => Self::RateLimited,
                500..=599 => Self::Server,
                401 | 403 => Self::Auth,
                402 => Self::Quota,
                _ => Self::Other,
            },
            providers::ProviderError::Timeout => Self::Timeout,
            providers::ProviderError::ResponsesScopeChanged { .. }
            | providers::ProviderError::InvalidSse { .. }
            | providers::ProviderError::InvalidJson { .. }
            | providers::ProviderError::Request(_)
            | providers::ProviderError::Transport { .. } => Self::Other,
            providers::ProviderError::RetriesExhausted { last, .. } => Self::from(last.as_ref()),
        }
    }
}

/// Kimi サブスクリプションが利用上限を 403 で返すときの本文に現れる語句 (小文字で照合)。
const KIMI_USAGE_LIMIT_MARKERS: &[&str] = &[
    "usage limit",
    "quota",
    "exceeded",
    "limit reached",
    "insufficient",
    "额度",
    "上限",
];

impl FailureKind {
    /// プロバイダ種別ごとの規則を加えて障害を分類します。
    ///
    /// 規則に当てはまらない障害は [`From<&providers::ProviderError>`] と同じ分類になります。
    pub fn classify(error: &providers::ProviderError, provider_type: model::ProviderType) -> Self {
        match error {
            providers::ProviderError::RetriesExhausted { last, .. } => {
                Self::classify(last, provider_type)
            }
            providers::ProviderError::Http { status: 403, body }
                if provider_type == model::ProviderType::KimiSubscription
                    && is_kimi_usage_limit(body) =>
            {
                Self::Quota
            }
            other => Self::from(other),
        }
    }
}

fn is_kimi_usage_limit(body: &str) -> bool {
    let body = body.to_lowercase();
    KIMI_USAGE_LIMIT_MARKERS
        .iter()
        .any(|marker| body.contains(marker))
}

/// 時間をおけば回復しうる一時的な失敗 (429・408・5xx・timeout・transport) か (ADR 0030)。
///
/// 一周するフォールバックで、失敗した位置を最後にもう一度試すかの判断に使います。
pub fn is_transient(error: &providers::ProviderError) -> bool {
    match error {
        providers::ProviderError::RateLimited { .. }
        | providers::ProviderError::Timeout
        | providers::ProviderError::Transport { .. } => true,
        providers::ProviderError::Http { status, .. } => {
            matches!(status, 408 | 429 | 500..=599)
        }
        providers::ProviderError::RetriesExhausted { last, .. } => is_transient(last),
        providers::ProviderError::ResponsesScopeChanged { .. }
        | providers::ProviderError::Request(_)
        | providers::ProviderError::InvalidSse { .. }
        | providers::ProviderError::InvalidJson { .. } => false,
    }
}

/// 障害が示す待機時間 (Retry-After) を返します。
pub fn retry_after(error: &providers::ProviderError) -> Option<std::time::Duration> {
    match error {
        providers::ProviderError::RateLimited { retry_after } => *retry_after,
        providers::ProviderError::RetriesExhausted { last, .. } => retry_after(last),
        _ => None,
    }
}

impl From<FailureKind> for event_bus::ProviderFailureKind {
    fn from(value: FailureKind) -> Self {
        match value {
            FailureKind::RateLimited => Self::RateLimited,
            FailureKind::Server => Self::Server,
            FailureKind::Timeout => Self::Timeout,
            FailureKind::Quota => Self::Quota,
            FailureKind::Auth => Self::Auth,
            FailureKind::Other => Self::Other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::FailureKind;
    use std::time::Duration;

    // Given: ProviderError の各変異 / When: FailureKind に分類する / Then: リトライ判断用の失敗種別になる
    #[test]
    fn failure_kind_maps_each_provider_error_variant() {
        let cases = [
            (
                providers::ProviderError::RateLimited {
                    retry_after: Some(Duration::from_secs(1)),
                },
                FailureKind::RateLimited,
            ),
            (
                providers::ProviderError::Http {
                    status: 400,
                    body: String::new(),
                },
                FailureKind::Other,
            ),
            (providers::ProviderError::Timeout, FailureKind::Timeout),
            (
                providers::ProviderError::InvalidSse {
                    detail: String::new(),
                },
                FailureKind::Other,
            ),
            (
                providers::ProviderError::InvalidJson {
                    detail: String::new(),
                },
                FailureKind::Other,
            ),
            (
                providers::ProviderError::Request(String::new()),
                FailureKind::Other,
            ),
            (
                providers::ProviderError::Transport {
                    message: String::new(),
                },
                FailureKind::Other,
            ),
            (
                providers::ProviderError::RetriesExhausted {
                    attempts: 3,
                    last: Box::new(providers::ProviderError::Timeout),
                },
                FailureKind::Timeout,
            ),
        ];

        for (error, expected) in cases {
            assert_eq!(FailureKind::from(&error), expected);
        }
    }

    // Given: 分類境界のHTTPステータス / When: FailureKind に分類する / Then: ステータス別の失敗種別になる
    #[test]
    fn failure_kind_classifies_http_statuses() {
        let cases = [
            (429, FailureKind::RateLimited),
            (500, FailureKind::Server),
            (503, FailureKind::Server),
            (401, FailureKind::Auth),
            (403, FailureKind::Auth),
            (402, FailureKind::Quota),
            (400, FailureKind::Other),
            (418, FailureKind::Other),
        ];

        for (status, expected) in cases {
            let error = providers::ProviderError::Http {
                status,
                body: String::new(),
            };
            assert_eq!(FailureKind::from(&error), expected, "status {status}");
        }
    }

    // Given: 利用上限を示す本文と示さない本文の 403、他種別の 403、retry 済みの 403
    // When: プロバイダ種別を加えて分類する
    // Then: Kimi サブスクリプションの利用上限だけが Quota になり、他は Auth のまま
    #[test]
    fn classify_treats_kimi_usage_limit_403_as_quota() {
        use model::ProviderType;
        let forbidden = |body: &str| providers::ProviderError::Http {
            status: 403,
            body: body.to_string(),
        };
        let limit = forbidden(r#"{"error":{"message":"You have exceeded the Usage Limit"}}"#);

        assert_eq!(
            FailureKind::classify(&limit, ProviderType::KimiSubscription),
            FailureKind::Quota
        );
        assert_eq!(
            FailureKind::classify(&forbidden("当前额度已用完"), ProviderType::KimiSubscription),
            FailureKind::Quota
        );
        assert_eq!(
            FailureKind::classify(
                &forbidden("invalid api key"),
                ProviderType::KimiSubscription
            ),
            FailureKind::Auth
        );
        assert_eq!(
            FailureKind::classify(&limit, ProviderType::OpenAiCompatible),
            FailureKind::Auth
        );
        let retried = providers::ProviderError::RetriesExhausted {
            attempts: 2,
            last: Box::new(limit),
        };
        assert_eq!(
            FailureKind::classify(&retried, ProviderType::KimiSubscription),
            FailureKind::Quota
        );
    }

    // Given: Codex の response.failed を providers が合成した HTTP エラー (402 の quota 系、
    // 503 の capacity、500 の server_error、404 の model_not_found、400 の context 超過) と retry 済みの 402
    // When: Codex の種別で分類する
    // Then: quota 系は Quota、capacity と server_error は Server、残りは Other になる
    #[test]
    fn classify_maps_codex_sse_failures_to_routing_kinds() {
        use model::ProviderType;
        let http = |status, body: &str| providers::ProviderError::Http {
            status,
            body: body.to_string(),
        };
        let quota = http(402, "usage_limit_reached: The usage limit has been reached");
        for (error, expected) in [
            (quota.clone(), FailureKind::Quota),
            (
                http(402, "insufficient_quota: You exceeded your current quota"),
                FailureKind::Quota,
            ),
            (
                http(503, "policy_capacity_unavailable: Capacity is unavailable"),
                FailureKind::Server,
            ),
            (
                http(500, "server_error: The server had an error"),
                FailureKind::Server,
            ),
            (
                http(404, "model_not_found: no such model"),
                FailureKind::Other,
            ),
            (
                http(400, "context_length_exceeded: too long"),
                FailureKind::Other,
            ),
            (
                providers::ProviderError::RetriesExhausted {
                    attempts: 2,
                    last: Box::new(quota),
                },
                FailureKind::Quota,
            ),
        ] {
            assert_eq!(
                FailureKind::classify(&error, ProviderType::OpenAiCodex),
                expected,
                "{error}"
            );
        }
    }

    // Given: Retry-After 付きの 429、retry 済みの 429、他の障害 / When: 待機時間を取り出す
    // Then: 429 の Retry-After だけが返る
    #[test]
    fn retry_after_reads_rate_limit_hint() {
        let limited = providers::ProviderError::RateLimited {
            retry_after: Some(Duration::from_secs(7)),
        };
        assert_eq!(super::retry_after(&limited), Some(Duration::from_secs(7)));
        let retried = providers::ProviderError::RetriesExhausted {
            attempts: 3,
            last: Box::new(limited),
        };
        assert_eq!(super::retry_after(&retried), Some(Duration::from_secs(7)));
        assert_eq!(super::retry_after(&providers::ProviderError::Timeout), None);
    }

    // Given: FailureKind の全 6 variant。
    // When: 観測イベント用の ProviderFailureKind へ変換する。
    // Then: 同名の分類に写像される (Http ステータスは routing 層では保持しない)。
    #[test]
    fn failure_kind_maps_to_provider_failure_kind() {
        use event_bus::ProviderFailureKind;

        let cases = [
            (FailureKind::RateLimited, ProviderFailureKind::RateLimited),
            (FailureKind::Server, ProviderFailureKind::Server),
            (FailureKind::Timeout, ProviderFailureKind::Timeout),
            (FailureKind::Quota, ProviderFailureKind::Quota),
            (FailureKind::Auth, ProviderFailureKind::Auth),
            (FailureKind::Other, ProviderFailureKind::Other),
        ];

        for (failure, expected) in cases {
            assert_eq!(ProviderFailureKind::from(failure), expected);
        }
    }
}
