//! `[storage]` セクション（会話全文索引 `[storage.thread_index]`）の設定型を定義します。
//!
//! ADR 0041 Phase B（決定 B-2）。ここは利用者向けの strict な設定値・既定値・検証だけを
//! 持ちます。storage 側の `storage::ThreadIndexSettings` への写像と writer への取り込みは
//! 配線段階（順序 7）で行います。config crate は storage crate に依存しないため、
//! 既定値は `storage::DEFAULT_THREAD_INDEX_MAX_BYTES` と同じ値をここでも定義します。

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::ConfigError;

/// 会話全文索引の論理予算の既定値（512 MiB、ADR 0041 決定 B-2）。
pub const DEFAULT_THREAD_INDEX_MAX_BYTES: u64 = 512 * 1024 * 1024;
/// 会話全文索引の論理予算の下限（1 MiB）。これ未満では 1 run も保持できないため拒否する。
pub const MIN_THREAD_INDEX_MAX_BYTES: u64 = 1024 * 1024;
/// 会話全文索引の論理予算の上限（10 GiB）。storage の DB 全体の既定上限
/// （`hard_limits.max_db_bytes`）と同じで、これを超える索引予算は意味を持たない。
pub const MAX_THREAD_INDEX_MAX_BYTES: u64 = 10 * 1024 * 1024 * 1024;

/// ストレージ層の利用者設定。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct StorageConfig {
    /// 会話全文索引（`thread_grep` の索引）の設定。
    pub thread_index: ThreadIndexConfig,
}

impl StorageConfig {
    /// 無効時も含め、ストレージ設定が許容範囲内であることを検証する。
    ///
    /// # Errors
    /// 範囲外の値は完全な設定パス付きの [`ConfigError::InvalidField`] として返す。
    pub fn validate(&self) -> Result<(), ConfigError> {
        self.thread_index.validate()
    }
}

/// 会話全文索引の設定（ADR 0041 Phase B）。
///
/// `max_bytes` は物理ファイルサイズではなく、索引本文・属性・FTS 係数を含む
/// 論理会計に対する予算です。超過すると古い run（作成時刻の昇順）から索引を外します。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ThreadIndexConfig {
    /// 会話全文索引を作成・更新する。既定 true。false でも正本（会話・note）は保存される。
    pub enabled: bool,
    /// 索引の論理予算（バイト）。1 MiB..=10 GiB、既定 512 MiB。
    pub max_bytes: u64,
}

impl Default for ThreadIndexConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_bytes: DEFAULT_THREAD_INDEX_MAX_BYTES,
        }
    }
}

impl ThreadIndexConfig {
    /// 無効時も含め、`max_bytes` が許容範囲内であることを検証する。
    ///
    /// # Errors
    /// 範囲外の値は `storage.thread_index.max_bytes` の [`ConfigError::InvalidField`] として返す。
    pub fn validate(&self) -> Result<(), ConfigError> {
        if !(MIN_THREAD_INDEX_MAX_BYTES..=MAX_THREAD_INDEX_MAX_BYTES).contains(&self.max_bytes) {
            return Err(ConfigError::InvalidField {
                path: "storage.thread_index.max_bytes".to_owned(),
                message: format!(
                    "must be in {MIN_THREAD_INDEX_MAX_BYTES}..={MAX_THREAD_INDEX_MAX_BYTES} bytes (inclusive); use enabled = false to disable the index"
                ),
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Given: storage セクションのない設定 / When: 既定値を得る
    // Then: 索引は有効で、予算は 512 MiB になる
    #[test]
    fn defaults_enable_the_index_with_a_512_mib_budget() {
        let config = StorageConfig::default();

        assert!(config.thread_index.enabled);
        assert_eq!(config.thread_index.max_bytes, 536_870_912);
        config.validate().expect("既定値は有効");
    }

    // Given: 一部のキーだけを指定した TOML / When: パースする
    // Then: 省略したキーは既定値で補完される
    #[test]
    fn omitted_keys_fall_back_to_defaults() {
        let config: StorageConfig =
            toml::from_str("[thread_index]\nenabled = false\n").expect("TOML");

        assert!(!config.thread_index.enabled);
        assert_eq!(
            config.thread_index.max_bytes,
            DEFAULT_THREAD_INDEX_MAX_BYTES
        );
    }

    // Given: 下限・上限ちょうどと、その外側の max_bytes / When: 検証する
    // Then: 境界は受け入れ、外側は完全な path 付きで拒否する
    #[test]
    fn max_bytes_bounds_are_inclusive() {
        for accepted in [MIN_THREAD_INDEX_MAX_BYTES, MAX_THREAD_INDEX_MAX_BYTES] {
            let config = ThreadIndexConfig {
                enabled: true,
                max_bytes: accepted,
            };
            config.validate().expect("境界値は有効");
        }
        for rejected in [
            0,
            MIN_THREAD_INDEX_MAX_BYTES - 1,
            MAX_THREAD_INDEX_MAX_BYTES + 1,
            u64::MAX,
        ] {
            let error = ThreadIndexConfig {
                enabled: true,
                max_bytes: rejected,
            }
            .validate()
            .expect_err("範囲外は拒否");
            assert!(error.to_string().contains("storage.thread_index.max_bytes"));
        }
    }

    // Given: 索引を無効にしたうえで範囲外の max_bytes / When: 検証する
    // Then: 無効時も範囲外は拒否される（0 で無効化する書き方を許さない）
    #[test]
    fn disabled_index_still_rejects_out_of_range_budget() {
        let error = ThreadIndexConfig {
            enabled: false,
            max_bytes: 0,
        }
        .validate()
        .expect_err("無効時も範囲外は拒否");

        assert!(error.to_string().contains("enabled = false"));
    }

    // Given: 負の値・文字列・未知キー / When: パースする
    // Then: serde が拒否する
    #[test]
    fn malformed_values_are_rejected_by_serde() {
        for text in [
            "[thread_index]\nmax_bytes = -1\n",
            "[thread_index]\nmax_bytes = \"512MiB\"\n",
            "[thread_index]\nenabled = \"yes\"\n",
            "[thread_index]\nmax_byte = 1048576\n",
            "unknown = true\n",
        ] {
            assert!(
                toml::from_str::<StorageConfig>(text).is_err(),
                "{text:?} は拒否される"
            );
        }
    }
}
