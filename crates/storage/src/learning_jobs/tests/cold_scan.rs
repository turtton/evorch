use super::*;

#[test]
fn cold_scan_filters_states_before_decoding_and_isolates_each_bad_row() {
    for corruption in [
        "origin_pid=-1",
        "origin_pid=4294967296",
        "origin_start_ticks=-1",
        "attempt=-1",
        "origin_pid=x'00'",
        "origin_instance=CAST(x'ff' AS TEXT)",
        "origin_kind='unknown'",
    ] {
        let db = Database::open_in_memory().unwrap();
        let owner = origin(LearningOriginKind::Core, "old");
        for (at, id) in [(0, "z-good"), (1, "bad"), (2, "a-good"), (3, "outside")] {
            write(
                &db.conn,
                Write::Admit(Box::new(job(id, id, false, &owner)), at),
            )
            .unwrap();
        }
        // Damage must bypass immutable-origin/CAS fences, as an external writer can.
        db.conn
            .execute_batch(
                "DROP TRIGGER learning_job_forward_only; PRAGMA ignore_check_constraints=ON;",
            )
            .unwrap();
        db.conn
            .execute(
                &format!("UPDATE learning_jobs SET {corruption} WHERE task_id='bad'"),
                [],
            )
            .unwrap();
        db.conn
            .execute(
                "UPDATE learning_jobs SET state='Finished', origin_pid=-1 WHERE task_id='outside'",
                [],
            )
            .unwrap();
        db.conn.execute("UPDATE learning_jobs SET origin_pid=4294967295, origin_start_ticks=9223372036854775807 WHERE task_id='z-good'", []).unwrap();
        let error = db.learning_jobs(&[LearningJobState::Pending]).unwrap_err();
        assert!(error.is_data_decode_error(), "{corruption}: {error}");
        let jobs = db
            .learning_jobs_isolated(&[
                LearningJobState::Pending,
                LearningJobState::Interrupted,
                LearningJobState::Pending,
            ])
            .unwrap();
        assert_eq!(
            jobs.iter().map(|j| j.task_id.as_str()).collect::<Vec<_>>(),
            ["z-good", "a-good"],
            "{corruption}"
        );
        assert_eq!(jobs[0].origin.pid, u32::MAX);
        assert_eq!(jobs[0].origin.start_ticks, Some(i64::MAX as u64));
        assert!(db.learning_jobs(&[]).unwrap().is_empty());
        // Outside is still corrupt: the state predicate, not a permissive decoder,
        // prevents it from poisoning the Pending read.
        assert!(db.learning_jobs(&[LearningJobState::Finished]).is_err());
        db.conn
            .execute(
                "UPDATE learning_jobs SET state='Finished' WHERE task_id='bad'",
                [],
            )
            .unwrap();
        assert_eq!(
            db.learning_jobs(&[LearningJobState::Pending]).unwrap(),
            jobs
        );
    }
}

#[test]
fn cold_scan_propagates_database_failures_instead_of_reporting_corrupt_rows() {
    let db = Database::open_in_memory().unwrap();
    db.conn
        .execute_batch("ALTER TABLE learning_jobs RENAME TO unavailable_jobs")
        .unwrap();
    let error = db
        .learning_jobs_isolated(&[LearningJobState::Interrupted])
        .unwrap_err();
    assert!(matches!(
        error,
        StorageError::Sqlite(rusqlite::Error::SqliteFailure(..))
    ));
    assert!(!error.is_data_decode_error());
    for code in [
        rusqlite::ffi::SQLITE_IOERR,
        rusqlite::ffi::SQLITE_BUSY,
        rusqlite::ffi::SQLITE_CORRUPT,
        rusqlite::ffi::SQLITE_CANTOPEN,
    ] {
        let error = StorageError::Sqlite(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(code),
            None,
        ));
        assert!(!error.is_data_decode_error(), "{error}");
    }
    assert!(!StorageError::WriterClosed.is_data_decode_error());
    assert!(!StorageError::Io("unavailable".into()).is_data_decode_error());
    assert!(!StorageError::Sqlite(rusqlite::Error::InvalidColumnIndex(99)).is_data_decode_error());
}
