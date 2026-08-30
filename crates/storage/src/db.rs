//! SQLite データベース接続を管理します。

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, types::FromSql};

use crate::migrations::apply_migrations;
use crate::repo::catalog;
use crate::{CatalogUpdateRecord, StorageConfig, StorageError};

const BUSY_TIMEOUT: Duration = Duration::from_millis(5_000);

/// SQLite の `PRAGMA auto_vacuum` で incremental モードを示す値です。
const AUTO_VACUUM_INCREMENTAL: i64 = 2;

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

    /// カタログ更新履歴を挿入順で返します。
    ///
    /// # Errors
    ///
    /// SQLite 操作または保存済みモデル数の変換に失敗した場合にエラーを返します。
    pub fn catalog_updates(&self) -> Result<Vec<CatalogUpdateRecord>, StorageError> {
        catalog::list(&self.conn)
    }

    fn pragma_value<T: FromSql>(&self, name: &str) -> Result<T, StorageError> {
        Ok(self.conn.pragma_query_value(None, name, |row| row.get(0))?)
    }
}

fn pragma_init(conn: &Connection) -> Result<(), StorageError> {
    conn.busy_timeout(BUSY_TIMEOUT)?;
    // journal_mode=WAL 等が空ファイルのヘッダ初期化で page 1 を作り得るため、
    // 「既存 DB か」の判定は他 pragma より先に行います。
    init_auto_vacuum(conn)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "wal_autocheckpoint", 1_000_i64)?;
    conn.pragma_update(None, "foreign_keys", true)?;
    Ok(())
}

/// incremental vacuum を可能にするため `auto_vacuum=INCREMENTAL` を保証します。
///
/// - 新規 DB（`page_count == 0`）: テーブル作成前に有効化します。
/// - 既存 DB の `auto_vacuum=FULL`(1): pointer-map 構造が既に存在するため、
///   INCREMENTAL への変更は full VACUUM なしで安全に適用できます（移行します）。
/// - 既存 DB の `auto_vacuum` 未設定(0): 反映に DB 全体の再書き込み（full VACUUM）
///   が必要になるため変更せず、診断ログで非アクティブであることを通知します
///   （起動時に既存接続を長時間 block する破壊的移行は行わない方針）。
fn init_auto_vacuum(conn: &Connection) -> Result<(), StorageError> {
    const AUTO_VACUUM_NONE: i64 = 0;
    const AUTO_VACUUM_FULL: i64 = 1;
    let mode: i64 = conn.pragma_query_value(None, "auto_vacuum", |row| row.get(0))?;
    if mode == AUTO_VACUUM_INCREMENTAL {
        return Ok(());
    }
    if mode == AUTO_VACUUM_FULL {
        conn.pragma_update(None, "auto_vacuum", AUTO_VACUUM_INCREMENTAL)?;
        tracing::info!("database auto_vacuum migrated: FULL -> INCREMENTAL (no rebuild required)");
        return Ok(());
    }
    debug_assert_eq!(mode, AUTO_VACUUM_NONE);
    let page_count: i64 = conn.pragma_query_value(None, "page_count", |row| row.get(0))?;
    if page_count == 0 {
        conn.pragma_update(None, "auto_vacuum", AUTO_VACUUM_INCREMENTAL)?;
    } else {
        tracing::info!(
            "existing database keeps auto_vacuum=NONE; incremental vacuum stays inactive until manual VACUUM"
        );
    }
    Ok(())
}

/// SQLite データベースファイル群（本体 / WAL / SHM）のサイズです。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileSizes {
    pub db: u64,
    pub wal: u64,
    pub shm: u64,
}

impl FileSizes {
    /// db / -wal / -shm の合計バイト数を返します。
    pub fn total(&self) -> u64 {
        self.db.saturating_add(self.wal).saturating_add(self.shm)
    }
}

/// データベース本体と WAL / SHM ファイルのサイズを返します。存在しないファイルは 0 として扱います。
pub fn file_sizes(db_path: &Path) -> Result<FileSizes, StorageError> {
    Ok(FileSizes {
        db: file_size(db_path)?,
        wal: file_size(&suffixed_path(db_path, "-wal"))?,
        shm: file_size(&suffixed_path(db_path, "-shm"))?,
    })
}

fn file_size(path: &Path) -> Result<u64, StorageError> {
    match fs::metadata(path) {
        Ok(metadata) => Ok(metadata.len()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(error) => Err(StorageError::Io(error.to_string())),
    }
}

/// SQLite 管理の temp 副産物（現行は rollback journal `<db>-journal`）の合計バイト数を返します。
/// 存在しない場合は 0 として扱います。db / -wal / -shm は `file_sizes` の責務であり二重計上しません。
/// OS 全体の temp ディレクトリ監視は対象外とし、evorch が管理する DB 同梱の副産物のみを測定します。
pub fn temp_files_bytes(db_path: &Path) -> Result<u64, StorageError> {
    file_size(&suffixed_path(db_path, "-journal"))
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

pub(crate) fn suffixed_path(path: &Path, suffix: &str) -> PathBuf {
    let mut value = OsString::from(path.as_os_str());
    value.push(suffix);
    PathBuf::from(value)
}

pub fn system_time_to_ns(time: SystemTime) -> Result<i64, StorageError> {
    let nanos = time
        .duration_since(UNIX_EPOCH)
        .map_err(|_| StorageError::OutOfRange("wall_clock before epoch"))?
        .as_nanos();
    i64::try_from(nanos).map_err(|_| StorageError::OutOfRange("wall_clock nanoseconds"))
}

pub fn ns_to_system_time(ns: i64) -> SystemTime {
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

    #[test]
    fn open_enables_incremental_auto_vacuum_on_fresh_database() {
        // Given: まだ一度も作成されていない DB パス
        let temp_dir = tempfile::tempdir().expect("temporary directory must be created");
        let db_path = temp_dir.path().join("fresh.db");
        let config = StorageConfig {
            db_path,
            ..Default::default()
        };

        // When: Database として開く
        let database = Database::open(&config).expect("fresh database must open");

        // Then: incremental auto_vacuum(2) が有効化されている
        let mode = database
            .pragma_i64("auto_vacuum")
            .expect("pragma must read");
        assert_eq!(mode, 2);
    }

    #[test]
    fn open_preserves_legacy_auto_vacuum_without_rebuilding_database() {
        // Given: auto_vacuum 未設定(0)の既存 DB を素の rusqlite 接続で用意する
        let temp_dir = tempfile::tempdir().expect("temporary directory must be created");
        let db_path = temp_dir.path().join("legacy.db");
        {
            let raw = Connection::open(&db_path).expect("legacy database must open");
            raw.execute_batch("CREATE TABLE t (x BLOB); INSERT INTO t VALUES (zeroblob(8192));")
                .expect("legacy schema must be created");
            let mode: i64 = raw
                .pragma_query_value(None, "auto_vacuum", |row| row.get(0))
                .expect("pragma must read");
            assert_eq!(mode, 0, "legacy fixture must start with auto_vacuum=0");
        }
        let config = StorageConfig {
            db_path,
            ..Default::default()
        };

        // When: Database として開く
        let database = Database::open(&config).expect("legacy database must open");

        // Then: auto_vacuum は破壊的な full VACUUM なしで 0 のまま保持される
        let mode = database
            .pragma_i64("auto_vacuum")
            .expect("pragma must read");
        assert_eq!(mode, 0, "existing database must not be force-migrated");
    }

    #[test]
    fn open_migrates_full_auto_vacuum_to_incremental_without_rebuild() {
        // Given: auto_vacuum=FULL(1) の既存 DB（pointer-map 構造を保持）
        let temp_dir = tempfile::tempdir().expect("temporary directory must be created");
        let db_path = temp_dir.path().join("legacy-full.db");
        {
            let raw = Connection::open(&db_path).expect("legacy database must open");
            raw.pragma_update(None, "auto_vacuum", 1)
                .expect("auto_vacuum must set");
            raw.execute_batch("CREATE TABLE t (x BLOB); INSERT INTO t VALUES (zeroblob(8192));")
                .expect("legacy schema must be created");
        }
        let config = StorageConfig {
            db_path: db_path.clone(),
            ..Default::default()
        };
        // 初回 open で migration を先に適用させ、この移行の影響と切り分ける
        drop(Database::open(&config).expect("first open must succeed"));
        {
            let raw = Connection::open(&db_path).expect("legacy database must open");
            raw.pragma_update(None, "auto_vacuum", 1)
                .expect("auto_vacuum must be restored to FULL");
            let mode: i64 = raw
                .pragma_query_value(None, "auto_vacuum", |row| row.get(0))
                .expect("pragma must read");
            assert_eq!(mode, 1, "fixture must re-enter auto_vacuum=FULL");
        }
        let bytes_before = std::fs::metadata(&db_path)
            .expect("db file must exist")
            .len();

        // When: FULL の DB を再び Database として開く
        let database = Database::open(&config).expect("legacy FULL database must open");

        // Then: pointer-map 互換の FULL→INCREMENTAL 変更が full VACUUM なしで適用される
        let mode = database
            .pragma_i64("auto_vacuum")
            .expect("pragma must read");
        assert_eq!(mode, 2, "FULL must migrate to INCREMENTAL");
        let bytes_after = std::fs::metadata(&db_path)
            .expect("db file must exist")
            .len();
        assert_eq!(
            bytes_after, bytes_before,
            "migration must not rewrite the database"
        );
    }

    #[test]
    fn temp_files_bytes_counts_journal_sidecar_and_ignores_missing() {
        // Given: rollback journal 副ファイルだけが存在する一時ディレクトリ
        let temp_dir = tempfile::tempdir().expect("temporary directory must be created");
        let db_path = temp_dir.path().join("storage.db");
        File::create(&db_path)
            .expect("database file must be created")
            .set_len(128)
            .expect("database size must be set");
        File::create(suffixed_path(&db_path, "-journal"))
            .expect("journal file must be created")
            .set_len(11)
            .expect("journal size must be set");

        // When: 管理対象 temp 副産物のサイズを取得する
        let bytes = temp_files_bytes(&db_path).expect("temp sizes must be readable");

        // Then: journal 副ファイルだけを合計する
        assert_eq!(bytes, 11);

        // Given: 副ファイルがない DB パス
        let other_path = temp_dir.path().join("empty.db");

        // When/Then: 0 として扱う
        let bytes = temp_files_bytes(&other_path).expect("temp sizes must be readable");
        assert_eq!(bytes, 0);
    }
}
