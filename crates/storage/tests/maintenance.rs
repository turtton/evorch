//! budgeted incremental vacuum と temp 容量診断の統合テストです。

use std::path::Path;

use rusqlite::Connection;
use storage::{Storage, StorageConfig};

fn freelist_pages(db_path: &Path) -> u64 {
    let conn = Connection::open(db_path).expect("side connection must open");
    conn.busy_timeout(std::time::Duration::from_millis(5_000))
        .expect("busy timeout must apply");
    let count: i64 = conn
        .pragma_query_value(None, "freelist_count", |row| row.get(0))
        .expect("freelist_count must read");
    u64::try_from(count).expect("freelist_count is never negative")
}

fn test_config(db_path: &Path) -> StorageConfig {
    // 定期 tick が assert と競合しないよう実用上無効化し、手動 tick のみで駆動する。
    StorageConfig {
        db_path: db_path.to_path_buf(),
        checkpoint_interval: std::time::Duration::from_secs(3_600),
        vacuum_freelist_threshold_pages: 3,
        vacuum_page_budget_per_tick: 2,
        temp_warn_bytes: u64::MAX,
        ..Default::default()
    }
}

#[test]
fn maintenance_ticks_reclaim_freelist_within_budget() {
    // Given: freelist を持つ fresh DB と budget 2 page/tick の設定
    let temp_dir = tempfile::tempdir().expect("temporary directory must be created");
    let db_path = temp_dir.path().join("mt.db");
    let storage = Storage::open(test_config(&db_path)).expect("storage must open");
    let handle = storage.handle();
    {
        let conn = Connection::open(&db_path).expect("side connection must open");
        conn.busy_timeout(std::time::Duration::from_millis(5_000))
            .expect("busy timeout must apply");
        conn.execute_batch(
            "CREATE TABLE filler (x BLOB);
             INSERT INTO filler VALUES (zeroblob(32768)),
              (zeroblob(32768)), (zeroblob(32768)), (zeroblob(32768)),
              (zeroblob(32768)), (zeroblob(32768)), (zeroblob(32768)), (zeroblob(32768));
             DROP TABLE filler;",
        )
        .expect("freelist fixture must run");
    }
    let initial = freelist_pages(&db_path);
    assert!(initial > 8, "freelist fixture too small: {initial}");

    // When: maintenance tick を繰り返す
    let mut previous = initial;
    let mut converged = false;
    for _ in 0..200 {
        handle.checkpoint_now().expect("maintenance tick must run");
        let current = freelist_pages(&db_path);
        let reclaimed = previous - current;
        assert!(
            current <= previous,
            "freelist must never grow: {previous} -> {current}"
        );
        assert!(
            reclaimed <= 2,
            "tick reclaim must respect budget: {reclaimed}"
        );
        previous = current;
        if current < 3 {
            converged = true;
            break;
        }
    }

    // Then: threshold まで段階的に回収が完了する
    assert!(converged, "freelist must converge to threshold: {previous}");

    // When: threshold 未満でさらに 2 tick 実行する
    handle.checkpoint_now().expect("maintenance tick must run");
    let after_extra = freelist_pages(&db_path);
    handle.checkpoint_now().expect("maintenance tick must run");
    let after_second_extra = freelist_pages(&db_path);

    // Then: threshold 未満では回収は一切起きない
    assert_eq!(after_extra, previous);
    assert_eq!(after_second_extra, previous);
    drop(storage);
}

#[derive(Clone, Default)]
struct LogBuffer(std::sync::Arc<std::sync::Mutex<String>>);

impl std::io::Write for LogBuffer {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .expect("log buffer lock")
            .push_str(&String::from_utf8_lossy(buf));
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogBuffer {
    type Writer = LogBuffer;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

fn capture_startup_logs(db_path: &Path, journal_bytes: u64, temp_warn_bytes: u64) -> String {
    // 実在する WAL DB への stale journal 残留を再現するため事前に DB を初期化する。
    // fresh DB に先行する orphan journal は SQLite が起動時に自ら除去する
    // （実測で確認）ため、警告対象たる temp 副産物は既存 DB 上で成立させる。
    {
        let conn = Connection::open(db_path).expect("seed connection must open");
        conn.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE t (x);")
            .expect("seed database must initialize");
    }
    if journal_bytes > 0 {
        std::fs::File::create(journal_path(db_path))
            .expect("journal must be created")
            .set_len(journal_bytes)
            .expect("journal size must be set");
    }
    let buffer = LogBuffer::default();
    let writer = buffer.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(writer)
        .with_ansi(false)
        .without_time()
        .finish();
    tracing::subscriber::with_default(subscriber, || {
        let config = StorageConfig {
            db_path: db_path.to_path_buf(),
            checkpoint_interval: std::time::Duration::from_secs(3_600),
            temp_warn_bytes,
            ..Default::default()
        };
        let storage = Storage::open(config).expect("storage must open");
        drop(storage);
    });
    buffer.0.lock().expect("log buffer lock").clone()
}

fn journal_path(db_path: &Path) -> std::path::PathBuf {
    let mut value = std::ffi::OsString::from(db_path.as_os_str());
    value.push("-journal");
    std::path::PathBuf::from(value)
}

#[test]
fn startup_temp_artifact_over_threshold_warns_once() {
    // Given: threshold 超過の journal 副産物を持つ DB パス
    let temp_dir = tempfile::tempdir().expect("temporary directory must be created");
    let db_path = temp_dir.path().join("t.db");

    // When: storage を起動する
    let logs = capture_startup_logs(&db_path, 64, 10);

    // Then: 起動時検査で 1 回だけ警告される
    assert_eq!(
        logs.matches("temp storage threshold exceeded").count(),
        1,
        "startup must warn exactly once: {logs}"
    );
}

#[test]
fn startup_temp_artifact_under_threshold_is_silent() {
    // Given: threshold 未満の journal 副産物
    let temp_dir = tempfile::tempdir().expect("temporary directory must be created");
    let db_path = temp_dir.path().join("t.db");

    // When: storage を起動する
    let logs = capture_startup_logs(&db_path, 5, 10_000);

    // Then: 警告は出ない
    assert!(
        !logs.contains("temp storage threshold exceeded"),
        "no warning under threshold: {logs}"
    );
}

#[test]
fn startup_temp_artifact_absent_is_silent() {
    // Given: 副産物のない DB パス
    let temp_dir = tempfile::tempdir().expect("temporary directory must be created");
    let db_path = temp_dir.path().join("t.db");

    // When: storage を起動する
    let logs = capture_startup_logs(&db_path, 0, 10);

    // Then: 警告は出ない
    assert!(
        !logs.contains("temp storage threshold exceeded"),
        "no warning without temp artifacts: {logs}"
    );
}
