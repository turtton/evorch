//! 診断・権限・メトリクス・自己改善ドラフトに関する設定型を定義します。

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::ConfigError;

/// 診断 (ログ出力) の設定。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct DiagnosticsConfig {
    /// ログレベル (`trace`/`debug`/`info`/`warn`/`error`)。
    pub log_level: String,
    /// ログ出力ディレクトリ。未指定の場合は既定の位置を使用する。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub log_dir: Option<String>,
}

impl Default for DiagnosticsConfig {
    fn default() -> Self {
        Self {
            log_level: "info".to_string(),
            log_dir: None,
        }
    }
}

/// 権限プリセットの設定。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct PermissionConfig {
    /// 権限プリセット名。
    pub preset: String,
}

impl Default for PermissionConfig {
    fn default() -> Self {
        Self {
            preset: "default".to_string(),
        }
    }
}

/// メトリクス収集の設定。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct MetricsConfig {
    /// メトリクス収集の有効フラグ。
    pub enabled: bool,
    /// メトリクスの保持日数。
    pub retention_days: u32,
}

impl Default for MetricsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            retention_days: 30,
        }
    }
}

/// 自己改善ドラフト生成の設定 (Phase A: 候補と下書きのみ。既定で無効)。
///
/// `enabled = false` の場合は候補収集・ドラフト生成を含め完全に停止する。
/// Phase A はローカルの下書きのみを生成し、GitHub issue の自動作成・公開は行わない。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct SelfImprovementConfig {
    /// 自己改善ドラフト生成を有効化する。既定 false (無効)。
    pub enabled: bool,
    /// ドラフト出力ディレクトリ。None の場合は runtime 側で既定位置 (storage dir 配下) に解決する。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub draft_dir: Option<String>,
    /// プロジェクトあたりの候補保持上限。1..=10_000、既定 200。
    pub max_candidates: u32,
    /// 候補 1 件あたりの evidence 最大バイト数。256..=65_536、既定 2048。
    pub evidence_max_bytes: u32,
    /// 1 日あたりの新規候補作成上限 (UTC 日替わり)。1..=1_000、既定 20。
    pub daily_limit: u32,
    /// 同一 dedup_key の再作成クールダウン秒数。60..=31_536_000、既定 86_400。
    pub duplicate_cooldown_secs: u64,
    /// Diagnostic イベントからの候補収集。既定 true (enabled=false なら何もしない)。
    pub collect_diagnostics: bool,
    /// 承認済み lesson からの候補収集。既定 true (enabled=false なら何もしない)。
    pub collect_lessons: bool,
}

impl Default for SelfImprovementConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            draft_dir: None,
            max_candidates: 200,
            evidence_max_bytes: 2048,
            daily_limit: 20,
            duplicate_cooldown_secs: 86_400,
            collect_diagnostics: true,
            collect_lessons: true,
        }
    }
}

impl SelfImprovementConfig {
    /// 無効時も含め、候補保持・収集上限が許容範囲内であることを検証する。
    ///
    /// # Errors
    /// 範囲外の値は完全な設定パス付きの [`ConfigError::InvalidField`] として返す。
    pub fn validate(&self) -> Result<(), ConfigError> {
        for (field, value, minimum, maximum) in [
            ("max_candidates", u64::from(self.max_candidates), 1, 10_000),
            (
                "evidence_max_bytes",
                u64::from(self.evidence_max_bytes),
                256,
                65_536,
            ),
            ("daily_limit", u64::from(self.daily_limit), 1, 1_000),
            (
                "duplicate_cooldown_secs",
                self.duplicate_cooldown_secs,
                60,
                31_536_000,
            ),
        ] {
            if !(minimum..=maximum).contains(&value) {
                return Err(ConfigError::InvalidField {
                    path: format!("self_improvement.{field}"),
                    message: format!("must be in {minimum}..={maximum} (inclusive)"),
                });
            }
        }
        Ok(())
    }
}
