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
    /// 設定値に許可されていないフィールド (未知キー / 平文 credential) が現れた。
    #[error("invalid config field `{path}`: {message}")]
    InvalidField {
        /// ドット区切りの config path (例: `providers.foo.api_key`)。
        path: String,
        /// 拒否理由と remediation 案内。
        message: String,
    },
    /// 環境変数の値が不正だった。
    #[error("invalid value for environment variable {key}: {value}")]
    InvalidEnvValue {
        /// 環境変数名。
        key: String,
        /// 不正だった値。
        value: String,
    },
    /// agents セクションのバインディング解決で固定 4 ロール外のロール名が指定された。
    #[error(
        "unknown agent role `{role}`, expected one of: orchestrator, explorer, worker, reviewer"
    )]
    UnknownAgentRole {
        /// 指定されたロール名。
        role: String,
    },
    /// agents セクションのバインディング解決で固定 6 カテゴリ外のカテゴリ名が指定された。
    #[error(
        "unknown agent category `{category}` for role `{role}`, expected one of: quick, deep, high-reasoning, visual, writing, research"
    )]
    UnknownCategory {
        /// カテゴリを要求したロール名。
        role: String,
        /// 指定されたカテゴリ名。
        category: String,
    },
    /// 指定された名前のプリセットが同梱・ユーザーのどちらにも存在しない。
    #[error("preset not found: `{name}`")]
    PresetNotFound {
        /// 見つからなかったプリセット名。
        name: String,
    },
    /// プリセット名が許容形式 `[a-z0-9-]{1,64}` に一致しない。
    #[error("invalid preset name `{name}`, expected [a-z0-9-]{{1,64}}")]
    PresetNameInvalid {
        /// 不正だったプリセット名。
        name: String,
    },
    /// プリセットファイルがサイズ上限 (64 KiB) を超えている。
    #[error("preset file `{path}` is too large: {size} bytes (limit 65536)")]
    PresetTooLarge {
        /// サイズ超過だったファイルのパス。
        path: PathBuf,
        /// 実際のファイルサイズ (バイト)。
        size: u64,
    },
    /// プリセットファイルが UTF-8 として読み取れない。
    #[error("preset file `{path}` is not valid UTF-8")]
    PresetNotUtf8 {
        /// 読み取れなかったファイルのパス。
        path: PathBuf,
    },
}
