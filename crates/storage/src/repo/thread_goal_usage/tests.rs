use super::*;
use crate::{Database, Storage, StorageConfig};

fn usage(r: u32, i: u64, o: u64) -> ThreadGoalUsage {
    ThreadGoalUsage {
        model_requests: r,
        input_tokens: i,
        output_tokens: o,
    }
}

#[test]
fn absolute_max_is_order_independent_and_preserves_unsigned_ranges() {
    let db = Database::open_in_memory().unwrap();
    assert_eq!(get(&db.conn, "g").unwrap(), None);
    for value in [
        usage(0, 0, 0),
        usage(u32::MAX, i64::MAX as u64, 0),
        usage(1, i64::MAX as u64 + 1, u64::MAX),
        usage(0, 0, 0),
    ] {
        upsert(&db.conn, "g", "thread", "root", &value).unwrap();
    }
    assert_eq!(
        get(&db.conn, "g").unwrap(),
        Some(usage(u32::MAX, i64::MAX as u64 + 1, u64::MAX))
    );
    assert_eq!(
        db.conn
            .query_row("SELECT COUNT(*) FROM thread_goal_usage_current", [], |r| {
                r.get::<_, i64>(0)
            })
            .unwrap(),
        1
    );
    assert!(db.events_all_ordered().unwrap().is_empty());
    upsert(&db.conn, "g", "other", "new-root", &usage(7, u64::MAX, 10)).unwrap();
    assert_eq!(
        get(&db.conn, "g").unwrap(),
        Some(usage(u32::MAX, u64::MAX, u64::MAX))
    );
}

#[test]
fn constraints_and_decoder_reject_corruption_without_repair() {
    let db = Database::open_in_memory().unwrap();
    for column in ["model_requests", "input_tokens", "output_tokens"] {
        for bad in ["0", "1.5", "'00000000'", "x'00'", "x'000000000000000000'"] {
            upsert(&db.conn, "g", "t", "r", &usage(1, 2, 3)).unwrap();
            let sql = format!("UPDATE thread_goal_usage_current SET {column} = {bad}");
            assert!(db.conn.execute(&sql, []).is_err());
            db.conn
                .pragma_update(None, "ignore_check_constraints", true)
                .unwrap();
            db.conn.execute(&sql, []).unwrap();
            assert!(get(&db.conn, "g").is_err());
            assert!(upsert(&db.conn, "g", "t", "r", &usage(0, 0, 0)).is_err());
            db.conn
                .execute("DELETE FROM thread_goal_usage_current", [])
                .unwrap();
            db.conn
                .pragma_update(None, "ignore_check_constraints", false)
                .unwrap();
        }
    }
    upsert(&db.conn, "g", "t", "r", &usage(0, 0, 0)).unwrap();
    let sql = "UPDATE thread_goal_usage_current SET model_requests=x'0000000100000000'";
    assert!(db.conn.execute(sql, []).is_err());
    db.conn
        .pragma_update(None, "ignore_check_constraints", true)
        .unwrap();
    db.conn.execute(sql, []).unwrap();
    assert!(get(&db.conn, "g").is_err());
}

#[test]
fn writer_ack_is_commit_and_has_no_event_notification() {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("db"),
        ..Default::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    let handle = storage.handle();
    let wakes = Arc::new(AtomicUsize::new(0));
    let observer: Arc<dyn Fn() + Send + Sync> = {
        let wakes = wakes.clone();
        Arc::new(move || {
            wakes.fetch_add(1, Ordering::SeqCst);
        })
    };
    handle.observe_event_commits(&observer);
    handle
        .upsert_thread_goal_usage("g", "t", "r", &usage(1, 2, 3))
        .unwrap();
    let db = Database::open_read_only(&config.db_path).unwrap();
    assert_eq!(db.thread_goal_usage("g").unwrap(), Some(usage(1, 2, 3)));
    assert_eq!(wakes.load(Ordering::SeqCst), 0);
    assert!(db.events_all_ordered().unwrap().is_empty());
}

#[test]
fn tombstones_are_checked_inside_transaction() {
    let db = Database::open_in_memory().unwrap();
    db.conn
        .execute(
            "INSERT INTO thread_tombstones(thread_id,family_key,deleted_at_ns) VALUES('t','f',0)",
            [],
        )
        .unwrap();
    db.conn
        .execute(
            "INSERT INTO run_tombstones(run_id,thread_id,family_key) VALUES('r','other','f')",
            [],
        )
        .unwrap();
    for (thread, root) in [("t", "live"), ("live", "r")] {
        assert!(matches!(
            upsert(&db.conn, "g", thread, root, &usage(1, 2, 3)),
            Err(StorageError::Tombstoned { .. })
        ));
    }
    assert_eq!(get(&db.conn, "g").unwrap(), None);
}

#[test]
fn failed_write_rolls_back_and_suspended_writer_refuses() {
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("db"),
        ..Default::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    let handle = storage.handle();
    handle.lane_job(true, |conn| {
        conn.execute_batch("CREATE TRIGGER fail_usage BEFORE INSERT ON thread_goal_usage_current BEGIN SELECT RAISE(ABORT,'injected failure'); END;")?;
        Ok(())
    }).unwrap();
    assert!(
        handle
            .upsert_thread_goal_usage("g", "t", "r", &usage(1, 2, 3))
            .is_err()
    );
    assert_eq!(
        Database::open_read_only(&config.db_path)
            .unwrap()
            .thread_goal_usage("g")
            .unwrap(),
        None
    );
    drop(handle);
    drop(storage);
    let mut config = config;
    config.hard_limits.max_db_bytes = 0;
    let storage = Storage::open(config).unwrap();
    assert!(
        storage
            .handle()
            .upsert_thread_goal_usage("g", "t", "r", &usage(1, 2, 3))
            .is_err()
    );
}
