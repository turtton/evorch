//! SQLite データベース接続を管理します。

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, types::FromSql};

use crate::migrations::apply_migrations;
use crate::{StorageConfig, StorageError};

const BUSY_TIMEOUT: Duration = Duration::from_millis(5_000);

/// 初期化済み SQLite 接続を所有します。
#[derive(Debug)]
pub struct Database {
    pub(crate) conn: Connection,
}

impl Database {
    /// ファイル上のデータベースを開き、接続設定と移行を適用します。
    pub fn open(config: &StorageConfig) -> Result<Self, StorageError> {
        let conn = Connection::open(&config.db_path)?;
        conn.busy_timeout(BUSY_TIMEOUT)?;
        pragma_init(&conn)?;
        apply_migrations(&conn)?;
        Ok(Self { conn })
    }

    /// メモリ上のデータベースを開き、接続設定と移行を適用します。
    pub fn open_in_memory() -> Result<Self, StorageError> {
        let conn = Connection::open_in_memory()?;
        conn.busy_timeout(BUSY_TIMEOUT)?;
        pragma_init(&conn)?;
        apply_migrations(&conn)?;
        Ok(Self { conn })
    }

    /// 接続の文字列 PRAGMA 値を返します。
    pub fn pragma_string(&self, name: &str) -> Result<String, StorageError> {
        self.pragma_value(name)
    }

    /// 接続の整数 PRAGMA 値を返します。
    pub fn pragma_i64(&self, name: &str) -> Result<i64, StorageError> {
        self.pragma_value(name)
    }

    fn pragma_value<T: FromSql>(&self, name: &str) -> Result<T, StorageError> {
        Ok(self.conn.pragma_query_value(None, name, |row| row.get(0))?)
    }
}

fn pragma_init(conn: &Connection) -> Result<(), StorageError> {
    conn.busy_timeout(BUSY_TIMEOUT)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "wal_autocheckpoint", 1_000_i64)?;
    conn.pragma_update(None, "foreign_keys", true)?;
    Ok(())
}

#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "writer capacity checks consume this staged crate-private API in the next task"
    )
)]
pub(crate) struct FileSizes {
    pub db: u64,
    pub wal: u64,
    pub shm: u64,
}

impl FileSizes {
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "writer capacity checks consume this staged crate-private API in the next task"
        )
    )]
    pub fn total(&self) -> u64 {
        self.db.saturating_add(self.wal).saturating_add(self.shm)
    }
}

#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "writer capacity checks consume this staged crate-private API in the next task"
    )
)]
pub(crate) fn file_sizes(db_path: &Path) -> Result<FileSizes, StorageError> {
    Ok(FileSizes {
        db: file_size(db_path)?,
        wal: file_size(&suffixed_path(db_path, "-wal"))?,
        shm: file_size(&suffixed_path(db_path, "-shm"))?,
    })
}

#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "called by the staged file_sizes API before its writer consumer lands"
    )
)]
fn file_size(path: &Path) -> Result<u64, StorageError> {
    match fs::metadata(path) {
        Ok(metadata) => Ok(metadata.len()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(error) => Err(StorageError::Io(error.to_string())),
    }
}

/// ADR 0012 自己参照防止 — 将来のファイルウォッチャーはこれらを除外すること。
pub fn watch_exclusions(db_path: &Path) -> Vec<PathBuf> {
    let absolute = std::path::absolute(db_path).unwrap_or_else(|_| db_path.to_path_buf());
    vec![
        absolute.clone(),
        suffixed_path(&absolute, "-wal"),
        suffixed_path(&absolute, "-shm"),
        suffixed_path(&absolute, "-journal"),
    ]
}

fn suffixed_path(path: &Path, suffix: &str) -> PathBuf {
    let mut value = OsString::from(path.as_os_str());
    value.push(suffix);
    PathBuf::from(value)
}

#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "event persistence consumes this staged crate-private API in the next task"
    )
)]
pub(crate) fn system_time_to_ns(time: SystemTime) -> Result<i64, StorageError> {
    let nanos = time
        .duration_since(UNIX_EPOCH)
        .map_err(|_| StorageError::OutOfRange("wall_clock before epoch"))?
        .as_nanos();
    i64::try_from(nanos).map_err(|_| StorageError::OutOfRange("wall_clock nanoseconds"))
}

#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "event persistence consumes this staged crate-private API in the next task"
    )
)]
pub(crate) fn ns_to_system_time(ns: i64) -> SystemTime {
    if ns >= 0 {
        UNIX_EPOCH + Duration::from_nanos(ns.unsigned_abs())
    } else {
        UNIX_EPOCH - Duration::from_nanos(ns.unsigned_abs())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;

    #[test]
    fn watch_exclusions_returns_absolute_sqlite_paths() {
        // Given: 相対パスのデータベース名
        let path = Path::new("data/storage.db");

        // When: 監視除外パスを生成する
        let exclusions = watch_exclusions(path);

        // Then: DB と SQLite 副ファイルの絶対パスが順番どおり返る
        assert!(exclusions.iter().all(|entry| entry.is_absolute()));
        assert_eq!(exclusions[0].file_name().unwrap(), "storage.db");
        assert_eq!(exclusions[1].file_name().unwrap(), "storage.db-wal");
        assert_eq!(exclusions[2].file_name().unwrap(), "storage.db-shm");
        assert_eq!(exclusions[3].file_name().unwrap(), "storage.db-journal");
    }

    #[test]
    fn file_sizes_counts_existing_files_and_ignores_missing_files() {
        // Given: DB と WAL のみが存在する一時ディレクトリ
        let temp_dir = tempfile::tempdir().expect("temporary directory must be created");
        let path = temp_dir.path().join("storage.db");
        File::create(&path)
            .expect("database file must be created")
            .set_len(11)
            .expect("database size must be set");
        File::create(suffixed_path(&path, "-wal"))
            .expect("WAL file must be created")
            .set_len(7)
            .expect("WAL size must be set");

        // When: SQLite 関連ファイルのサイズを取得する
        let sizes = file_sizes(&path).expect("file sizes must be readable");

        // Then: 存在するファイルだけを合計する
        assert_eq!((sizes.db, sizes.wal, sizes.shm), (11, 7, 0));
        assert_eq!(sizes.total(), 18);
    }

    #[test]
    fn system_time_nanoseconds_round_trip() {
        // Given: epoch 後のナノ秒精度の時刻
        let time = UNIX_EPOCH + Duration::new(1_234, 567_890_123);

        // When: ナノ秒整数へ変換して時刻へ戻す
        let restored = ns_to_system_time(system_time_to_ns(time).expect("time must fit"));

        // Then: 元の時刻と一致する
        assert_eq!(restored, time);
    }

    #[test]
    fn system_time_before_epoch_is_rejected() {
        // Given: epoch より一ナノ秒前の時刻
        let time = UNIX_EPOCH - Duration::from_nanos(1);

        // When: ナノ秒整数へ変換する
        let error = system_time_to_ns(time).expect_err("pre-epoch time must fail");

        // Then: wall clock の範囲外エラーになる
        assert_eq!(error, StorageError::OutOfRange("wall_clock before epoch"));
    }
}
