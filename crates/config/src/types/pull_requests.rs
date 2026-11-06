//! `[pull_requests]` セクション（スレッドに紐づいた PR の追跡、ADR 0043）の設定型。
//!
//! `gh` を sandbox の外で実行する機能なので、`comment_checker` / `tools` と同じ信頼済み
//! セクションとして扱う（既定値とユーザー層だけが設定でき、プロジェクト層・環境変数・
//! CLI 上書きでは有効化できない）。既定は無効で、無効の間は取得も追跡も行わない。

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::ConfigError;

/// `pending_poll_seconds` の許容範囲（秒）。GitHub API の rate limit を守る下限と、
/// 「pending」の意味を失わない上限。
pub const PENDING_POLL_SECONDS_RANGE: std::ops::RangeInclusive<u64> = 5..=3_600;
/// `stable_poll_seconds` の許容範囲（秒）。
pub const STABLE_POLL_SECONDS_RANGE: std::ops::RangeInclusive<u64> = 30..=86_400;
/// `review_max_age_seconds` の許容範囲（秒）。0 は審査のたびに再取得する。
pub const REVIEW_MAX_AGE_SECONDS_RANGE: std::ops::RangeInclusive<u64> = 0..=3_600;
/// `review_refresh_timeout_seconds` の許容範囲（秒）。
pub const REVIEW_REFRESH_TIMEOUT_SECONDS_RANGE: std::ops::RangeInclusive<u64> = 1..=60;
/// `fetch_timeout_seconds` の許容範囲（秒）。
pub const FETCH_TIMEOUT_SECONDS_RANGE: std::ops::RangeInclusive<u64> = 1..=300;
/// `backoff_max_seconds` の許容範囲（秒）。
pub const BACKOFF_MAX_SECONDS_RANGE: std::ops::RangeInclusive<u64> = 30..=86_400;

/// PR 追跡の利用者設定。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct PullRequestsConfig {
    /// PR の紐づけ・取得・追跡を行う。既定 false（opt-in）。false の間は何も取得しない。
    pub enabled: bool,
    /// 自動修正通知の全体スイッチ。既定 true。PR ごとの opt-in は別に必要。
    pub auto_fix_enabled: bool,
    /// 取得した PR の事実を escalation 審査の入力に含める。既定 true。
    pub include_in_review: bool,
    /// check が pending の PR を取得し直す間隔（秒）。5..=3600、既定 30。
    pub pending_poll_seconds: u64,
    /// check が安定している PR を取得し直す間隔（秒）。30..=86400、既定 300。
    /// `pending_poll_seconds` 以上でなければならない。
    pub stable_poll_seconds: u64,
    /// 審査時に、追跡結果をそのまま使える取得からの経過時間（秒）。0..=3600、既定 60。
    /// これより古ければ短い期限で再取得する。
    pub review_max_age_seconds: u64,
    /// 審査時の再取得の期限（秒）。1..=60、既定 3。`fetch_timeout_seconds` 以下でなければならない。
    pub review_refresh_timeout_seconds: u64,
    /// 1 回の取得（`gh` の呼び出し全体）の期限（秒）。1..=300、既定 20。
    pub fetch_timeout_seconds: u64,
    /// 取得失敗時の指数 backoff の上限（秒）。30..=86400、既定 1800。
    /// `stable_poll_seconds` 以上でなければならない。
    pub backoff_max_seconds: u64,
}

impl Default for PullRequestsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            auto_fix_enabled: true,
            include_in_review: true,
            pending_poll_seconds: 30,
            stable_poll_seconds: 300,
            review_max_age_seconds: 60,
            review_refresh_timeout_seconds: 3,
            fetch_timeout_seconds: 20,
            backoff_max_seconds: 1_800,
        }
    }
}

fn out_of_range(
    field: &str,
    value: u64,
    range: &std::ops::RangeInclusive<u64>,
) -> Result<(), ConfigError> {
    if range.contains(&value) {
        return Ok(());
    }
    Err(ConfigError::InvalidField {
        path: format!("pull_requests.{field}"),
        message: format!(
            "must be in {}..={} seconds (inclusive), got {value}",
            range.start(),
            range.end()
        ),
    })
}

impl PullRequestsConfig {
    /// 無効時も含め、各値が許容範囲内で互いに矛盾しないことを検証する。
    ///
    /// # Errors
    /// 範囲外の値や、`pending_poll_seconds <= stable_poll_seconds <= backoff_max_seconds`、
    /// `review_refresh_timeout_seconds <= fetch_timeout_seconds` に反する組み合わせは、
    /// 完全な設定パス付きの [`ConfigError::InvalidField`] として返す。
    pub fn validate(&self) -> Result<(), ConfigError> {
        out_of_range(
            "pending_poll_seconds",
            self.pending_poll_seconds,
            &PENDING_POLL_SECONDS_RANGE,
        )?;
        out_of_range(
            "stable_poll_seconds",
            self.stable_poll_seconds,
            &STABLE_POLL_SECONDS_RANGE,
        )?;
        out_of_range(
            "review_max_age_seconds",
            self.review_max_age_seconds,
            &REVIEW_MAX_AGE_SECONDS_RANGE,
        )?;
        out_of_range(
            "review_refresh_timeout_seconds",
            self.review_refresh_timeout_seconds,
            &REVIEW_REFRESH_TIMEOUT_SECONDS_RANGE,
        )?;
        out_of_range(
            "fetch_timeout_seconds",
            self.fetch_timeout_seconds,
            &FETCH_TIMEOUT_SECONDS_RANGE,
        )?;
        out_of_range(
            "backoff_max_seconds",
            self.backoff_max_seconds,
            &BACKOFF_MAX_SECONDS_RANGE,
        )?;
        if self.stable_poll_seconds < self.pending_poll_seconds {
            return Err(ConfigError::InvalidField {
                path: "pull_requests.stable_poll_seconds".to_owned(),
                message: "must not be shorter than pending_poll_seconds".to_owned(),
            });
        }
        if self.backoff_max_seconds < self.stable_poll_seconds {
            return Err(ConfigError::InvalidField {
                path: "pull_requests.backoff_max_seconds".to_owned(),
                message: "must not be shorter than stable_poll_seconds".to_owned(),
            });
        }
        if self.review_refresh_timeout_seconds > self.fetch_timeout_seconds {
            return Err(ConfigError::InvalidField {
                path: "pull_requests.review_refresh_timeout_seconds".to_owned(),
                message: "must not be longer than fetch_timeout_seconds".to_owned(),
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Config;

    // Given: pull_requests セクションのない設定 / When: 既定値を得る
    // Then: 無効で、審査への供給は有効、間隔は ADR 0043 の既定になり、検証を通る
    #[test]
    fn defaults_are_disabled_and_valid() {
        let default = Config::default().pull_requests;

        assert!(!default.enabled);
        assert!(default.include_in_review);
        assert_eq!(default.pending_poll_seconds, 30);
        assert_eq!(default.stable_poll_seconds, 300);
        assert_eq!(default.review_max_age_seconds, 60);
        assert_eq!(default.review_refresh_timeout_seconds, 3);
        default.validate().expect("既定値は有効");
        assert_eq!(
            toml::from_str::<Config>("").unwrap().pull_requests,
            default,
            "省略したセクションは既定値になる"
        );
    }

    // Given: 一部のキーだけの TOML / When: パースする
    // Then: 省略したキーは既定値で補完され、未知キーは拒否される
    #[test]
    fn partial_sections_fill_defaults_and_unknown_keys_are_rejected() {
        let parsed: Config =
            toml::from_str("[pull_requests]\nenabled = true\npending_poll_seconds = 10").unwrap();

        assert!(parsed.pull_requests.enabled);
        assert_eq!(parsed.pull_requests.pending_poll_seconds, 10);
        assert_eq!(parsed.pull_requests.stable_poll_seconds, 300);
        assert!(toml::from_str::<Config>("[pull_requests]\npoll = 1").is_err());
    }

    // Given: 範囲外、または互いに矛盾する値 / When: 検証する
    // Then: 完全なパス付きで拒否される
    #[test]
    fn out_of_range_and_inconsistent_values_are_rejected() {
        let cases: [(PullRequestsConfig, &str); 8] = [
            (
                PullRequestsConfig {
                    pending_poll_seconds: 4,
                    ..Default::default()
                },
                "pull_requests.pending_poll_seconds",
            ),
            (
                PullRequestsConfig {
                    stable_poll_seconds: 86_401,
                    backoff_max_seconds: 86_400,
                    ..Default::default()
                },
                "pull_requests.stable_poll_seconds",
            ),
            (
                PullRequestsConfig {
                    review_max_age_seconds: 3_601,
                    ..Default::default()
                },
                "pull_requests.review_max_age_seconds",
            ),
            (
                PullRequestsConfig {
                    review_refresh_timeout_seconds: 0,
                    ..Default::default()
                },
                "pull_requests.review_refresh_timeout_seconds",
            ),
            (
                PullRequestsConfig {
                    fetch_timeout_seconds: 301,
                    ..Default::default()
                },
                "pull_requests.fetch_timeout_seconds",
            ),
            (
                PullRequestsConfig {
                    pending_poll_seconds: 60,
                    stable_poll_seconds: 30,
                    ..Default::default()
                },
                "pull_requests.stable_poll_seconds",
            ),
            (
                PullRequestsConfig {
                    backoff_max_seconds: 100,
                    ..Default::default()
                },
                "pull_requests.backoff_max_seconds",
            ),
            (
                PullRequestsConfig {
                    review_refresh_timeout_seconds: 30,
                    fetch_timeout_seconds: 20,
                    ..Default::default()
                },
                "pull_requests.review_refresh_timeout_seconds",
            ),
        ];
        for (config, expected_path) in cases {
            match config.validate() {
                Err(ConfigError::InvalidField { path, .. }) => assert_eq!(path, expected_path),
                other => panic!("{expected_path}: {other:?}"),
            }
        }
    }
}
