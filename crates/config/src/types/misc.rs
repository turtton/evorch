//! 診断・権限・メトリクスに関する設定型を定義します。

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// 診断 (ログ出力) の設定。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
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
#[serde(default)]
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
#[serde(default)]
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
