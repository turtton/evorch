//! 読み込んだ設定値のバージョン間マイグレーション (ADR 0014)。
//!
use crate::error::ConfigError;
use crate::merge::deep_merge;
use crate::types::{CURRENT_VERSION, MetricsConfig};

type MigrationResult = Result<toml::Value, ConfigError>;

/// バージョン間の変換関数列。
///
/// インデックス `i` の関数は v`(i + 1)` から v`(i + 2)` へ変換する。
const MIGRATIONS: &[fn(toml::Value) -> MigrationResult] = &[migrate_v1_to_v2];

/// 読み込んだ 1 ファイル分の設定値を現在のスキーマへマイグレーションする。
///
/// ファイル単位・マージ前 (deep_merge の直前) にかならず通すシームであり、
/// `version` キーは次の規則で扱う:
///
/// - ルートがテーブルでない、または `version` キーが存在しない場合は
///   現行バージョン ([`CURRENT_VERSION`]) とみなしてそのまま返す。
/// - `version` が [`CURRENT_VERSION`] より大きい場合は
///   [`ConfigError::UnsupportedVersion`] を返す。
/// - 過去のバージョンの場合は、対応する変換を順に適用する。
///
/// v1 → v2 は `[metrics]` セクションを追加する。既存の `[metrics]` 値は維持し、
/// 未指定のキーだけを [`MetricsConfig::default`] の値で補完する。
/// 変換列のインデックス `i` は v`(i + 1)` → v`(i + 2)` に対応する。
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
    if version == 0 {
        return Err(ConfigError::Migration(
            "version key must be at least 1".to_string(),
        ));
    }

    let migrations = &MIGRATIONS[version as usize - 1..];
    let mut migrated = value;
    for migration in migrations {
        migrated = migration(migrated)?;
    }

    if let Some(table) = migrated.as_table_mut() {
        table.insert(
            "version".to_string(),
            toml::Value::Integer(CURRENT_VERSION.into()),
        );
    }
    Ok(migrated)
}

/// v1 の設定へ v2 で追加された metrics 既定値を補完する。
///
/// 既存の `[metrics]` テーブルは既定値へ深マージするため、ユーザが指定したキーを
/// 優先しながら、未指定のキーのみを追加する。
///
/// # Errors
///
/// [`MetricsConfig::default`] の TOML への直列化または再パースに失敗した場合に
/// [`ConfigError::Migration`] を返す。型定義が TOML 往復可能である限り発生しない。
fn migrate_v1_to_v2(mut value: toml::Value) -> Result<toml::Value, ConfigError> {
    let serialized = toml::to_string(&MetricsConfig::default()).map_err(|err| {
        ConfigError::Migration(format!("failed to serialize default metrics config: {err}"))
    })?;
    let default_metrics: toml::Value = toml::from_str(serialized.as_str()).map_err(|err| {
        ConfigError::Migration(format!("failed to parse default metrics config: {err}"))
    })?;

    let table = value.as_table_mut().ok_or_else(|| {
        ConfigError::Migration("versioned config root must be a table".to_string())
    })?;
    let metrics = table
        .remove("metrics")
        .map_or(default_metrics.clone(), |existing| {
            deep_merge(default_metrics, existing)
        });
    table.insert("metrics".to_string(), metrics);
    table.insert("version".to_string(), toml::Value::Integer(2));
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Given: metrics セクションを持たないバージョン 1 の設定値 / When: マイグレーションする
    // Then: バージョン 2 と既定の metrics テーブルを持つ値になる
    #[test]
    fn v1_migration_adds_default_metrics() {
        let migrated = run(toml::toml! {
            version = 1
        }
        .into())
        .expect("バージョン 1 の設定を移行できる");

        assert_eq!(
            migrated.get("version").and_then(toml::Value::as_integer),
            Some(2)
        );
        let metrics = migrated
            .get("metrics")
            .and_then(toml::Value::as_table)
            .expect("metrics テーブルが追加される");
        assert_eq!(
            metrics.get("enabled").and_then(toml::Value::as_bool),
            Some(true)
        );
        assert_eq!(
            metrics
                .get("retention_days")
                .and_then(toml::Value::as_integer),
            Some(30)
        );
    }
}
