//! Bound, transition and reconcile tests of the audit store on an in-memory database.
//! The real byte bounds cannot be reached with valid field sizes, so those tests shrink
//! [`Limits`]; the row bounds and the writer path are also covered with the real limits
//! in `tests/admin_audit.rs`.

use rusqlite::params;

use super::store::{Limits, RESERVE_BYTES, execute_with};
use super::*;
use crate::Database;

const DAY_MS: u64 = 24 * 60 * 60 * 1000;

pub(super) fn nonce(n: u32) -> String {
    format!("{n:08x}-0000-4000-8000-000000000000")
}

fn key(id: &str, n: u32) -> AuditOperationKey {
    AuditOperationKey {
        operation_id: id.into(),
        attempt_nonce: nonce(n),
    }
}

fn principal(subject: &str) -> AuditPrincipal {
    AuditPrincipal {
        method: "unix-peer".into(),
        subject: subject.into(),
        binding: "conn-1".into(),
    }
}

fn accepted(id: &str, n: u32, operation: AuditOperation, epoch: u64, at_ms: u64) -> AuditAccepted {
    AuditAccepted {
        key: key(id, n),
        operation,
        surface: AuditSurface::Cli,
        principal: principal("uid-1000"),
        accepted_core_epoch: epoch,
        intent_digest: "ab".repeat(32),
        accepted_at_ms: at_ms,
    }
}

fn plain(id: &str, at_ms: u64) -> AuditAccepted {
    accepted(id, 1, AuditOperation::Cancel, 7, at_ms)
}

fn db() -> Database {
    Database::open_in_memory().unwrap()
}

fn run(db: &Database, write: AuditWrite) -> Result<AuditReply, StorageError> {
    crate::writer::admin_audit::execute(db, write)
}

fn run_with(db: &Database, write: AuditWrite, limits: &Limits) -> Result<AuditReply, StorageError> {
    execute_with(&db.conn, write, limits)
}

fn audit_error(result: Result<AuditReply, StorageError>) -> AuditError {
    result.unwrap_err().admin_audit().expect("an audit error")
}

fn finish(db: &Database, id: &str, at_ms: u64) {
    run(
        db,
        AuditWrite::Finalize {
            key: key(id, 1),
            outcome: AuditOutcome::Succeeded,
            at_ms,
        },
    )
    .unwrap();
}

#[test]
fn the_default_limits_are_the_documented_bounds() {
    let limits = Limits::default();
    assert_eq!(limits.accepted_rows, 10_000);
    assert_eq!(limits.accepted_bytes, 32 * 1024 * 1024);
    assert_eq!(limits.record_bytes, 4096);
    assert_eq!(limits.completed_retention_ms, 30 * DAY_MS);
    assert_eq!(limits.denial_rows, 1_000);
    assert_eq!(limits.denial_bytes, 4 * 1024 * 1024);
    assert_eq!(limits.denial_retention_ms, DAY_MS);
    assert_eq!(limits.denial_keys_per_window, 256);
    assert_eq!(limits.batch, 64);
}

#[test]
fn the_budget_of_the_largest_valid_record_stays_under_the_record_limit() {
    let mut record = accepted(
        &"o".repeat(128),
        1,
        AuditOperation::RestartRequest,
        u64::MAX >> 1,
        u64::MAX >> 1,
    );
    record.principal = AuditPrincipal {
        method: "m".repeat(32),
        subject: "s".repeat(256),
        binding: "b".repeat(128),
    };
    let budget = serde_json::to_string(&record).unwrap().len() as u64 + RESERVE_BYTES;
    assert!(budget <= AUDIT_MAX_RECORD_BYTES, "budget {budget}");
    let db = db();
    run(&db, AuditWrite::Accept(record)).unwrap();
}

#[test]
fn accept_start_bind_and_finalize_follow_one_order() {
    let db = db();
    let record = accepted("op-1", 1, AuditOperation::RestartRequest, 7, 1_000);
    let AuditReply::Reservation(AuditReservation::Created(created)) =
        run(&db, AuditWrite::Accept(record.clone())).unwrap()
    else {
        panic!("not created");
    };
    assert_eq!(created.state, AuditState::Prepared);
    assert_eq!(created.accepted, record);

    let AuditReply::Applied(started) = run(
        &db,
        AuditWrite::Start {
            key: key("op-1", 1),
        },
    )
    .unwrap() else {
        panic!("not applied");
    };
    assert_eq!(started.state, AuditState::Started);
    // Started again is the same answer, not an error and not a new right to execute.
    assert_eq!(
        run(
            &db,
            AuditWrite::Start {
                key: key("op-1", 1)
            }
        )
        .unwrap(),
        AuditReply::Applied(started.clone())
    );

    let AuditReply::Applied(bound) = run(
        &db,
        AuditWrite::BindRestart {
            key: key("op-1", 1),
            restart_id: "restart-A".into(),
        },
    )
    .unwrap() else {
        panic!("not applied");
    };
    assert_eq!(bound.restart_id.as_deref(), Some("restart-A"));
    assert_eq!(bound.state, AuditState::Started);

    let AuditReply::Applied(done) = run(
        &db,
        AuditWrite::Finalize {
            key: key("op-1", 1),
            outcome: AuditOutcome::Failed,
            at_ms: 2_000,
        },
    )
    .unwrap() else {
        panic!("not applied");
    };
    assert_eq!(done.state, AuditState::Finished);
    assert_eq!(done.outcome, Some(AuditOutcome::Failed));
    assert_eq!(done.finished_at_ms, Some(2_000));
    assert_eq!(db.admin_audit_record("op-1").unwrap(), Some(done));
}

#[test]
fn a_finished_operation_answers_the_same_finalize_and_refuses_everything_else() {
    let db = db();
    run(&db, AuditWrite::Accept(plain("op-1", 10))).unwrap();
    let finalize = |outcome, at_ms| AuditWrite::Finalize {
        key: key("op-1", 1),
        outcome,
        at_ms,
    };
    let first = run(&db, finalize(AuditOutcome::Cancelled, 20)).unwrap();
    // A resend after an unknown acknowledgement keeps the first finish time.
    assert_eq!(
        run(&db, finalize(AuditOutcome::Cancelled, 99)).unwrap(),
        first
    );
    assert_eq!(
        audit_error(run(&db, finalize(AuditOutcome::Failed, 30))),
        AuditError::Conflict(AuditConflict::OutcomeMismatch)
    );
    assert_eq!(
        audit_error(run(
            &db,
            AuditWrite::Start {
                key: key("op-1", 1)
            }
        )),
        AuditError::Conflict(AuditConflict::InvalidTransition)
    );
}

#[test]
fn an_operation_can_finish_before_it_started() {
    let db = db();
    run(&db, AuditWrite::Accept(plain("op-1", 10))).unwrap();
    let AuditReply::Applied(done) = run(
        &db,
        AuditWrite::Finalize {
            key: key("op-1", 1),
            outcome: AuditOutcome::AbortedBeforeEffect,
            at_ms: 11,
        },
    )
    .unwrap() else {
        panic!("not applied");
    };
    assert_eq!(done.outcome, Some(AuditOutcome::AbortedBeforeEffect));
}

#[test]
fn the_same_facts_with_another_nonce_exist_without_a_right_to_execute() {
    let db = db();
    run(
        &db,
        AuditWrite::Accept(accepted("op-1", 1, AuditOperation::Cancel, 7, 10)),
    )
    .unwrap();
    // Same operation, surface, principal, epoch and digest; another attempt and time.
    let again = accepted("op-1", 2, AuditOperation::Cancel, 7, 99);
    let AuditReply::Reservation(AuditReservation::Existing(existing)) =
        run(&db, AuditWrite::Accept(again)).unwrap()
    else {
        panic!("not existing");
    };
    assert_eq!(existing.accepted.key.attempt_nonce, nonce(1));
    assert_eq!(existing.accepted.accepted_at_ms, 10);
    for write in [
        AuditWrite::Start {
            key: key("op-1", 2),
        },
        AuditWrite::BindRestart {
            key: key("op-1", 2),
            restart_id: "r".into(),
        },
        AuditWrite::Finalize {
            key: key("op-1", 2),
            outcome: AuditOutcome::Succeeded,
            at_ms: 1,
        },
    ] {
        assert_eq!(
            audit_error(run(&db, write)),
            AuditError::Conflict(AuditConflict::NonceMismatch)
        );
    }
    assert_eq!(
        db.admin_audit_record("op-1").unwrap().unwrap().state,
        AuditState::Prepared
    );
}

#[test]
fn every_semantic_field_alone_makes_a_reused_id_a_conflict() {
    let db = db();
    run(&db, AuditWrite::Accept(plain("op-1", 10))).unwrap();
    let base = plain("op-1", 10);
    let mut variants = Vec::new();
    let mut other = base.clone();
    other.operation = AuditOperation::Discard;
    variants.push(other);
    let mut other = base.clone();
    other.surface = AuditSurface::Web;
    variants.push(other);
    let mut other = base.clone();
    other.principal.subject = "uid-2".into();
    variants.push(other);
    let mut other = base.clone();
    other.accepted_core_epoch = 8;
    variants.push(other);
    let mut other = base;
    other.intent_digest = "cd".repeat(32);
    variants.push(other);
    for variant in variants {
        assert_eq!(
            audit_error(run(&db, AuditWrite::Accept(variant))),
            AuditError::Conflict(AuditConflict::SemanticMismatch)
        );
    }
}

#[test]
fn bind_restart_is_one_to_one_per_operation_and_many_to_one_per_restart() {
    let db = db();
    for id in ["op-1", "op-2"] {
        run(
            &db,
            AuditWrite::Accept(accepted(id, 1, AuditOperation::RestartRequest, 7, 1)),
        )
        .unwrap();
    }
    let bind = |id: &str, restart: &str| AuditWrite::BindRestart {
        key: key(id, 1),
        restart_id: restart.into(),
    };
    let first = run(&db, bind("op-1", "restart-A")).unwrap();
    assert_eq!(run(&db, bind("op-1", "restart-A")).unwrap(), first);
    assert_eq!(
        audit_error(run(&db, bind("op-1", "restart-B"))),
        AuditError::Conflict(AuditConflict::RestartIdMismatch)
    );
    run(&db, bind("op-2", "restart-A")).unwrap();
    let bound: i64 = db
        .conn
        .query_row(
            "SELECT COUNT(*) FROM accepted_admin_operations WHERE restart_id = 'restart-A'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(bound, 2);

    run(&db, AuditWrite::Accept(plain("op-3", 1))).unwrap();
    assert_eq!(
        audit_error(run(&db, bind("op-3", "restart-A"))),
        AuditError::Conflict(AuditConflict::NotRestartOperation)
    );
    // After the end of the operation no restart can be attached any more.
    run(
        &db,
        AuditWrite::Accept(accepted("op-4", 1, AuditOperation::RestartRequest, 7, 1)),
    )
    .unwrap();
    finish(&db, "op-4", 5);
    assert_eq!(
        audit_error(run(&db, bind("op-4", "restart-A"))),
        AuditError::Conflict(AuditConflict::InvalidTransition)
    );
}

#[test]
fn commands_on_a_missing_operation_conflict() {
    let db = db();
    for write in [
        AuditWrite::Start {
            key: key("nope", 1),
        },
        AuditWrite::BindRestart {
            key: key("nope", 1),
            restart_id: "r".into(),
        },
        AuditWrite::Finalize {
            key: key("nope", 1),
            outcome: AuditOutcome::Failed,
            at_ms: 1,
        },
    ] {
        assert_eq!(
            audit_error(run(&db, write)),
            AuditError::Conflict(AuditConflict::MissingOperation)
        );
    }
    assert_eq!(
        run(
            &db,
            AuditWrite::Read {
                operation_id: "nope".into()
            }
        )
        .unwrap(),
        AuditReply::Record(None)
    );
}

#[test]
fn malformed_input_is_invalid_and_changes_nothing() {
    let db = db();
    let mut bad = Vec::new();
    for (id, nonce_text) in [
        ("", nonce(1)),
        ("a\nb", nonce(1)),
        (&"x".repeat(129), nonce(1)),
        ("ok", nonce(0xabc).to_uppercase()),
        ("ok", "not-a-uuid".to_owned()),
        ("ok", "00000001-0000-1000-8000-000000000000".to_owned()),
        ("ok", "00000001-0000-4000-c000-000000000000".to_owned()),
    ] {
        let mut record = plain("ok", 1);
        record.key = AuditOperationKey {
            operation_id: id.into(),
            attempt_nonce: nonce_text,
        };
        bad.push(record);
    }
    let mut record = plain("ok", 1);
    record.intent_digest = "AB".repeat(32);
    bad.push(record);
    let mut record = plain("ok", 1);
    record.intent_digest = "ab".repeat(31);
    bad.push(record);
    for (method, subject, binding) in [
        ("", "s", "b"),
        (&"m".repeat(33), "s", "b"),
        ("m", &"s".repeat(257), "b"),
        ("m", "s", &"b".repeat(129)),
        ("m", "s\u{0}", "b"),
        ("m", "s", ""),
    ] {
        let mut record = plain("ok", 1);
        record.principal = AuditPrincipal {
            method: method.into(),
            subject: subject.into(),
            binding: binding.into(),
        };
        bad.push(record);
    }
    let mut record = plain("ok", 1);
    record.accepted_core_epoch = u64::MAX;
    bad.push(record);
    let mut record = plain("ok", 1);
    record.accepted_at_ms = u64::MAX;
    bad.push(record);
    for record in bad {
        assert!(
            matches!(
                audit_error(run(&db, AuditWrite::Accept(record.clone()))),
                AuditError::Invalid(_)
            ),
            "{record:?}"
        );
    }
    assert_eq!(db.admin_audit_stats().unwrap().accepted_count, 0);
}

#[test]
fn a_record_may_use_exactly_the_record_limit_and_no_more() {
    let record = plain("op-1", 1);
    let budget = serde_json::to_string(&record).unwrap().len() as u64 + RESERVE_BYTES;
    let exact = Limits {
        record_bytes: budget,
        ..Limits::default()
    };
    let db1 = db();
    run_with(&db1, AuditWrite::Accept(record.clone()), &exact).unwrap();
    assert_eq!(db1.admin_audit_stats().unwrap().accepted_bytes, budget);

    let below = Limits {
        record_bytes: budget - 1,
        ..Limits::default()
    };
    let db2 = db();
    assert!(matches!(
        audit_error(run_with(&db2, AuditWrite::Accept(record), &below)),
        AuditError::Invalid(_)
    ));
    assert_eq!(db2.admin_audit_stats().unwrap().accepted_count, 0);
}

#[test]
fn the_row_bound_keeps_unfinished_rows_and_evicts_the_oldest_finished_one() {
    let db = db();
    let limits = Limits {
        accepted_rows: 3,
        ..Limits::default()
    };
    for (index, id) in ["a", "b", "c"].into_iter().enumerate() {
        run_with(&db, AuditWrite::Accept(plain(id, index as u64)), &limits).unwrap();
    }
    // Full of unfinished rows: refused, nothing evicted.
    assert_eq!(
        audit_error(run_with(&db, AuditWrite::Accept(plain("d", 9)), &limits)),
        AuditError::Capacity(AuditCapacity::PendingOperations)
    );
    assert_eq!(db.admin_audit_stats().unwrap().accepted_count, 3);
    // b finishes before a: b is the older finished row and goes first.
    finish(&db, "b", 100);
    finish(&db, "a", 200);
    run_with(&db, AuditWrite::Accept(plain("d", 10)), &limits).unwrap();
    assert!(db.admin_audit_record("b").unwrap().is_none());
    assert!(db.admin_audit_record("a").unwrap().is_some());
    run_with(&db, AuditWrite::Accept(plain("e", 11)), &limits).unwrap();
    assert!(db.admin_audit_record("a").unwrap().is_none());
    // c, d, e are all unfinished now.
    assert_eq!(
        audit_error(run_with(&db, AuditWrite::Accept(plain("f", 12)), &limits)),
        AuditError::Capacity(AuditCapacity::PendingOperations)
    );
    let stats = db.admin_audit_stats().unwrap();
    assert_eq!((stats.accepted_count, stats.pending_count), (3, 3));
}

#[test]
fn the_byte_bound_counts_the_reserve_and_frees_only_what_is_needed() {
    let db = db();
    let budget = serde_json::to_string(&plain("a", 1)).unwrap().len() as u64 + RESERVE_BYTES;
    let limits = Limits {
        accepted_bytes: budget * 2,
        ..Limits::default()
    };
    run_with(&db, AuditWrite::Accept(plain("a", 1)), &limits).unwrap();
    run_with(&db, AuditWrite::Accept(plain("b", 2)), &limits).unwrap();
    assert_eq!(db.admin_audit_stats().unwrap().accepted_bytes, budget * 2);
    assert_eq!(
        audit_error(run_with(&db, AuditWrite::Accept(plain("c", 3)), &limits)),
        AuditError::Capacity(AuditCapacity::PendingOperations)
    );
    finish(&db, "a", 10);
    run_with(&db, AuditWrite::Accept(plain("c", 3)), &limits).unwrap();
    assert!(db.admin_audit_record("a").unwrap().is_none());
    assert!(db.admin_audit_record("b").unwrap().is_some());
    assert_eq!(db.admin_audit_stats().unwrap().accepted_bytes, budget * 2);
}

#[test]
fn the_ten_thousand_row_bound_holds_with_the_real_limits() {
    let db = db();
    let tx = db.conn.unchecked_transaction().unwrap();
    // Bulk-load pending rows below the bound through the same statement shape.
    for index in 0..9_999u64 {
        let record = plain(&format!("op-{index}"), index);
        let json = serde_json::to_string(&record).unwrap();
        tx.execute(
            "INSERT INTO accepted_admin_operations(operation_id, attempt_nonce, \
             accepted_core_epoch, operation, accepted_at_ms, accepted_json, state, budget_bytes) \
             VALUES (?1, ?2, 7, 'cancel', ?3, ?4, 'prepared', ?5)",
            params![
                record.key.operation_id,
                record.key.attempt_nonce,
                index,
                json,
                json.len() as u64 + RESERVE_BYTES
            ],
        )
        .unwrap();
    }
    tx.commit().unwrap();
    run(&db, AuditWrite::Accept(plain("op-9999", 9_999))).unwrap();
    assert_eq!(db.admin_audit_stats().unwrap().accepted_count, 10_000);
    assert_eq!(
        audit_error(run(&db, AuditWrite::Accept(plain("op-10000", 10_000)))),
        AuditError::Capacity(AuditCapacity::PendingOperations)
    );
    finish(&db, "op-5", 1);
    run(&db, AuditWrite::Accept(plain("op-10000", 10_000))).unwrap();
    assert_eq!(db.admin_audit_stats().unwrap().accepted_count, 10_000);
    assert!(db.admin_audit_record("op-5").unwrap().is_none());
}

#[test]
fn cleanup_removes_finished_rows_exactly_thirty_days_after_they_finished() {
    let db = db();
    run(&db, AuditWrite::Accept(plain("a", 1))).unwrap();
    run(&db, AuditWrite::Accept(plain("pending", 1))).unwrap();
    finish(&db, "a", 1_000);
    let thirty_days = 30 * DAY_MS;
    let cleanup = |now_ms| match run(&db, AuditWrite::Cleanup { now_ms }).unwrap() {
        AuditReply::Cleanup(done) => done,
        other => panic!("{other:?}"),
    };
    let before = cleanup(1_000 + thirty_days - 1);
    assert_eq!((before.accepted_removed, before.more), (0, false));
    assert!(db.admin_audit_record("a").unwrap().is_some());
    let at = cleanup(1_000 + thirty_days);
    assert_eq!((at.accepted_removed, at.more), (1, false));
    assert!(db.admin_audit_record("a").unwrap().is_none());
    // An unfinished row is kept however old it is.
    let late = cleanup(u64::MAX >> 1);
    assert_eq!(late.accepted_removed, 0);
    assert!(db.admin_audit_record("pending").unwrap().is_some());
}

#[test]
fn cleanup_works_in_batches_of_sixty_four() {
    let db = db();
    for index in 0..65u64 {
        let id = format!("op-{index:03}");
        run(&db, AuditWrite::Accept(plain(&id, index))).unwrap();
        finish(&db, &id, index);
    }
    let now_ms = 30 * DAY_MS + 1_000;
    let AuditReply::Cleanup(first) = run(&db, AuditWrite::Cleanup { now_ms }).unwrap() else {
        panic!("not cleanup");
    };
    assert_eq!((first.accepted_removed, first.more), (64, true));
    let AuditReply::Cleanup(second) = run(&db, AuditWrite::Cleanup { now_ms }).unwrap() else {
        panic!("not cleanup");
    };
    assert_eq!((second.accepted_removed, second.more), (1, false));
    assert_eq!(db.admin_audit_stats().unwrap().accepted_count, 0);
}

#[test]
fn rows_cannot_move_backwards_or_be_rewritten_below_the_api() {
    let db = db();
    run(
        &db,
        AuditWrite::Accept(accepted("op-1", 1, AuditOperation::RestartRequest, 7, 1)),
    )
    .unwrap();
    run(
        &db,
        AuditWrite::Start {
            key: key("op-1", 1),
        },
    )
    .unwrap();
    let sql = |statement: &str| db.conn.execute_batch(statement);
    assert!(sql("UPDATE accepted_admin_operations SET state = 'prepared'").is_err());
    assert!(sql("UPDATE accepted_admin_operations SET accepted_json = '{}'").is_err());
    assert!(sql("UPDATE accepted_admin_operations SET attempt_nonce = 'x'").is_err());
    assert!(sql("UPDATE accepted_admin_operations SET state = 'finished'").is_err());
    assert!(sql("DELETE FROM accepted_admin_operations").is_err());
    run(
        &db,
        AuditWrite::BindRestart {
            key: key("op-1", 1),
            restart_id: "r".into(),
        },
    )
    .unwrap();
    assert!(sql("UPDATE accepted_admin_operations SET restart_id = 'other'").is_err());
    finish(&db, "op-1", 9);
    assert!(sql("UPDATE accepted_admin_operations SET outcome = 'failed'").is_err());
    assert!(sql("UPDATE accepted_admin_operations SET state = 'started', outcome = NULL, finished_at_ms = NULL").is_err());
    assert!(sql("UPDATE accepted_admin_operations SET budget_bytes = 4097").is_err());
    assert!(sql("UPDATE accepted_admin_operations SET budget_bytes = 4096").is_err());
    assert_eq!(
        db.admin_audit_record("op-1").unwrap().unwrap().outcome,
        Some(AuditOutcome::Succeeded)
    );
}

#[test]
fn a_row_that_no_longer_validates_is_reported_not_returned() {
    let db = db();
    run(&db, AuditWrite::Accept(plain("op-1", 1))).unwrap();
    db.conn
        .execute_batch("DROP TRIGGER admin_operations_one_way")
        .unwrap();
    db.conn
        .execute(
            "UPDATE accepted_admin_operations SET accepted_json = replace(accepted_json, 'cancel', 'discard')",
            [],
        )
        .unwrap();
    let error = db.admin_audit_record("op-1").unwrap_err();
    assert!(matches!(error, StorageError::Serialization(_)), "{error:?}");
}

fn denial_key(subject: &str, reason: DenialReason) -> DenialKey {
    DenialKey::Caller {
        principal: Some(principal(subject)),
        surface: AuditSurface::Cli,
        operation: DenialOperation::RestartRequest,
        reason,
    }
}

fn aggregate(key: DenialKey, window: u64, first: u64, last: u64, count: u64) -> DenialAggregate {
    DenialAggregate {
        key,
        window_start_ms: window * DENIAL_WINDOW_MS,
        first_ms: window * DENIAL_WINDOW_MS + first,
        last_ms: window * DENIAL_WINDOW_MS + last,
        count,
    }
}

fn denials(db: &Database, rows: Vec<DenialAggregate>, now_ms: u64) -> (usize, usize) {
    match run(db, AuditWrite::Denials { rows, now_ms }).unwrap() {
        AuditReply::Denials { stored, dropped } => (stored, dropped),
        other => panic!("{other:?}"),
    }
}

#[test]
fn denials_merge_by_window_and_key_and_saturate_the_count() {
    let db = db();
    let key = denial_key("uid-1", DenialReason::Expired);
    let now = 10 * DENIAL_WINDOW_MS;
    assert_eq!(
        denials(&db, vec![aggregate(key.clone(), 10, 500, 600, 2)], now),
        (1, 0)
    );
    assert_eq!(
        denials(&db, vec![aggregate(key.clone(), 10, 100, 900, 3)], now),
        (1, 0)
    );
    // Another window is another row.
    assert_eq!(
        denials(&db, vec![aggregate(key.clone(), 11, 1, 1, 1)], now),
        (1, 0)
    );
    let stored = db.admin_audit_denials(10).unwrap();
    assert_eq!(stored.len(), 2);
    assert_eq!(stored[0], aggregate(key.clone(), 10, 100, 900, 5));
    assert_eq!(
        denials(&db, vec![aggregate(key.clone(), 10, 1, 1, u64::MAX)], now),
        (1, 0)
    );
    assert_eq!(db.admin_audit_denials(10).unwrap()[0].count, u64::MAX);
    let column: i64 = db
        .conn
        .query_row(
            "SELECT count FROM admin_denials ORDER BY window_start_ms LIMIT 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(column, i64::MAX);
}

#[test]
fn an_unidentified_caller_is_stored_as_the_fixed_anonymous_principal() {
    let db = db();
    let anonymous = |principal| DenialKey::Caller {
        principal,
        surface: AuditSurface::Unknown,
        operation: DenialOperation::Status,
        reason: DenialReason::Unauthenticated,
    };
    let now = DENIAL_WINDOW_MS;
    denials(&db, vec![aggregate(anonymous(None), 1, 1, 1, 1)], now);
    denials(
        &db,
        vec![aggregate(
            anonymous(Some(AuditPrincipal::anonymous())),
            1,
            2,
            2,
            1,
        )],
        now,
    );
    let stored = db.admin_audit_denials(10).unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].count, 2);
    assert_eq!(stored[0].key, anonymous(Some(AuditPrincipal::anonymous())));
}

#[test]
fn a_window_holds_256_keys_and_merges_the_rest_into_overflow() {
    let db = db();
    let now = DENIAL_WINDOW_MS;
    let rows = |range: std::ops::Range<u32>| -> Vec<DenialAggregate> {
        range
            .map(|n| {
                aggregate(
                    denial_key(&format!("uid-{n}"), DenialReason::Expired),
                    1,
                    1,
                    1,
                    1,
                )
            })
            .collect()
    };
    for start in [0, 64, 128, 192] {
        assert_eq!(denials(&db, rows(start..start + 64), now), (64, 0));
    }
    assert_eq!(db.admin_audit_denials(1_000).unwrap().len(), 256);
    // The 257th and later keys all land on the one overflow row.
    assert_eq!(denials(&db, rows(256..257), now), (1, 0));
    let stored = db.admin_audit_denials(1_000).unwrap();
    assert_eq!(stored.len(), 257);
    let overflow: Vec<_> = stored
        .iter()
        .filter(|row| row.key == DenialKey::Overflow)
        .collect();
    assert_eq!(overflow.len(), 1);
    assert_eq!(overflow[0].count, 1);
    assert_eq!(denials(&db, rows(257..321), now), (64, 0));
    let stored = db.admin_audit_denials(1_000).unwrap();
    assert_eq!(stored.len(), 257);
    assert_eq!(
        stored
            .iter()
            .find(|row| row.key == DenialKey::Overflow)
            .unwrap()
            .count,
        65
    );
    // A key that already has its row still merges into it.
    assert_eq!(denials(&db, rows(0..1), now), (1, 0));
    assert_eq!(db.admin_audit_denials(1_000).unwrap().len(), 257);
    // The next window starts again with its own 256.
    assert_eq!(
        denials(
            &db,
            vec![aggregate(
                denial_key("uid-0", DenialReason::Expired),
                2,
                1,
                1,
                1
            )],
            2 * DENIAL_WINDOW_MS
        ),
        (1, 0)
    );
    assert_eq!(db.admin_audit_denials(1_000).unwrap().len(), 258);
}

#[test]
fn a_flood_of_distinct_callers_stays_inside_the_bounds() {
    let db = db();
    let now = 5 * DENIAL_WINDOW_MS;
    let mut submitted = 0u64;
    for batch in 0..100u32 {
        let rows: Vec<_> = (0..64u32)
            .map(|n| {
                aggregate(
                    denial_key(
                        &format!("uid-{}", batch * 64 + n),
                        DenialReason::StaleBinding,
                    ),
                    5,
                    1,
                    1,
                    1,
                )
            })
            .collect();
        submitted += rows.len() as u64;
        assert_eq!(denials(&db, rows, now), (64, 0));
    }
    let stats = db.admin_audit_stats().unwrap();
    assert_eq!(stats.denial_count, 257);
    assert!(stats.denial_bytes <= DENIAL_MAX_BYTES);
    let total: u64 = db
        .admin_audit_denials(1_000)
        .unwrap()
        .iter()
        .map(|row| row.count)
        .sum();
    assert_eq!(total, submitted);
    assert_eq!(stats.accepted_count, 0);
}

#[test]
fn the_row_bound_of_denials_evicts_the_oldest_window_and_never_touches_operations() {
    let db = db();
    run(&db, AuditWrite::Accept(plain("op-1", 1))).unwrap();
    let now = 2_000 * DENIAL_WINDOW_MS;
    let one = |window: u64| aggregate(denial_key("uid-1", DenialReason::Expired), window, 1, 1, 1);
    for start in (1_000..1_960).step_by(64) {
        let rows: Vec<_> = (start..(start + 64).min(1_960)).map(one).collect();
        assert_eq!(denials(&db, rows.clone(), now), (rows.len(), 0));
    }
    // 960 rows so far; fill to exactly 1,000.
    assert_eq!(
        denials(&db, (1_960..2_000).map(one).collect(), now),
        (40, 0)
    );
    assert_eq!(db.admin_audit_stats().unwrap().denial_count, 1_000);
    // The 1,001st evicts the oldest window and still stores the new row.
    assert_eq!(
        denials(&db, vec![one(2_000)], now + DENIAL_WINDOW_MS),
        (1, 0)
    );
    let stored = db.admin_audit_denials(2_000).unwrap();
    assert_eq!(stored.len(), 1_000);
    assert_eq!(stored[0].window_start_ms, 1_001 * DENIAL_WINDOW_MS);
    assert_eq!(stored[999].window_start_ms, 2_000 * DENIAL_WINDOW_MS);
    assert!(db.admin_audit_record("op-1").unwrap().is_some());
}

#[test]
fn denials_beyond_the_eviction_batch_are_dropped_and_counted() {
    let db = db();
    let limits = Limits {
        denial_rows: 2,
        evictions: 2,
        ..Limits::default()
    };
    let now = 100 * DENIAL_WINDOW_MS;
    let write = |windows: &[u64]| match run_with(
        &db,
        AuditWrite::Denials {
            rows: windows
                .iter()
                .map(|w| aggregate(denial_key("uid-1", DenialReason::Expired), *w, 1, 1, 1))
                .collect(),
            now_ms: now,
        },
        &limits,
    )
    .unwrap()
    {
        AuditReply::Denials { stored, dropped } => (stored, dropped),
        other => panic!("{other:?}"),
    };
    assert_eq!(write(&[1, 2]), (2, 0));
    // Two evictions are allowed per batch; the third new row finds no room left.
    assert_eq!(write(&[3, 4, 5]), (2, 1));
    assert_eq!(db.admin_audit_stats().unwrap().denial_count, 2);
    // The bound check is on the batch size too.
    let limits = Limits { batch: 2, ..limits };
    let too_many = AuditWrite::Denials {
        rows: (1..4)
            .map(|w| aggregate(DenialKey::Overflow, w, 1, 1, 1))
            .collect(),
        now_ms: now,
    };
    assert!(matches!(
        audit_error(run_with(&db, too_many, &limits)),
        AuditError::Invalid(_)
    ));
}

#[test]
fn a_batch_of_sixty_four_is_taken_and_sixty_five_is_refused_whole() {
    let db = db();
    let now = 10 * DENIAL_WINDOW_MS;
    let rows = |count: u32| -> Vec<DenialAggregate> {
        (0..count)
            .map(|n| {
                aggregate(
                    denial_key(&format!("uid-{n}"), DenialReason::Expired),
                    10,
                    1,
                    1,
                    1,
                )
            })
            .collect()
    };
    assert_eq!(denials(&db, rows(64), now), (64, 0));
    let before = db.admin_audit_stats().unwrap();
    assert!(matches!(
        audit_error(run(
            &db,
            AuditWrite::Denials {
                rows: rows(65),
                now_ms: now
            }
        )),
        AuditError::Invalid(_)
    ));
    assert_eq!(db.admin_audit_stats().unwrap(), before);
}

#[test]
fn the_byte_bound_of_denials_drops_what_does_not_fit() {
    let db = db();
    let now = 10 * DENIAL_WINDOW_MS;
    let row = aggregate(denial_key("uid-1", DenialReason::Expired), 10, 1, 1, 1);
    let first = denials(&db, vec![row.clone()], now);
    assert_eq!(first, (1, 0));
    let budget = db.admin_audit_stats().unwrap().denial_bytes;
    let limits = Limits {
        denial_bytes: budget,
        evictions: 1,
        ..Limits::default()
    };
    // The new key evicts the old one (the single eviction of the batch); the old key
    // then comes back as a new row, finds no eviction left and is dropped.
    let other = aggregate(denial_key("uid-2", DenialReason::Expired), 10, 1, 1, 1);
    let AuditReply::Denials { stored, dropped } = run_with(
        &db,
        AuditWrite::Denials {
            rows: vec![other, row],
            now_ms: now,
        },
        &limits,
    )
    .unwrap() else {
        panic!("not denials");
    };
    assert_eq!((stored, dropped), (1, 1));
    assert_eq!(db.admin_audit_stats().unwrap().denial_bytes, budget);
}

#[test]
fn denials_expire_exactly_twenty_four_hours_after_their_window_started() {
    let db = db();
    let key = denial_key("uid-1", DenialReason::Expired);
    let window_start = 3 * DENIAL_WINDOW_MS;
    let row = aggregate(key, 3, 5, 6, 1);
    // Already past retention when written: dropped, not stored.
    assert_eq!(
        denials(&db, vec![row.clone()], window_start + DAY_MS),
        (0, 1)
    );
    assert_eq!(denials(&db, vec![row], window_start + DAY_MS - 1), (1, 0));
    let cleanup = |now_ms| match run(&db, AuditWrite::Cleanup { now_ms }).unwrap() {
        AuditReply::Cleanup(done) => done,
        other => panic!("{other:?}"),
    };
    assert_eq!(cleanup(window_start + DAY_MS - 1).denials_removed, 0);
    let done = cleanup(window_start + DAY_MS);
    assert_eq!((done.denials_removed, done.more), (1, false));
    assert!(db.admin_audit_denials(10).unwrap().is_empty());
}

#[test]
fn malformed_denials_are_invalid() {
    let db = db();
    let now = 10 * DENIAL_WINDOW_MS;
    let good = aggregate(denial_key("uid-1", DenialReason::Expired), 10, 1, 1, 1);
    let mut bad = Vec::new();
    let mut row = good.clone();
    row.window_start_ms += 1;
    bad.push(row);
    let mut row = good.clone();
    row.first_ms = row.window_start_ms - 1;
    bad.push(row);
    let mut row = good.clone();
    row.last_ms = row.first_ms - 1;
    bad.push(row);
    let mut row = good.clone();
    row.last_ms = row.window_start_ms + DENIAL_WINDOW_MS;
    bad.push(row);
    let mut row = good.clone();
    row.count = 0;
    bad.push(row);
    let mut row = good;
    row.key = denial_key(&"s".repeat(257), DenialReason::Expired);
    bad.push(row);
    for row in bad {
        assert!(
            matches!(
                audit_error(run(
                    &db,
                    AuditWrite::Denials {
                        rows: vec![row.clone()],
                        now_ms: now
                    }
                )),
                AuditError::Invalid(_)
            ),
            "{row:?}"
        );
    }
    assert_eq!(db.admin_audit_stats().unwrap().denial_count, 0);
}

fn insert_restart(db: &Database, id: &str, phase: &str, ready_at_ms: Option<u64>) {
    db.conn
        .execute(
            "INSERT INTO restart_transactions(restart_id, capture_id, revision, phase, \
             metadata_json, old_builtin_tool_names_json, ready_at_ms) \
             VALUES (?1, ?2, 0, ?3, '{}', '[]', ?4)",
            params![id, format!("capture-{id}"), phase, ready_at_ms],
        )
        .unwrap();
}

fn accept_restart(db: &Database, id: &str, epoch: u64, restart_id: Option<&str>) {
    run(
        db,
        AuditWrite::Accept(accepted(id, 1, AuditOperation::RestartRequest, epoch, 1)),
    )
    .unwrap();
    if let Some(restart_id) = restart_id {
        run(
            db,
            AuditWrite::BindRestart {
                key: key(id, 1),
                restart_id: restart_id.into(),
            },
        )
        .unwrap();
    }
}

fn reconcile(db: &Database, mode: AuditReconcile, now_ms: u64) -> AuditCleanup {
    match run(db, AuditWrite::Reconcile { mode, now_ms }).unwrap() {
        AuditReply::Cleanup(done) => done,
        other => panic!("{other:?}"),
    }
}

fn outcome_of(db: &Database, id: &str) -> Option<AuditOutcome> {
    db.admin_audit_record(id).unwrap().unwrap().outcome
}

#[test]
fn a_cold_reconcile_ends_old_epoch_rows_from_the_restart_transactions() {
    let db = db();
    insert_restart(&db, "ready", "adopted", Some(50));
    insert_restart(&db, "ready-complete", "completed", Some(50));
    insert_restart(&db, "aborted", "aborted", None);
    insert_restart(&db, "invalidated", "invalidated", None);
    insert_restart(&db, "superseded", "superseded", None);
    insert_restart(&db, "completed-no-ready", "completed", None);
    insert_restart(&db, "armed", "armed", None);
    let cases = [
        ("op-ready", Some("ready"), AuditOutcome::Succeeded),
        (
            "op-ready-complete",
            Some("ready-complete"),
            AuditOutcome::Succeeded,
        ),
        ("op-aborted", Some("aborted"), AuditOutcome::Failed),
        ("op-invalidated", Some("invalidated"), AuditOutcome::Failed),
        ("op-superseded", Some("superseded"), AuditOutcome::Failed),
        (
            "op-completed-no-ready",
            Some("completed-no-ready"),
            AuditOutcome::Unknown,
        ),
        ("op-armed", Some("armed"), AuditOutcome::Unknown),
        ("op-missing-tx", Some("not-there"), AuditOutcome::Unknown),
        ("op-unbound", None, AuditOutcome::Unknown),
    ];
    for (id, restart, _) in cases {
        accept_restart(&db, id, 7, restart);
    }
    run(
        &db,
        AuditWrite::Accept(accepted("op-cancel", 1, AuditOperation::Cancel, 7, 1)),
    )
    .unwrap();
    // The live epoch's in-flight operation is not touched.
    accept_restart(&db, "op-live", 8, Some("armed"));
    run(
        &db,
        AuditWrite::Start {
            key: key("op-ready", 1),
        },
    )
    .unwrap();

    let done = reconcile(&db, AuditReconcile::Cold { live_core_epoch: 8 }, 9_000);
    assert_eq!((done.reconciled, done.more), (10, false));
    for (id, _, expected) in cases {
        let record = db.admin_audit_record(id).unwrap().unwrap();
        assert_eq!(record.outcome, Some(expected), "{id}");
        assert_eq!(record.state, AuditState::Finished);
        assert_eq!(record.finished_at_ms, Some(9_000));
    }
    assert_eq!(outcome_of(&db, "op-cancel"), Some(AuditOutcome::Unknown));
    let live = db.admin_audit_record("op-live").unwrap().unwrap();
    assert_eq!((live.state, live.outcome), (AuditState::Prepared, None));
    // Nothing is left for a second pass and the finished rows are not rewritten.
    let again = reconcile(&db, AuditReconcile::Cold { live_core_epoch: 8 }, 9_999);
    assert_eq!((again.reconciled, again.more), (0, false));
    assert_eq!(
        db.admin_audit_record("op-ready")
            .unwrap()
            .unwrap()
            .finished_at_ms,
        Some(9_000)
    );
}

#[test]
fn a_reused_live_epoch_is_never_ended_by_a_cold_reconcile() {
    let db = db();
    accept_restart(&db, "op-1", 7, None);
    let done = reconcile(&db, AuditReconcile::Cold { live_core_epoch: 7 }, 5);
    assert_eq!(done.reconciled, 0);
    assert_eq!(
        db.admin_audit_record("op-1").unwrap().unwrap().state,
        AuditState::Prepared
    );
}

#[test]
fn a_ready_reconcile_ends_only_rows_bound_to_that_restart_after_ready_is_durable() {
    let db = db();
    insert_restart(&db, "r1", "adopted", None);
    insert_restart(&db, "r2", "adopted", Some(5));
    accept_restart(&db, "op-a", 7, Some("r1"));
    accept_restart(&db, "op-b", 7, Some("r1"));
    accept_restart(&db, "op-c", 7, Some("r2"));
    accept_restart(&db, "op-unbound", 7, None);
    // Ready is not durable yet: nothing changes.
    let none = reconcile(
        &db,
        AuditReconcile::Ready {
            restart_id: "r1".into(),
        },
        100,
    );
    assert_eq!((none.reconciled, none.more), (0, false));
    assert_eq!(outcome_of(&db, "op-a"), None);
    db.conn
        .execute(
            "UPDATE restart_transactions SET ready_at_ms = 60 WHERE restart_id = 'r1'",
            [],
        )
        .unwrap();
    let done = reconcile(
        &db,
        AuditReconcile::Ready {
            restart_id: "r1".into(),
        },
        200,
    );
    assert_eq!((done.reconciled, done.more), (2, false));
    assert_eq!(outcome_of(&db, "op-a"), Some(AuditOutcome::Succeeded));
    assert_eq!(outcome_of(&db, "op-b"), Some(AuditOutcome::Succeeded));
    // Another restart's row and an unbound row are not touched.
    assert_eq!(outcome_of(&db, "op-c"), None);
    assert_eq!(outcome_of(&db, "op-unbound"), None);
}

#[test]
fn reconcile_works_in_steps_of_sixty_four() {
    let db = db();
    for index in 0..130u64 {
        let id = format!("op-{index:03}");
        run(
            &db,
            AuditWrite::Accept(accepted(&id, 1, AuditOperation::Cancel, 7, index)),
        )
        .unwrap();
    }
    let mode = || AuditReconcile::Cold { live_core_epoch: 8 };
    let steps: Vec<_> = (0..3)
        .map(|_| {
            let done = reconcile(&db, mode(), 1_000);
            (done.reconciled, done.more)
        })
        .collect();
    assert_eq!(steps, [(64, true), (64, true), (2, false)]);
    assert_eq!(db.admin_audit_stats().unwrap().pending_count, 0);
}

#[test]
fn the_audit_tables_are_part_of_the_v70_schema_and_the_undo_removes_them() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    crate::migrations::apply_up_to(&conn, 69).unwrap();
    let has = |name: &str| -> bool {
        conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name = ?1)",
            [name],
            |row| row.get(0),
        )
        .unwrap()
    };
    assert!(!has("accepted_admin_operations") && !has("admin_denials"));
    // A pre-existing row of another table survives the migration.
    conn.execute_batch("PRAGMA foreign_keys = OFF").unwrap();
    crate::migrations::apply_up_to(&conn, 70).unwrap();
    assert_eq!(
        conn.pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
            .unwrap(),
        70
    );
    assert_eq!(crate::migrations::MIGRATIONS.len(), 70);
    assert!(has("accepted_admin_operations") && has("admin_denials"));
    assert!(has("admin_operations_one_way") && has("admin_operations_keep_unfinished"));
    conn.execute_batch(include_str!("../../undo/undo_v70.sql"))
        .unwrap();
    assert!(!has("accepted_admin_operations") && !has("admin_denials"));
    assert!(!has("admin_operations_one_way") && !has("idx_admin_denials_window"));
}
