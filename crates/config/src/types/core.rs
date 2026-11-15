//! Standalone core and opt-in loopback Web delivery (ADR 0045 P7a).
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct CoreConfig {
    /// Idle grace in seconds. Web delivery disables idle shutdown entirely.
    pub idle_grace_secs: u64,
    pub restart: CoreRestartConfig,
}
impl Default for CoreConfig {
    fn default() -> Self {
        Self {
            idle_grace_secs: 60,
            restart: CoreRestartConfig::default(),
        }
    }
}

impl CoreConfig {
    /// 再起動設定が許容範囲内であることを検証する。
    ///
    /// # Errors
    /// 範囲外の値は完全な設定パス付きの [`ConfigError::InvalidField`] として返す。
    pub fn validate(&self) -> Result<(), crate::ConfigError> {
        self.restart.validate()
    }
}

/// 安全な再起動 (ADR 0045 §8) の待機上限。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct CoreRestartConfig {
    /// 再起動の準備で、開始済みの learning (抽出と review を含む) の完了を待つ上限秒数。
    /// 上限を超えると再起動の要求だけを取り消し、learning は止めない。範囲は 1..=3600。
    pub learning_drain_timeout_secs: u64,
}

/// `core.restart.learning_drain_timeout_secs` の許容範囲 (両端を含む)。
pub const LEARNING_DRAIN_TIMEOUT_SECS: std::ops::RangeInclusive<u64> = 1..=3600;

impl Default for CoreRestartConfig {
    fn default() -> Self {
        Self {
            // 暫定値: 実測に基づくものではない。抽出から review の保存までの
            // 所要時間の実測 (p95 x 1.25、30 秒単位で切り上げ) で置き換える。
            learning_drain_timeout_secs: 120,
        }
    }
}

impl CoreRestartConfig {
    /// # Errors
    /// 範囲外の値は完全な設定パス付きの [`ConfigError::InvalidField`] として返す。
    pub fn validate(&self) -> Result<(), crate::ConfigError> {
        if !LEARNING_DRAIN_TIMEOUT_SECS.contains(&self.learning_drain_timeout_secs) {
            return Err(crate::ConfigError::InvalidField {
                path: "core.restart.learning_drain_timeout_secs".into(),
                message: format!(
                    "must be in {}..={} (inclusive)",
                    LEARNING_DRAIN_TIMEOUT_SECS.start(),
                    LEARNING_DRAIN_TIMEOUT_SECS.end()
                ),
            });
        }
        Ok(())
    }

    /// 準備中に learning の完了を待つ上限。
    pub fn learning_drain_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.learning_drain_timeout_secs)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct WebConfig {
    pub enabled: bool,
    pub port: u16,
    /// Extra hosts without scheme or port, e.g. evorch.tailnet-name.ts.net.
    pub allowed_hosts: Vec<String>,
    /// Extra cross-origin frontends (scheme://host[:port]); not needed for same-origin Serve.
    pub allowed_origins: Vec<String>,
}
impl Default for WebConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            port: 7878,
            allowed_hosts: Vec::new(),
            allowed_origins: Vec::new(),
        }
    }
}
