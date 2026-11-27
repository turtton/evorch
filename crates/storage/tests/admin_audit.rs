//! The audit through the single writer: acknowledgements, lost acknowledgements, a full
//! queue, persistence across reopen and the v70 migration. Everything synchronizes through
//! channels and the writer's own ordering; no test waits for a time to pass.
use std::collections::BTreeSet;
use std::sync::mpsc;

use rusqlite::Connection;
use storage::admin_audit::fault::{AuditFault, AuditFaultPoint};
use storage::admin_audit::*;
use storage::{Database, HardLimits, Storage, StorageConfig, StorageError, StorageHandle};

struct Fixture {
    _dir: tempfile::TempDir,
    config: StorageConfig,
    storage: Storage,
}

impl Fixture {
    fn new() -> Self {
        Self::with(|_| {})
    }

    fn with(adjust: impl FnOnce(&mut StorageConfig)) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let mut config = StorageConfig {
            db_path: dir.path().join("audit.db"),
            ..Default::default()
        };
        adjust(&mut config);
        let storage = Storage::open(config.clone()).unwrap();
        Self {
            _dir: dir,
            config,
            storage,
        }
    }

    fn reopen(self) -> Self {
        let Self {
            _dir,
            config,
            storage,
        } = self;
        storage.close();
        let storage = Storage::open(config.clone()).unwrap();
        Self {
            _dir,
            config,
            storage,
        }
    }

    fn handle(&self) -> StorageHandle {
        self.storage.handle()
    }

    /// An independent connection: what a restarted process or a test would see.
    fn readback(&self) -> Database {
        Database::open(&self.config).unwrap()
    }

    fn audit(&self, write: AuditWrite) -> Result<AuditReply, StorageError> {
        self.handle().admin_audit(write)
    }
}

fn nonce(n: u32) -> String {
    format!("{n:08x}-0000-4000-8000-000000000000")
}

fn key(id: &str, n: u32) -> AuditOperationKey {
    AuditOperationKey {
        operation_id: id.into(),
        attempt_nonce: nonce(n),
    }
}

fn accepted(id: &str, n: u32, operation: AuditOperation, epoch: u64) -> AuditAccepted {
    AuditAccepted {
        key: key(id, n),
        operation,
        surface: AuditSurface::Desktop,
        principal: AuditPrincipal {
            method: "unix-peer".into(),
            subject: "uid-1000".into(),
            binding: "conn-1".into(),
        },
        accepted_core_epoch: epoch,
        intent_digest: "0f".repeat(32),
        accepted_at_ms: 1_000,
    }
}

fn error_of(result: Result<AuditReply, StorageError>) -> AuditError {
    result.unwrap_err().admin_audit().expect("an audit error")
}

fn created(reply: AuditReply) -> AuditRecord {
    match reply {
        AuditReply::Reservation(AuditReservation::Created(record)) => record,
        other => panic!("not created: {other:?}"),
    }
}

fn applied(reply: AuditReply) -> AuditRecord {
    match reply {
        AuditReply::Applied(record) => record,
        other => panic!("not applied: {other:?}"),
    }
}

fn recorded(reply: AuditReply) -> Option<AuditRecord> {
    match reply {
        AuditReply::Record(record) => record,
        other => panic!("not a record: {other:?}"),
    }
}

fn read(fixture: &Fixture, id: &str) -> Option<AuditRecord> {
    recorded(
        fixture
            .audit(AuditWrite::Read {
                operation_id: id.into(),
            })
            .unwrap(),
    )
}

#[test]
fn the_whole_order_is_durable_and_a_second_reader_and_a_reopen_see_it() {
    let fixture = Fixture::new();
    let record = accepted("op-1", 1, AuditOperation::RestartRequest, 3);
    let before = created(fixture.audit(AuditWrite::Accept(record.clone())).unwrap());
    assert_eq!(
        fixture.readback().admin_audit_record("op-1").unwrap(),
        Some(before)
    );
    applied(
        fixture
            .audit(AuditWrite::Start {
                key: key("op-1", 1),
            })
            .unwrap(),
    );
    assert_eq!(
        fixture
            .readback()
            .admin_audit_record("op-1")
            .unwrap()
            .unwrap()
            .state,
        AuditState::Started
    );
    applied(
        fixture
            .audit(AuditWrite::BindRestart {
                key: key("op-1", 1),
                restart_id: "restart-1".into(),
            })
            .unwrap(),
    );
    let done = applied(
        fixture
            .audit(AuditWrite::Finalize {
                key: key("op-1", 1),
                outcome: AuditOutcome::Succeeded,
                at_ms: 5_000,
            })
            .unwrap(),
    );
    assert_eq!(done.restart_id.as_deref(), Some("restart-1"));
    assert_eq!(
        fixture.readback().admin_audit_record("op-1").unwrap(),
        Some(done.clone())
    );

    let fixture = fixture.reopen();
    assert_eq!(read(&fixture, "op-1"), Some(done));
    let AuditReply::Reservation(AuditReservation::Existing(existing)) =
        fixture.audit(AuditWrite::Accept(record)).unwrap()
    else {
        panic!("not existing");
    };
    assert_eq!(existing.state, AuditState::Finished);
}

#[test]
fn a_failure_before_the_commit_leaves_nothing_and_the_readback_says_so() {
    let fixture = Fixture::new();
    let fault = AuditFault::new();
    fixture
        .handle()
        .install_admin_audit_fault(fault.clone())
        .unwrap();
    let record = accepted("op-1", 1, AuditOperation::Cancel, 3);

    fault.arm(AuditFaultPoint::BeforeCommit, 0);
    assert!(matches!(
        fixture.audit(AuditWrite::Accept(record.clone())),
        Err(StorageError::Io(_))
    ));
    assert!(!fault.is_armed());
    assert_eq!(read(&fixture, "op-1"), None);
    assert_eq!(
        fixture
            .readback()
            .admin_audit_stats()
            .unwrap()
            .accepted_count,
        0
    );

    // The retry is a fresh reservation, not a ghost of the failed one.
    created(fixture.audit(AuditWrite::Accept(record)).unwrap());

    // A failed Start leaves the row prepared.
    fault.arm(AuditFaultPoint::BeforeCommit, 0);
    assert!(
        fixture
            .audit(AuditWrite::Start {
                key: key("op-1", 1)
            })
            .is_err()
    );
    assert_eq!(read(&fixture, "op-1").unwrap().state, AuditState::Prepared);
    applied(
        fixture
            .audit(AuditWrite::Start {
                key: key("op-1", 1),
            })
            .unwrap(),
    );

    // A failed Finalize leaves it started, and nothing is ever recorded as finished.
    fault.arm(AuditFaultPoint::BeforeCommit, 0);
    assert!(
        fixture
            .audit(AuditWrite::Finalize {
                key: key("op-1", 1),
                outcome: AuditOutcome::Succeeded,
                at_ms: 9,
            })
            .is_err()
    );
    let after = fixture
        .readback()
        .admin_audit_record("op-1")
        .unwrap()
        .unwrap();
    assert_eq!((after.state, after.outcome), (AuditState::Started, None));
}

#[test]
fn a_lost_acknowledgement_is_resolved_by_reading_the_same_operation_back() {
    let fixture = Fixture::new();
    let fault = AuditFault::new();
    let handle = fixture.handle();
    handle.install_admin_audit_fault(fault.clone()).unwrap();
    let record = accepted("op-1", 1, AuditOperation::Interrupt, 3);

    fault.arm(AuditFaultPoint::AfterCommitDropReply, 0);
    let (reply, lost) = mpsc::channel();
    handle
        .try_submit_admin_audit(
            AuditWrite::Accept(record.clone()),
            Box::new(move |result| {
                let _ = reply.send(result);
            }),
        )
        .unwrap();
    // The callback was dropped uncalled, which closes the channel: the acknowledgement
    // is unknown, never an error and never a success.
    assert!(lost.recv().is_err());
    // The readback is served after the commit and shows the write happened.
    let found = read(&fixture, "op-1").expect("committed before the acknowledgement was lost");
    assert_eq!(found.accepted, record);
    assert_eq!(found.accepted.key.attempt_nonce, nonce(1));
    // Resending the same accept returns the same row: the holder of the nonce may go on.
    let AuditReply::Reservation(AuditReservation::Existing(again)) =
        fixture.audit(AuditWrite::Accept(record)).unwrap()
    else {
        panic!("not existing");
    };
    assert_eq!(again, found);

    // A lost acknowledgement of a Finalize is resolved the same way, and the resend of
    // the same outcome is idempotent.
    fault.arm(AuditFaultPoint::AfterCommitDropReply, 0);
    let (reply, lost) = mpsc::channel();
    handle
        .try_submit_admin_audit(
            AuditWrite::Finalize {
                key: key("op-1", 1),
                outcome: AuditOutcome::Failed,
                at_ms: 77,
            },
            Box::new(move |result| {
                let _ = reply.send(result);
            }),
        )
        .unwrap();
    assert!(lost.recv().is_err());
    let finished = read(&fixture, "op-1").unwrap();
    assert_eq!(
        (finished.state, finished.outcome, finished.finished_at_ms),
        (AuditState::Finished, Some(AuditOutcome::Failed), Some(77))
    );
    let resent = applied(
        fixture
            .audit(AuditWrite::Finalize {
                key: key("op-1", 1),
                outcome: AuditOutcome::Failed,
                at_ms: 999,
            })
            .unwrap(),
    );
    assert_eq!(resent, finished);
}

#[test]
fn an_error_after_the_commit_is_also_resolved_by_readback() {
    let fixture = Fixture::new();
    let fault = AuditFault::new();
    fixture
        .handle()
        .install_admin_audit_fault(fault.clone())
        .unwrap();
    fault.arm(AuditFaultPoint::AfterCommitError, 0);
    let record = accepted("op-1", 1, AuditOperation::Discard, 3);
    assert!(matches!(
        fixture.audit(AuditWrite::Accept(record)),
        Err(StorageError::Io(_))
    ));
    assert_eq!(read(&fixture, "op-1").unwrap().state, AuditState::Prepared);
    // A fault armed for the second write skips the first.
    fault.arm(AuditFaultPoint::BeforeCommit, 1);
    applied(
        fixture
            .audit(AuditWrite::Start {
                key: key("op-1", 1),
            })
            .unwrap(),
    );
    assert!(fault.is_armed());
    assert!(
        fixture
            .audit(AuditWrite::Finalize {
                key: key("op-1", 1),
                outcome: AuditOutcome::Cancelled,
                at_ms: 1,
            })
            .is_err()
    );
    assert!(!fault.is_armed());
}

#[test]
fn another_attempt_nonce_sees_the_row_but_gets_no_right_to_execute() {
    let fixture = Fixture::new();
    created(
        fixture
            .audit(AuditWrite::Accept(accepted(
                "op-1",
                1,
                AuditOperation::Resume,
                3,
            )))
            .unwrap(),
    );
    let AuditReply::Reservation(AuditReservation::Existing(existing)) = fixture
        .audit(AuditWrite::Accept(accepted(
            "op-1",
            2,
            AuditOperation::Resume,
            3,
        )))
        .unwrap()
    else {
        panic!("not existing");
    };
    assert_eq!(existing.accepted.key, key("op-1", 1));
    for write in [
        AuditWrite::Start {
            key: key("op-1", 2),
        },
        AuditWrite::Finalize {
            key: key("op-1", 2),
            outcome: AuditOutcome::Succeeded,
            at_ms: 1,
        },
    ] {
        assert_eq!(
            error_of(fixture.audit(write)),
            AuditError::Conflict(AuditConflict::NonceMismatch)
        );
    }
    assert_eq!(read(&fixture, "op-1").unwrap().state, AuditState::Prepared);
    // The same ID with other facts is a semantic conflict, not a second reservation.
    assert_eq!(
        error_of(fixture.audit(AuditWrite::Accept(accepted(
            "op-1",
            1,
            AuditOperation::Cancel,
            3
        )))),
        AuditError::Conflict(AuditConflict::SemanticMismatch)
    );
}

/// Holds the writer thread inside a callback until the returned sender is used.
fn hold_writer(handle: &StorageHandle) -> mpsc::Sender<()> {
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    handle
        .try_submit_admin_audit(
            AuditWrite::Stats,
            Box::new(move |_| {
                entered_tx.send(()).unwrap();
                let _ = release_rx.recv();
            }),
        )
        .unwrap();
    entered_rx.recv().unwrap();
    release_tx
}

#[test]
fn a_full_queue_is_busy_at_once_and_everything_queued_is_answered_in_order() {
    let fixture = Fixture::with(|config| config.channel_capacity = 2);
    let handle = fixture.handle();
    let release = hold_writer(&handle);

    let (replies_tx, replies_rx) = mpsc::channel();
    let submit = |tag: &'static str, write: AuditWrite| {
        let replies = replies_tx.clone();
        handle.try_submit_admin_audit(
            write,
            Box::new(move |result| {
                replies.send((tag, result)).unwrap();
            }),
        )
    };
    // The writer is held: the queue takes exactly its capacity, then says Busy without
    // waiting, and the refused callback never runs.
    submit(
        "accept",
        AuditWrite::Accept(accepted("op-1", 1, AuditOperation::Cancel, 3)),
    )
    .unwrap();
    submit(
        "read",
        AuditWrite::Read {
            operation_id: "op-1".into(),
        },
    )
    .unwrap();
    let busy = submit("busy", AuditWrite::Stats).unwrap_err();
    assert_eq!(busy.admin_audit(), Some(AuditError::Busy));
    assert_eq!(busy.class(), storage::FailureClass::Busy);

    release.send(()).unwrap();
    let (first_tag, first) = replies_rx.recv().unwrap();
    let (second_tag, second) = replies_rx.recv().unwrap();
    assert_eq!((first_tag, second_tag), ("accept", "read"));
    created(first.unwrap());
    // The read, queued after the accept, sees it.
    assert_eq!(
        recorded(second.unwrap()).unwrap().accepted.key,
        key("op-1", 1)
    );
    drop(replies_tx);
    assert!(replies_rx.recv().is_err());
}

#[test]
fn a_stopped_writer_is_reported_and_drops_the_callback() {
    let fixture = Fixture::new();
    let handle = fixture.handle();
    fixture.storage.close();
    let (reply, dropped) = mpsc::channel::<()>();
    let error = handle
        .try_submit_admin_audit(
            AuditWrite::Stats,
            Box::new(move |_| {
                let _ = reply.send(());
            }),
        )
        .unwrap_err();
    assert_eq!(error, StorageError::WriterClosed);
    assert!(dropped.recv().is_err());
    assert_eq!(
        handle.admin_audit(AuditWrite::Stats),
        Err(StorageError::WriterClosed)
    );
}

#[test]
fn a_panicking_callback_does_not_stop_the_writer() {
    let fixture = Fixture::new();
    let handle = fixture.handle();
    handle
        .try_submit_admin_audit(AuditWrite::Stats, Box::new(|_| panic!("callback bug")))
        .unwrap();
    created(
        fixture
            .audit(AuditWrite::Accept(accepted(
                "op-1",
                1,
                AuditOperation::Cancel,
                3,
            )))
            .unwrap(),
    );
}

#[test]
fn a_database_at_its_size_limit_refuses_audit_writes_as_capacity_but_still_reads() {
    let fixture = Fixture::with(|config| {
        config.hard_limits = HardLimits {
            max_db_bytes: 1,
            ..HardLimits::default()
        }
    });
    assert_eq!(
        error_of(fixture.audit(AuditWrite::Accept(accepted(
            "op-1",
            1,
            AuditOperation::Cancel,
            3
        )))),
        AuditError::Capacity(AuditCapacity::WritesSuspended)
    );
    assert_eq!(read(&fixture, "op-1"), None);
    assert!(matches!(
        fixture.audit(AuditWrite::Stats).unwrap(),
        AuditReply::Stats(_)
    ));
}

#[test]
fn restart_reconcile_runs_through_the_writer_after_ready_is_durable() {
    let fixture = Fixture::new();
    for id in ["op-a", "op-b"] {
        created(
            fixture
                .audit(AuditWrite::Accept(accepted(
                    id,
                    1,
                    AuditOperation::RestartRequest,
                    3,
                )))
                .unwrap(),
        );
        applied(
            fixture
                .audit(AuditWrite::BindRestart {
                    key: key(id, 1),
                    restart_id: "restart-1".into(),
                })
                .unwrap(),
        );
    }
    let reconcile = |mode| match fixture
        .audit(AuditWrite::Reconcile {
            mode,
            now_ms: 8_000,
        })
        .unwrap()
    {
        AuditReply::Cleanup(done) => done,
        other => panic!("{other:?}"),
    };
    // No restart transaction is durable: Ready changes nothing, a cold start ends both as
    // unknown (never as a success) and the live epoch's rows stay.
    assert_eq!(
        reconcile(AuditReconcile::Ready {
            restart_id: "restart-1".into()
        })
        .reconciled,
        0
    );
    created(
        fixture
            .audit(AuditWrite::Accept(accepted(
                "op-live",
                1,
                AuditOperation::Cancel,
                4,
            )))
            .unwrap(),
    );
    let done = reconcile(AuditReconcile::Cold { live_core_epoch: 4 });
    assert_eq!((done.reconciled, done.more), (2, false));
    for id in ["op-a", "op-b"] {
        let record = fixture.readback().admin_audit_record(id).unwrap().unwrap();
        assert_eq!(record.outcome, Some(AuditOutcome::Unknown));
        assert_eq!(record.finished_at_ms, Some(8_000));
    }
    assert_eq!(
        read(&fixture, "op-live").unwrap().state,
        AuditState::Prepared
    );
}

fn denial(subject: &str, window: u64) -> DenialAggregate {
    DenialAggregate {
        key: DenialKey::Caller {
            principal: Some(AuditPrincipal {
                method: "unix-peer".into(),
                subject: subject.into(),
                binding: "conn".into(),
            }),
            surface: AuditSurface::Web,
            operation: DenialOperation::Cancel,
            reason: DenialReason::StaleBinding,
        },
        window_start_ms: window * DENIAL_WINDOW_MS,
        first_ms: window * DENIAL_WINDOW_MS + 1,
        last_ms: window * DENIAL_WINDOW_MS + 2,
        count: 1,
    }
}

fn store_denials(fixture: &Fixture, rows: Vec<DenialAggregate>, now_ms: u64) -> (usize, usize) {
    match fixture.audit(AuditWrite::Denials { rows, now_ms }).unwrap() {
        AuditReply::Denials { stored, dropped } => (stored, dropped),
        other => panic!("{other:?}"),
    }
}

#[test]
fn denials_hold_one_thousand_aggregates_and_never_take_room_from_operations() {
    let fixture = Fixture::new();
    created(
        fixture
            .audit(AuditWrite::Accept(accepted(
                "op-1",
                1,
                AuditOperation::Cancel,
                3,
            )))
            .unwrap(),
    );
    let now = 5_000 * DENIAL_WINDOW_MS;
    let mut window = 4_000;
    let mut stored_total = 0;
    while stored_total < 1_000 {
        let take = (1_000 - stored_total).min(64);
        let rows: Vec<_> = (0..take as u64)
            .map(|n| denial("uid-1", window + n))
            .collect();
        window += take as u64;
        assert_eq!(store_denials(&fixture, rows, now), (take, 0));
        stored_total += take;
    }
    assert_eq!(
        fixture.readback().admin_audit_stats().unwrap().denial_count,
        1_000
    );
    // The 1,001st row evicts the oldest window and is stored.
    assert_eq!(
        store_denials(&fixture, vec![denial("uid-1", window)], now),
        (1, 0)
    );
    let stats = fixture.readback().admin_audit_stats().unwrap();
    assert_eq!((stats.denial_count, stats.accepted_count), (1_000, 1));
    let rows = fixture.readback().admin_audit_denials(2_000).unwrap();
    assert_eq!(rows.len(), 1_000);
    assert_eq!(rows[0].window_start_ms, 4_001 * DENIAL_WINDOW_MS);
    assert!(read(&fixture, "op-1").is_some());
}

#[test]
fn a_flood_of_callers_in_one_window_is_bounded_at_256_keys_plus_overflow() {
    let fixture = Fixture::new();
    let now = 10 * DENIAL_WINDOW_MS;
    let mut submitted = 0;
    for batch in 0..32u64 {
        let rows: Vec<_> = (0..64u64)
            .map(|n| denial(&format!("uid-{}", batch * 64 + n), 10))
            .collect();
        submitted += rows.len() as u64;
        assert_eq!(store_denials(&fixture, rows, now), (64, 0));
    }
    let rows = fixture.readback().admin_audit_denials(1_000).unwrap();
    assert_eq!(rows.len(), 257);
    assert_eq!(rows.iter().map(|row| row.count).sum::<u64>(), submitted);
    let overflow = rows
        .iter()
        .find(|row| row.key == DenialKey::Overflow)
        .unwrap();
    assert_eq!(overflow.count, submitted - 256);
    let stats = fixture.readback().admin_audit_stats().unwrap();
    assert!(stats.denial_bytes <= DENIAL_MAX_BYTES);
}

#[test]
fn nothing_but_the_declared_fields_reaches_the_tables() {
    let fixture = Fixture::new();
    created(
        fixture
            .audit(AuditWrite::Accept(accepted(
                "op-1",
                1,
                AuditOperation::Cancel,
                3,
            )))
            .unwrap(),
    );
    store_denials(&fixture, vec![denial("uid-1", 10)], 10 * DENIAL_WINDOW_MS);
    let conn = Connection::open(&fixture.config.db_path).unwrap();
    let accepted_json: String = conn
        .query_row(
            "SELECT accepted_json FROM accepted_admin_operations",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let object: serde_json::Value = serde_json::from_str(&accepted_json).unwrap();
    let keys: BTreeSet<_> = object.as_object().unwrap().keys().cloned().collect();
    let expected: BTreeSet<_> = [
        "key",
        "operation",
        "surface",
        "principal",
        "accepted_core_epoch",
        "intent_digest",
        "accepted_at_ms",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    assert_eq!(keys, expected);
    let denial_json: String = conn
        .query_row("SELECT record_json FROM admin_denials", [], |row| {
            row.get(0)
        })
        .unwrap();
    for forbidden in ["client", "session", "request", "token", "prompt"] {
        assert!(
            !denial_json.contains(forbidden),
            "{forbidden} in {denial_json}"
        );
    }
}

#[test]
fn opening_a_v69_database_adds_the_audit_tables_without_touching_other_data() {
    let fixture = Fixture::new();
    created(
        fixture
            .audit(AuditWrite::Accept(accepted(
                "op-1",
                1,
                AuditOperation::Cancel,
                3,
            )))
            .unwrap(),
    );
    let config = fixture.config.clone();
    fixture.storage.close();
    let conn = Connection::open(&config.db_path).unwrap();
    conn.execute_batch(concat!(
        include_str!("../undo/undo_v79.sql"),
        include_str!("../undo/undo_v78.sql"),
        include_str!("../undo/undo_v77.sql"),
        include_str!("../undo/undo_v76.sql"),
        include_str!("../undo/undo_v75.sql"),
        include_str!("../undo/undo_v74.sql"),
        include_str!("../undo/undo_v73.sql"),
        include_str!("../undo/undo_v72.sql"),
        include_str!("../undo/undo_v71.sql"),
        include_str!("../undo/undo_v70.sql")
    ))
    .unwrap();
    conn.pragma_update(None, "user_version", 69).unwrap();
    drop(conn);
    let database = Database::open(&config).unwrap();
    assert_eq!(database.admin_audit_record("op-1").unwrap(), None);
    let stats = database.admin_audit_stats().unwrap();
    assert_eq!((stats.accepted_count, stats.denial_count), (0, 0));
    let version: u32 = Connection::open(&config.db_path)
        .unwrap()
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(version, 79);
}
