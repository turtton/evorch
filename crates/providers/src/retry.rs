use crate::ProviderError;
use std::time::Duration;

/// provider 層で用いる、決定的なストリーム再試行ポリシー。
///
/// 再試行の責務は 2026-09-12 にオペレータが確定した provider 層に置く。
/// `max_attempts` は初回を含む総試行回数であり、たとえば `3` は初回と最大 2 回の再試行を表す。
#[derive(Clone, Debug, PartialEq)]
pub struct RetryPolicy {
    pub max_attempts: u32,
    pub initial_delay: Duration,
    pub backoff_factor: u32,
    pub max_delay: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            initial_delay: Duration::from_millis(250),
            backoff_factor: 2,
            max_delay: Duration::from_secs(2),
        }
    }
}

impl RetryPolicy {
    /// 初回試行だけを許可するポリシーを返す。
    pub fn no_retry() -> Self {
        Self {
            max_attempts: 1,
            ..Self::default()
        }
    }

    /// `attempt` 回目（0 始まり）の失敗後に待機する、上限付きバックオフ時間を返す。
    pub fn backoff_for(&self, attempt: u32) -> Duration {
        self.initial_delay
            .saturating_mul(self.backoff_factor.saturating_pow(attempt))
            .min(self.max_delay)
    }
}

/// エラーが provider 層の再試行対象かを分類する。
///
/// 認証系を含むその他の 4xx は即時 escalation し、`RateLimited` は quota 領域のため本スコープ外とする。
pub fn is_retryable(error: &ProviderError) -> bool {
    match error {
        ProviderError::Timeout | ProviderError::Transport { .. } => true,
        ProviderError::Http { status, .. } => *status == 408 || (500..=599).contains(status),
        ProviderError::RateLimited { .. }
        | ProviderError::InvalidSse { .. }
        | ProviderError::InvalidJson { .. }
        | ProviderError::Request(_)
        | ProviderError::RetriesExhausted { .. } => false,
    }
}

#[cfg(test)]
mod tests {
    use super::{RetryPolicy, is_retryable};
    use crate::ProviderError;
    use std::time::Duration;

    // Given: デフォルトの再試行ポリシー / When: Default を生成 / Then: 仕様の上限・遅延値になる
    #[test]
    fn default_policy_uses_specified_values() {
        let policy = RetryPolicy::default();

        assert_eq!(policy.max_attempts, 3);
        assert_eq!(policy.initial_delay, Duration::from_millis(250));
        assert_eq!(policy.backoff_factor, 2);
        assert_eq!(policy.max_delay, Duration::from_secs(2));
    }

    // Given: 再試行を無効化するポリシー / When: no_retry を生成 / Then: 総試行回数は初回の 1 回だけになる
    #[test]
    fn no_retry_allows_only_the_initial_attempt() {
        let policy = RetryPolicy::no_retry();

        assert_eq!(policy.max_attempts, 1);
    }

    // Given: デフォルトの再試行ポリシー / When: 各失敗試行の待機時間を求める / Then: 指数バックオフは上限で飽和する
    #[test]
    fn backoff_for_grows_until_the_configured_cap() {
        let policy = RetryPolicy::default();

        assert_eq!(policy.backoff_for(0), Duration::from_millis(250));
        assert_eq!(policy.backoff_for(1), Duration::from_millis(500));
        assert_eq!(policy.backoff_for(2), Duration::from_secs(1));
        assert_eq!(policy.backoff_for(3), Duration::from_secs(2));
        assert_eq!(policy.backoff_for(4), Duration::from_secs(2));
    }

    // Given: デフォルトの再試行ポリシー / When: 非常に大きい失敗試行番号の待機時間を求める / Then: 演算はオーバーフローせず上限に飽和する
    #[test]
    fn backoff_for_saturates_for_large_attempts() {
        let policy = RetryPolicy::default();

        assert_eq!(policy.backoff_for(u32::MAX), Duration::from_secs(2));
    }

    // Given: 各 ProviderError バリアント / When: 再試行可否を分類 / Then: 一時障害だけが再試行可能になる
    #[test]
    fn is_retryable_classifies_every_provider_error_variant() {
        let retryable = [
            ProviderError::Timeout,
            ProviderError::Transport {
                message: "connection reset".to_string(),
            },
            ProviderError::Http {
                status: 408,
                body: String::new(),
            },
            ProviderError::Http {
                status: 500,
                body: String::new(),
            },
            ProviderError::Http {
                status: 503,
                body: String::new(),
            },
            ProviderError::Http {
                status: 599,
                body: String::new(),
            },
        ];
        let non_retryable = [
            ProviderError::Http {
                status: 400,
                body: String::new(),
            },
            ProviderError::Http {
                status: 401,
                body: String::new(),
            },
            ProviderError::Http {
                status: 403,
                body: String::new(),
            },
            ProviderError::RateLimited { retry_after: None },
            ProviderError::InvalidSse {
                detail: String::new(),
            },
            ProviderError::InvalidJson {
                detail: String::new(),
            },
            ProviderError::Request(String::new()),
            ProviderError::RetriesExhausted {
                attempts: 3,
                last: Box::new(ProviderError::Timeout),
            },
        ];

        assert!(retryable.iter().all(is_retryable));
        assert!(non_retryable.iter().all(|error| !is_retryable(error)));
    }
}
