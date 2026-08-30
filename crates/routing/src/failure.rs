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
            providers::ProviderError::InvalidSse { .. }
            | providers::ProviderError::InvalidJson { .. }
            | providers::ProviderError::Request(_) => Self::Other,
        }
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
