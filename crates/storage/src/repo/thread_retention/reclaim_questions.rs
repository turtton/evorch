//! A recipient owns its links, not another recipient's question body.
use super::*;

pub(super) fn take(tx: &Transaction<'_>, run: &str) -> Result<Work, StorageError> {
    // Link + possibly orphaned body: at most RECLAIM_ROWS explicit deletions.
    let limit = i64::try_from((RECLAIM_ROWS / 2).max(1)).unwrap_or(i64::MAX);
    let mut statement = tx.prepare_cached(
        "SELECT rowid, question_id FROM user_question_links WHERE run_id=?1 LIMIT ?2",
    )?;
    let candidates = statement
        .query_map(params![run, limit], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    if candidates.is_empty() {
        return Ok(Work::default());
    }
    let rowids = candidates.iter().map(|(id, _)| *id).collect::<Vec<_>>();
    let mut rows = tx.execute(
        "DELETE FROM user_question_links WHERE rowid IN (SELECT value FROM json_each(?1))",
        [json_ids(&rowids)?],
    )?;
    let mut collect = tx.prepare_cached(
        "DELETE FROM user_questions WHERE id=?1
         AND (run_id IN (SELECT run_id FROM run_tombstones)
              OR root_run_id IN (SELECT run_id FROM run_tombstones))
         AND NOT EXISTS (SELECT 1 FROM user_question_links l WHERE l.question_id=user_questions.id)",
    )?;
    // Primary-key point lookup for each removed link. Never sweep unrelated dead
    // owners: the last recipient collects exactly its newly unreferenced bodies.
    for (_, question) in candidates {
        rows += collect.execute([question])?;
    }
    Ok(Work {
        rows: rows as u64,
        ..Work::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    };

    fn cost(unrelated: usize) -> u64 {
        let dir = tempfile::tempdir().unwrap();
        let config = crate::StorageConfig {
            db_path: dir.path().join("gc.db"),
            ..Default::default()
        };
        drop(crate::Database::open_unchecked(&config).unwrap());
        let mut c = Connection::open(&config.db_path).unwrap();
        c.pragma_update(None, "foreign_keys", true).unwrap();
        let tx = c.transaction().unwrap();
        for n in 0..=unrelated {
            let id = format!("question-{n}");
            tx.execute("INSERT INTO user_questions(id,run_id,root_run_id,root_name,payload) VALUES (?1,'old','old','old','{}')", [&id]).unwrap();
            tx.execute(
                "INSERT INTO user_question_links(question_id,run_id) VALUES (?1,?2)",
                params![
                    id,
                    if n == 0 {
                        "recipient"
                    } else {
                        "other-live-recipient"
                    }
                ],
            )
            .unwrap();
        }
        tx.execute("INSERT INTO run_tombstones(run_id,thread_id,family_key) VALUES ('old','old-thread','old-family')", []).unwrap();
        tx.commit().unwrap();
        let steps = Arc::new(AtomicU64::new(0));
        let count = steps.clone();
        c.progress_handler(
            1,
            Some(move || {
                count.fetch_add(1, Ordering::Relaxed);
                false
            }),
        )
        .unwrap();
        let tx = c.transaction().unwrap();
        let mut cursor = Cursor {
            run: Some("recipient".into()),
            ..Default::default()
        };
        for table in ["user_question_links", "user_questions"] {
            cursor.table_idx = RUN_TABLES
                .iter()
                .position(|(name, _)| *name == table)
                .unwrap() as u32;
            super::super::run_tables(&tx, "family", &mut cursor).unwrap();
        }
        tx.commit().unwrap();
        c.progress_handler(0, None::<fn() -> bool>).unwrap();
        assert_eq!(
            c.query_row("SELECT COUNT(*) FROM user_questions", [], |r| r
                .get::<_, usize>(0))
                .unwrap(),
            unrelated
        );
        assert_eq!(
            c.query_row(
                "SELECT COUNT(*) FROM user_question_links WHERE run_id='recipient'",
                [],
                |r| r.get::<_, usize>(0)
            )
            .unwrap(),
            0
        );
        assert_eq!(
            c.query_row(
                "SELECT COUNT(*) FROM user_questions WHERE id='question-0'",
                [],
                |r| r.get::<_, usize>(0)
            )
            .unwrap(),
            0
        );
        assert_eq!(
            c.query_row(
                "SELECT COUNT(*) FROM user_question_links WHERE run_id='other-live-recipient'",
                [],
                |r| r.get::<_, usize>(0)
            )
            .unwrap(),
            unrelated
        );
        steps.load(Ordering::Relaxed)
    }
    #[test]
    fn last_recipient_gc_is_point_lookup_bounded_despite_unrelated_retained_questions() {
        let small = cost(0);
        let large = cost(20_000);
        assert!(
            small > 0,
            "VM-step instrumentation must observe SQL execution"
        );
        assert_eq!(
            large, small,
            "GC VM steps must not scan unrelated questions: {small} vs {large}"
        );
    }
}
