//! Durable at-most-once learning admission, separate from conversation snapshots.
use crate::StorageError;
use rusqlite::{Connection, params};

pub(crate) fn claim(
    conn: &Connection,
    task: &str,
    roots: &[String],
    goal: bool,
    daily_limit: u32,
) -> Result<bool, StorageError> {
    let now = crate::system_time_to_ns(std::time::SystemTime::now())?;
    // UTC days, shared across reopen and concurrent runtime instances.
    let day = now.div_euclid(86_400_000_000_000);
    claim_on_day(conn, task, roots, goal, daily_limit, day)
}

fn claim_on_day(
    conn: &Connection,
    task: &str,
    roots: &[String],
    goal: bool,
    daily_limit: u32,
    day: i64,
) -> Result<bool, StorageError> {
    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
    let exists: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM learning_claims WHERE task_id=?1)",
        [task],
        |row| row.get(0),
    )?;
    if exists {
        return Ok(false);
    }
    for root in roots {
        let duplicate: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM learning_claim_roots r JOIN learning_claims c USING(task_id) WHERE r.run_id=?1 AND (?2=0 OR c.is_goal=0))",
            params![root, goal], |row| row.get(0))?;
        if duplicate {
            return Ok(false);
        }
    }
    let count: u32 = tx.query_row(
        "SELECT count(*) FROM learning_claims WHERE utc_day=?1",
        [day],
        |row| row.get(0),
    )?;
    if count >= daily_limit {
        return Ok(false);
    }
    tx.execute(
        "INSERT INTO learning_claims(task_id,is_goal,utc_day) VALUES(?1,?2,?3)",
        params![task, goal, day],
    )?;
    for root in roots {
        tx.execute(
            "INSERT OR IGNORE INTO learning_claim_roots(task_id,run_id) VALUES(?1,?2)",
            params![task, root],
        )?;
    }
    tx.commit()?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use crate::{Storage, StorageConfig};
    use std::sync::{Arc, Barrier};

    #[test]
    fn claims_survive_reopen_and_deduplicate_goal_epochs_and_legacy_sources() {
        let dir = tempfile::tempdir().unwrap();
        let config = StorageConfig {
            db_path: dir.path().join("claims.db"),
            ..Default::default()
        };
        {
            let storage = Storage::open(config.clone()).unwrap();
            let writer = storage.handle();
            assert!(
                writer
                    .claim_learning("goal:a:1", &["run-1".into(), "run-2".into()], true, 10)
                    .unwrap()
            );
            assert!(
                !writer
                    .claim_learning("goal:a:1", &["run-2".into()], true, 10)
                    .unwrap()
            );
            assert!(
                !writer
                    .claim_learning("legacy-2", &["run-2".into()], false, 10)
                    .unwrap()
            );
            // A later goal may legitimately share the same interactive root.
            assert!(
                writer
                    .claim_learning("goal:b:2", &["run-2".into()], true, 10)
                    .unwrap()
            );
            assert!(
                writer
                    .claim_learning("legacy-3", &["run-3".into()], false, 10)
                    .unwrap()
            );
            assert!(
                !writer
                    .claim_learning("goal:c:1", &["run-3".into()], true, 10)
                    .unwrap()
            );
        }
        let storage = Storage::open(config).unwrap();
        assert!(
            !storage
                .handle()
                .claim_learning("goal:a:1", &["run-1".into()], true, 10)
                .unwrap()
        );
        assert!(
            !storage
                .handle()
                .claim_learning("legacy-again", &["run-3".into()], false, 10)
                .unwrap()
        );
    }

    #[test]
    fn daily_quota_resets_only_on_a_new_day_and_zero_disables_admission() {
        let db = crate::Database::open_in_memory().unwrap();
        let claim = |id: &str, limit, day| {
            super::claim_on_day(&db.conn, id, &[id.into()], true, limit, day).unwrap()
        };
        assert!(!claim("zero", 0, 100));
        assert!(claim("first", 1, 100));
        assert!(!claim("second", 1, 100));
        assert!(claim("second", 1, 101));
        assert!(
            !claim("first", 1, 102),
            "deduplication survives quota reset"
        );
    }

    #[test]
    fn concurrent_writers_admit_the_same_task_only_once() {
        let dir = tempfile::tempdir().unwrap();
        let config = StorageConfig {
            db_path: dir.path().join("concurrent-key.db"),
            ..Default::default()
        };
        let stores: Vec<_> = (0..4)
            .map(|_| Storage::open(config.clone()).unwrap())
            .collect();
        let barrier = Arc::new(Barrier::new(stores.len()));
        let admitted: usize = std::thread::scope(|scope| {
            let joins: Vec<_> = stores
                .iter()
                .map(|store| {
                    let writer = store.handle();
                    let barrier = Arc::clone(&barrier);
                    scope.spawn(move || {
                        barrier.wait();
                        usize::from(
                            writer
                                .claim_learning(
                                    "goal:concurrent:1",
                                    &["run-1".into()],
                                    true,
                                    u32::MAX,
                                )
                                .unwrap(),
                        )
                    })
                })
                .collect();
            joins.into_iter().map(|join| join.join().unwrap()).sum()
        });
        assert_eq!(admitted, 1);
    }

    #[test]
    fn concurrent_connections_cannot_overspend_the_last_daily_slot() {
        let dir = tempfile::tempdir().unwrap();
        let config = StorageConfig {
            db_path: dir.path().join("concurrent-quota.db"),
            ..Default::default()
        };
        let connections: Vec<_> = (0..4)
            .map(|_| crate::Database::open(&config).unwrap())
            .collect();
        // Inject one logical day into the same admission transaction used by the
        // writers, so crossing UTC midnight cannot grant an extra test slot.
        assert!(
            super::claim_on_day(
                &connections[0].conn,
                "already-admitted",
                &["run-0".into()],
                true,
                2,
                100,
            )
            .unwrap()
        );
        let barrier = Arc::new(Barrier::new(connections.len()));
        let admitted: usize = std::thread::scope(|scope| {
            let joins: Vec<_> = connections
                .into_iter()
                .enumerate()
                .map(|(index, db)| {
                    let barrier = Arc::clone(&barrier);
                    scope.spawn(move || {
                        barrier.wait();
                        let task = format!("goal:{index}:1");
                        usize::from(
                            super::claim_on_day(
                                &db.conn,
                                &task,
                                &[format!("run-{}", index + 1)],
                                true,
                                2,
                                100,
                            )
                            .unwrap(),
                        )
                    })
                })
                .collect();
            joins.into_iter().map(|join| join.join().unwrap()).sum()
        });
        assert_eq!(admitted, 1);
        let db = crate::Database::open(&config).unwrap();
        let claimed: u32 = db
            .conn
            .query_row(
                "SELECT count(*) FROM learning_claims WHERE utc_day=100",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(claimed, 2);
    }
}
