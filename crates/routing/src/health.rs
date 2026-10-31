//! 失敗したプロファイル・モデルを一定期間候補から外すクールダウンを管理します (ADR 0030)。

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::FailureKind;

/// (プロファイル名, 実モデル ID)。モデル `None` はプロファイル全体。
type CooldownKey = (String, Option<String>);

/// 記録されたクールダウン 1 件分。観測イベントの発行に使う。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cooldown {
    /// 止めたプロファイル名。
    pub profile: String,
    /// 止めた実モデル ID。プロファイル全体なら `None`。
    pub model: Option<String>,
    /// クールダウンの長さ。runtime の再構成まで続く場合は `None`。
    pub duration: Option<Duration>,
}

/// プロファイル単位と (プロファイル, モデル) 単位のクールダウン期限。
///
/// 支払い・利用上限と認証エラーはプロファイル全体、レート制限は
/// (プロファイル, モデル) を止める。期限 `None` は runtime の再構成まで続く。
/// 状態は永続化せず、runtime の再構成で作り直される。
#[derive(Debug, Default)]
pub struct ProviderHealth {
    policy: config::CooldownConfig,
    /// 対象 → 期限。
    until: Mutex<BTreeMap<CooldownKey, Option<Instant>>>,
}

impl ProviderHealth {
    /// 既定期間を設定から受け取って空のレジストリを作ります。
    pub fn new(policy: config::CooldownConfig) -> Self {
        Self {
            policy,
            until: Mutex::default(),
        }
    }

    /// 失敗を記録し、クールダウンを開始・延長した場合はその内容を返します。
    ///
    /// Retry-After が示されていればそれを優先します。サーバーエラー・timeout など
    /// 一時的な失敗はクールダウンしません。既存の期限より短い期限では縮めません。
    pub fn record_failure(
        &self,
        profile: &str,
        model: &str,
        failure: FailureKind,
        retry_after: Option<Duration>,
        now: Instant,
    ) -> Option<Cooldown> {
        let (model, duration) = match failure {
            FailureKind::Quota => (
                None,
                Some(retry_after.unwrap_or(Duration::from_secs(self.policy.quota_secs))),
            ),
            FailureKind::Auth => (None, None),
            FailureKind::RateLimited => (
                Some(model.to_owned()),
                Some(retry_after.unwrap_or(Duration::from_secs(self.policy.rate_limit_secs))),
            ),
            FailureKind::Server | FailureKind::Timeout | FailureKind::Other => return None,
        };
        Some(self.record(profile, model, duration, now))
    }

    /// Pause one capability-local candidate; callers attach this registry only
    /// to that capability's router, leaving chat and other models available.
    pub fn record_candidate_failure(
        &self,
        profile: &str,
        model: &str,
        duration: Duration,
        now: Instant,
    ) -> Cooldown {
        self.record(profile, Some(model.to_owned()), Some(duration), now)
    }

    fn record(
        &self,
        profile: &str,
        model: Option<String>,
        duration: Option<Duration>,
        now: Instant,
    ) -> Cooldown {
        // 期限に加算できない遅延 (Retry-After が極端に大きい場合) は、panic させず
        // 再構成まで続くクールダウンとして扱う。
        let deadline = duration.and_then(|duration| now.checked_add(duration));
        let duration = duration.filter(|_| deadline.is_some());
        let mut until = self
            .until
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = until
            .entry((profile.to_owned(), model.clone()))
            .or_insert(deadline);
        *entry = match (*entry, deadline) {
            (None, _) | (_, None) => None,
            (Some(current), Some(next)) => Some(current.max(next)),
        };
        Cooldown {
            profile: profile.to_owned(),
            model,
            duration,
        }
    }

    /// プロファイル全体、またはその実モデルがクールダウン中かを返します。
    pub fn is_cooling_down(&self, profile: &str, model: &str, now: Instant) -> bool {
        let until = self
            .until
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        [
            (profile.to_owned(), None),
            (profile.to_owned(), Some(model.to_owned())),
        ]
        .iter()
        .filter_map(|key| until.get(key))
        .any(|deadline| deadline.is_none_or(|deadline| now < deadline))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn health() -> ProviderHealth {
        ProviderHealth::new(config::CooldownConfig {
            quota_secs: 600,
            rate_limit_secs: 30,
        })
    }

    // Given: Instant に加算できない Retry-After (u64::MAX 秒) の rate limit と quota
    // When: 失敗を記録して判定する / Then: panic せず、再構成まで続くクールダウンになる
    #[test]
    fn unrepresentable_retry_after_becomes_an_indefinite_cooldown() {
        let health = health();
        let now = Instant::now();
        let huge = Some(Duration::from_secs(u64::MAX));

        let limited = health.record_failure("codex", "gpt", FailureKind::RateLimited, huge, now);
        let quota = health.record_failure("kimi", "k3", FailureKind::Quota, huge, now);

        for (cooldown, model) in [(limited, Some("gpt".to_owned())), (quota, None)] {
            let cooldown = cooldown.expect("cooldown recorded");
            assert_eq!(cooldown.model, model);
            assert_eq!(cooldown.duration, None);
        }
        assert!(health.is_cooling_down("codex", "gpt", now + Duration::from_secs(86_400 * 365)));
        assert!(health.is_cooling_down("kimi", "any", now + Duration::from_secs(86_400 * 365)));
    }

    // Given: quota で失敗したプロファイル / When: 期限前後で同プロファイルの別モデルを判定する
    // Then: 期限まではプロファイル全体が止まり、期限を過ぎると戻る
    #[test]
    fn quota_pauses_whole_profile_until_deadline() {
        let health = health();
        let now = Instant::now();

        let cooldown = health.record_failure("kimi", "k3", FailureKind::Quota, None, now);

        assert_eq!(
            cooldown,
            Some(Cooldown {
                profile: "kimi".into(),
                model: None,
                duration: Some(Duration::from_secs(600)),
            })
        );
        assert!(health.is_cooling_down("kimi", "kimi-for-coding", now + Duration::from_secs(599)));
        assert!(!health.is_cooling_down("kimi", "kimi-for-coding", now + Duration::from_secs(600)));
        assert!(!health.is_cooling_down("neuralwatt", "k3", now));
    }

    // Given: Retry-After 付きのレート制限 / When: 同じモデルと別モデルを判定する
    // Then: Retry-After の間だけ失敗したモデルだけが止まる
    #[test]
    fn rate_limit_pauses_only_the_model_for_retry_after() {
        let health = health();
        let now = Instant::now();

        health.record_failure(
            "codex-work",
            "gpt-6.1-sol",
            FailureKind::RateLimited,
            Some(Duration::from_secs(5)),
            now,
        );

        assert!(health.is_cooling_down("codex-work", "gpt-6.1-sol", now + Duration::from_secs(4)));
        assert!(!health.is_cooling_down("codex-work", "gpt-6.1-sol", now + Duration::from_secs(5)));
        assert!(!health.is_cooling_down("codex-work", "gpt-6-luna", now));
    }

    // Given: 認証エラーのプロファイル / When: 十分な時間が経った後に判定する
    // Then: 期限を持たず止まり続ける
    #[test]
    fn auth_pauses_profile_without_deadline() {
        let health = health();
        let now = Instant::now();

        let cooldown = health.record_failure("claude", "opus", FailureKind::Auth, None, now);

        assert_eq!(cooldown.and_then(|cooldown| cooldown.duration), None);
        assert!(health.is_cooling_down("claude", "haiku", now + Duration::from_secs(86_400)));
    }

    // Given: サーバーエラーと timeout / When: 記録する / Then: クールダウンしない
    #[test]
    fn transient_failures_do_not_pause() {
        let health = health();
        let now = Instant::now();

        assert_eq!(
            health.record_failure("p", "m", FailureKind::Server, None, now),
            None
        );
        assert_eq!(
            health.record_failure("p", "m", FailureKind::Timeout, None, now),
            None
        );
        assert!(!health.is_cooling_down("p", "m", now));
    }

    // Given: 長いクールダウン中のプロファイル / When: より短い期限で再度失敗を記録する
    // Then: 期限は縮まない
    #[test]
    fn shorter_retry_after_does_not_shorten_existing_cooldown() {
        let health = health();
        let now = Instant::now();
        health.record_failure("kimi", "k3", FailureKind::Quota, None, now);

        health.record_failure(
            "kimi",
            "k3",
            FailureKind::Quota,
            Some(Duration::from_secs(1)),
            now,
        );

        assert!(health.is_cooling_down("kimi", "k3", now + Duration::from_secs(300)));
    }
}
