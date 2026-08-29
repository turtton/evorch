//! 読み込んだ設定値のバージョン間マイグレーション (ADR 0014)。
//!
//! 現時点では v1 → v2 の変換チェーンは未実装であり、バージョン検証
//! (現行より新しいバージョンの拒否) のみを行う。

use crate::error::ConfigError;
use crate::types::CURRENT_VERSION;

/// 読み込んだ 1 ファイル分の設定値を現在のスキーマへマイグレーションする。
///
/// ファイル単位・マージ前 (deep_merge の直前) にかならず通すシームであり、
/// `version` キーは次の規則で扱う:
///
/// - ルートがテーブルでない、または `version` キーが存在しない場合は
///   現行バージョン ([`CURRENT_VERSION`]) とみなしてそのまま返す。
/// - `version` が [`CURRENT_VERSION`] より大きい場合は
///   [`ConfigError::UnsupportedVersion`] を返す。
/// - それ以外の場合はそのまま返す (現行では変換不要)。
///
/// # Errors
///
/// - `version` キーが 0 以上の整数として読み取れない場合
///   ([`ConfigError::Migration`])。
/// - `version` が [`CURRENT_VERSION`] より大きい場合
///   ([`ConfigError::UnsupportedVersion`])。
pub fn run(value: toml::Value) -> Result<toml::Value, ConfigError> {
    let version = match value.as_table().and_then(|table| table.get("version")) {
        None => CURRENT_VERSION,
        Some(raw_version) => match raw_version
            .as_integer()
            .and_then(|raw| u32::try_from(raw).ok())
        {
            Some(version) => version,
            None => {
                return Err(ConfigError::Migration(format!(
                    "version key must be a non-negative integer, got: {raw_version}"
                )));
            }
        },
    };

    if version > CURRENT_VERSION {
        return Err(ConfigError::UnsupportedVersion {
            found: version,
            current: CURRENT_VERSION,
        });
    }

    // TODO (次タスク): v1 → v2 の変換チェーンはここに挿入する。
    // version == 1 の場合に (ファイル単位で) キーのリネームなどの変換を行ってから
    // 返す。現行 (= 2) との直接比較ではなく match による段階的な連鎖にする。
    Ok(value)
}
