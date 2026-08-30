//! ストレージ層の容量制限と実行時設定を定義します。

use std::path::PathBuf;
use std::time::Duration;

/// 超過を検査するストレージ容量上限の種別です。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitKind {
    /// 単一イベントの最大バイト数です。
    EventSize,
    /// 単一セッションの最大累積バイト数です。
    SessionSize,
    /// 一日あたりのイベント最大累積バイト数です。
    DailyBytes,
    /// WAL ファイルの最大バイト数です。
    WalSize,
    /// データベースファイルの最大バイト数です。
    DbSize,
}

/// ストレージに適用する容量上限です。
#[derive(Debug, Clone, PartialEq)]
pub struct HardLimits {
    /// 単一イベントの最大バイト数です。
    pub max_event_bytes: u64,
    /// 単一セッションの最大累積バイト数です。
    pub max_session_bytes: u64,
    /// 一日あたりのイベント最大累積バイト数です。
    pub max_daily_event_bytes: u64,
    /// WAL ファイルの最大バイト数です。
    pub max_wal_bytes: u64,
    /// データベースファイルの最大バイト数です。
    pub max_db_bytes: u64,
    /// 警告を開始する上限に対する比率です。
    pub soft_warn_ratio: f64,
}

impl Default for HardLimits {
    fn default() -> Self {
        Self {
            max_event_bytes: 262_144,
            max_session_bytes: 67_108_864,
            max_daily_event_bytes: 268_435_456,
            max_wal_bytes: 67_108_864,
            max_db_bytes: 1_073_741_824,
            soft_warn_ratio: 0.8,
        }
    }
}

/// ストレージ層の実行時設定です。
#[derive(Debug, Clone, PartialEq)]
pub struct StorageConfig {
    /// SQLite データベースファイルのパスです。
    pub db_path: PathBuf,
    /// 適用する容量上限です。
    pub hard_limits: HardLimits,
    /// 書き込みチャネルのバッファ容量です。
    pub channel_capacity: usize,
    /// 保留中の書き込みをフラッシュする間隔です。
    pub flush_interval: Duration,
    /// 一度のフラッシュで処理する最大保留件数です。
    pub flush_max_pending: usize,
    /// WAL チェックポイントを実行する間隔です。
    pub checkpoint_interval: Duration,
    /// freelist page 数がこの値以上の tick で `PRAGMA incremental_vacuum` を実行する閾値です。
    pub vacuum_freelist_threshold_pages: u64,
    /// 1 maintenance tick あたりに `PRAGMA incremental_vacuum(N)` へ渡す page budget です。
    /// 0 を指定すると freelist 回収を無効化します。
    pub vacuum_page_budget_per_tick: u64,
    /// SQLite 管理 temp 副産物（rollback journal `<db>-journal`）の合計バイト数が
    /// この値以上で警告を出す閾値です。警告は閾値の超過/復帰の遷移時にのみ 1 回出ます。
    pub temp_warn_bytes: u64,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            db_path: PathBuf::from("evorch.db"),
            hard_limits: HardLimits::default(),
            channel_capacity: 1_024,
            flush_interval: Duration::from_secs(5),
            flush_max_pending: 64,
            checkpoint_interval: Duration::from_secs(60),
            vacuum_freelist_threshold_pages: 1_024,
            vacuum_page_budget_per_tick: 256,
            temp_warn_bytes: 268_435_456,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::Duration;

    #[test]
    fn hard_limits_default_uses_documented_limits() {
        // Given: 既定の容量制限
        // When: 既定値を生成する
        let limits = HardLimits::default();

        // Then: すべての上限値が仕様どおりである
        assert_eq!(limits.max_event_bytes, 262_144);
        assert_eq!(limits.max_session_bytes, 67_108_864);
        assert_eq!(limits.max_daily_event_bytes, 268_435_456);
        assert_eq!(limits.max_wal_bytes, 67_108_864);
        assert_eq!(limits.max_db_bytes, 1_073_741_824);
        assert_eq!(limits.soft_warn_ratio, 0.8);
    }

    #[test]
    fn storage_config_default_uses_documented_values() {
        // Given: 既定のストレージ設定
        // When: 既定値を生成する
        let config = StorageConfig::default();

        // Then: パス、キュー、および各間隔が仕様どおりである
        assert_eq!(config.db_path, PathBuf::from("evorch.db"));
        assert_eq!(config.hard_limits, HardLimits::default());
        assert_eq!(config.channel_capacity, 1_024);
        assert_eq!(config.flush_interval, Duration::from_secs(5));
        assert_eq!(config.flush_max_pending, 64);
        assert_eq!(config.checkpoint_interval, Duration::from_secs(60));
        assert_eq!(config.vacuum_freelist_threshold_pages, 1_024);
        assert_eq!(config.vacuum_page_budget_per_tick, 256);
        assert_eq!(config.temp_warn_bytes, 268_435_456);
    }
}
