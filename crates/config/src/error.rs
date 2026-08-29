//! 設定関連の操作で返すエラーを定義します。

use std::path::PathBuf;

/// 設定関連の操作で発生するエラー。
///
/// [`std::error::Error`] は thiserror により自動実装される。
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// 設定ファイルの入出力に失敗した。
    #[error("config I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// 設定ファイルのパース (TOML デコード) に失敗した。
    #[error("failed to parse config {path}: {source}")]
    Parse {
        /// パース対象だったファイルのパス。
        path: PathBuf,
        /// パース失敗の原因となったエラー。
        source: toml::de::Error,
    },
    /// サポート対象外の設定バージョンを読み込んだ (ADR 0014)。
    #[error("unsupported config version: found {found}, current {current}")]
    UnsupportedVersion {
        /// 読み込んだファイルのバージョン。
        found: u32,
        /// 現在のスキーマバージョン。
        current: u32,
    },
    /// 設定のマイグレーションに失敗した。
    #[error("config migration failed: {0}")]
    Migration(String),
    /// 環境変数の値が不正だった。
    #[error("invalid value for environment variable {key}: {value}")]
    InvalidEnvValue {
        /// 環境変数名。
        key: String,
        /// 不正だった値。
        value: String,
    },
}
