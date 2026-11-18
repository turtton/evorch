use std::cell::Cell;
use std::time::Duration;

use serde_json::json;
use storage::thread_index::{
    BlockOrigin, BlockProvenance, IndexOwnerKind, IndexState, IndexStepResult, LeaseResult,
    MessageProvenance, PROVENANCE_FORMAT_VERSION, ProvenanceEvidence, ProvenanceMap,
    ProvenanceStatus, RequestGapReason, RequestProgress, SnapshotWrite, ThreadIndexSettings,
};
use storage::{Storage, StorageConfig, THREAD_INDEX_LEASE_NS};

use super::*;

/// What a run to its end made of a request (the cases of the area's loop).
#[derive(Debug)]
enum Outcome {
    Result(StorageResult),
    Cancelled,
}

fn now_ns() -> i64 {
    clock_ns()
}

/// Runs a request to its end. The clock and the reaction to a wait are
/// injected, so a test moves work and leases forward with no sleeps.
fn execute(
    handle: &StorageHandle,
    request: &ReindexRequest,
    owner: &IndexOwner,
    wait: bool,
    mut clock: impl FnMut() -> i64,
    mut pause: impl FnMut(Duration),
) -> Outcome {
    let now = clock();
    let mut driver = match submit(handle, request, owner.clone(), wait, now) {
        Submitted::Failed(reason) => {
            return Outcome::Result(StorageResult::Failed {
                request_id: None,
                reason,
            });
        }
        Submitted::Delegated { request_id } => {
            return Outcome::Result(StorageResult::Accepted { request_id });
        }
        Submitted::Driving(driver) => driver,
    };
    let outcome = loop {
        match driver.advance(handle, &mut clock) {
            Advance::Done(Finished::Result(result)) => break Outcome::Result(result),
            Advance::Done(Finished::Cancelled) => {
                break Outcome::Cancelled;
            }
            Advance::Progressed => {}
            Advance::Wait(duration) => pause(duration),
        }
    };
    driver.release(handle);
    outcome
}

fn owner(id: &str, kind: IndexOwnerKind) -> IndexOwner {
    IndexOwner {
        id: id.into(),
        kind,
    }
}

fn fixture() -> (tempfile::TempDir, StorageConfig, Storage) {
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("fixture.db"),
        ..StorageConfig::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    (dir, config, storage)
}

fn source(handle: &StorageHandle, id: &str, project: &str, at: i64, verified: bool) {
    handle
        .save_snapshot_bundle(&SnapshotWrite {
            record: storage::RunContextRecord {
                run_id: id.into(),
                role: "worker".into(),
                name: "worker".into(),
                parent_run_id: None,
                config_json: json!({"thread_id": id}).to_string(),
                messages_json:
                    json!([{"role": "user", "content": [{"type": "text", "text": "hello"}]}])
                        .to_string(),
                checkpoints_json: "[]".into(),
                terminal_phase: "Done".into(),
                restorable: true,
                updated_at_ns: at,
                host_inputs_json: "[]".into(),
            },
            expected_snapshot_revision: None,
            binding: None,
            status: if verified {
                ProvenanceStatus::Verified {
                    evidence: ProvenanceEvidence::CompletePositionMap,
                }
            } else {
                ProvenanceStatus::Unknown {
                    reason: storage::thread_index::ProvenanceUnknownReason::LegacyUnproven,
                }
            },
            map: ProvenanceMap {
                version: PROVENANCE_FORMAT_VERSION,
                raw_message_count: 1,
                checkpoint_count: 0,
                messages: vec![MessageProvenance {
                    raw_index: 0,
                    ordinal: 0,
                    blocks: vec![BlockProvenance {
                        block_index: 0,
                        origin: BlockOrigin::Native,
                        excluded_reason: None,
                    }],
                }],
            },
        })
        .unwrap();
    handle
        .upsert_threads_host(&[storage::threads::ThreadUpsert {
            thread_id: id.into(),
            project_root: Some(project.into()),
            root_run_id: Some(id.into()),
            created_at_ns: at,
            updated_at_ns: at,
            ..Default::default()
        }])
        .unwrap();
}

fn all() -> ReindexRequest {
    ReindexRequest {
        canonical_project: None,
        since_ns: None,
        include_evicted: false,
    }
}

fn foreground(handle: &StorageHandle, request: &ReindexRequest, wait: bool) -> StorageResult {
    let polls = Cell::new(0);
    unwrap_outcome(execute(
        handle,
        request,
        &owner("cli", IndexOwnerKind::Cli),
        wait,
        now_ns,
        |_| {
            polls.set(polls.get() + 1);
            assert!(polls.get() < 100_000, "request must reach terminal");
            std::thread::yield_now();
        },
    ))
}

fn unwrap_outcome(outcome: Outcome) -> StorageResult {
    match outcome {
        Outcome::Result(result) => result,
        other => panic!("expected a persisted result, got {other:?}"),
    }
}

fn summary_of(result: StorageResult) -> storage::thread_index::RequestSummary {
    match result {
        StorageResult::Completed { summary, .. }
        | StorageResult::CompletedWithGaps { summary, .. } => summary,
        other => panic!("expected terminal result, got {other:?}"),
    }
}

#[test]
fn project_is_canonical_and_since_is_inclusive() {
    let (dir, config, storage) = fixture();
    let root = dir.path().canonicalize().unwrap();
    let project = root.to_str().unwrap();
    let handle = storage.handle();
    source(&handle, "old", project, 10, true);
    source(&handle, "boundary", project, 20, true);
    source(&handle, "other", "/other", 30, true);
    let request = ReindexRequest {
        canonical_project: Some(project.to_owned()),
        since_ns: Some(20),
        include_evicted: false,
    };
    let result = foreground(&handle, &request, false);
    assert!(matches!(result, StorageResult::Completed { .. }));
    let counts = summary_of(result);
    assert_eq!((counts.target_count, counts.succeeded), (1, 1));
    let db = storage::Database::open(&config).unwrap();
    assert_eq!(
        db.thread_index_run_status("boundary")
            .unwrap()
            .unwrap()
            .state,
        IndexState::Indexed
    );
    assert_eq!(
        summary_of(foreground(
            &handle,
            &ReindexRequest {
                since_ns: Some(40),
                ..all()
            },
            true
        ))
        .target_count,
        0
    );
}

#[test]
fn foreground_completes_without_owner_with_or_without_wait() {
    for wait in [false, true] {
        let (_dir, _config, storage) = fixture();
        source(&storage.handle(), "native", "/project", 20, true);
        assert!(matches!(
            foreground(&storage.handle(), &all(), wait),
            StorageResult::Completed { .. }
        ));
    }
}

#[test]
fn other_gui_or_cli_owner_returns_accepted_and_wait_never_steps_twice() {
    for kind in [IndexOwnerKind::Gui, IndexOwnerKind::Cli] {
        let (_dir, config, storage) = fixture();
        let handle = storage.handle();
        let peer = Storage::open(config).unwrap();
        let peer_handle = peer.handle();
        source(&handle, "native", "/project", 20, true);
        let other = owner("other", kind);
        let LeaseResult::Acquired(other_lease) = peer_handle
            .acquire_thread_index_lease(&other, now_ns(), THREAD_INDEX_LEASE_NS)
            .unwrap()
        else {
            panic!("lease");
        };
        let result = foreground(&handle, &all(), false);
        let StorageResult::Accepted { request_id } = result else {
            panic!("accepted");
        };
        assert!(matches!(
            handle
                .thread_index_request_status(&request_id, now_ns())
                .unwrap()
                .progress,
            RequestProgress::Pending(_)
        ));
        let calls = Cell::new(0);
        let result = unwrap_outcome(execute(
            &handle,
            &all(),
            &owner("waiting", IndexOwnerKind::Cli),
            true,
            now_ns,
            |_| {
                calls.set(calls.get() + 1);
                assert!(calls.get() < 100_000);
                peer_handle
                    .step_thread_index(&other_lease, now_ns())
                    .unwrap();
                std::thread::yield_now();
            },
        ));
        assert!(calls.get() > 0);
        assert!(matches!(result, StorageResult::Completed { .. }));
        assert_eq!(
            handle
                .thread_index_request_status(&request_id, now_ns())
                .unwrap()
                .owner
                .unwrap()
                .owner,
            other
        );
    }
}

#[test]
fn waiter_takes_over_only_after_expiry() {
    let (_dir, _config, storage) = fixture();
    let handle = storage.handle();
    source(&handle, "native", "/project", 20, true);
    let initial_now = now_ns();
    let LeaseResult::Acquired(old) = handle
        .acquire_thread_index_lease(
            &owner("dead-gui", IndexOwnerKind::Gui),
            initial_now,
            THREAD_INDEX_LEASE_NS,
        )
        .unwrap()
    else {
        panic!("lease");
    };
    let now = Cell::new(initial_now);
    let result = unwrap_outcome(execute(
        &handle,
        &all(),
        &owner("waiter", IndexOwnerKind::Cli),
        true,
        || now.get().max(now_ns()),
        |_| {
            now.set(old.expires_at_ns);
            std::thread::yield_now();
        },
    ));
    assert!(matches!(result, StorageResult::Completed { .. }));
    assert_eq!(
        handle.step_thread_index(&old, now.get()).unwrap(),
        IndexStepResult::LeaseLost
    );
}

#[test]
fn gaps_and_evicted_budget_are_not_failed_or_automatically_revived() {
    let (_dir, config, storage) = fixture();
    let handle = storage.handle();
    source(&handle, "native", "/project", 20, true);
    handle
        .apply_thread_index_settings(ThreadIndexSettings {
            enabled: true,
            max_bytes: 1,
        })
        .unwrap();
    let result = foreground(&handle, &all(), false);
    assert!(matches!(result, StorageResult::CompletedWithGaps { .. }));
    assert_eq!(
        summary_of(result).gaps[0].reason,
        RequestGapReason::OversizedBudget
    );
    let result = foreground(
        &handle,
        &ReindexRequest {
            include_evicted: true,
            ..all()
        },
        false,
    );
    assert!(matches!(result, StorageResult::CompletedWithGaps { .. }));
    assert_eq!(
        summary_of(result).gaps[0].reason,
        RequestGapReason::OversizedBudget
    );
    handle
        .apply_thread_index_settings(config.thread_index)
        .unwrap();
    assert_eq!(
        summary_of(foreground(&handle, &all(), false)).target_count,
        0
    );
    let result = foreground(
        &handle,
        &ReindexRequest {
            include_evicted: true,
            ..all()
        },
        false,
    );
    assert!(matches!(result, StorageResult::Completed { .. }));
    handle
        .apply_thread_index_settings(ThreadIndexSettings {
            enabled: false,
            ..config.thread_index
        })
        .unwrap();
    let result = foreground(&handle, &all(), false);
    assert!(matches!(result, StorageResult::CompletedWithGaps { .. }));
    assert_eq!(
        summary_of(result).gaps[0].reason,
        RequestGapReason::IndexDisabled
    );
}

#[test]
fn unknown_provenance_is_a_reasoned_all_gap_not_a_failure() {
    let (_dir, _config, storage) = fixture();
    source(&storage.handle(), "unknown", "/project", 20, false);
    let result = foreground(&storage.handle(), &all(), false);
    assert!(matches!(result, StorageResult::CompletedWithGaps { .. }));
    let counts = summary_of(result);
    assert_eq!((counts.target_count, counts.succeeded), (1, 0));
    assert!(matches!(
        counts.gaps[0].reason,
        RequestGapReason::Blocked(_)
    ));
}

#[test]
fn invalid_request_is_failed_before_acceptance() {
    let (_dir, _config, storage) = fixture();
    assert_eq!(
        foreground(
            &storage.handle(),
            &ReindexRequest {
                since_ns: Some(-1),
                ..all()
            },
            false
        ),
        StorageResult::Failed {
            request_id: None,
            reason: RequestFailure::InvalidArguments
        }
    );
}

#[test]
fn external_cancel_is_not_reported_as_failed() {
    let (_dir, _config, storage) = fixture();
    let handle = storage.handle();
    source(&handle, "native", "/project", 20, true);
    handle
        .acquire_thread_index_lease(
            &owner("gui", IndexOwnerKind::Gui),
            now_ns(),
            THREAD_INDEX_LEASE_NS,
        )
        .unwrap();
    let waiting = owner("waiting", IndexOwnerKind::Cli);
    let Submitted::Driving(mut driver) = submit(&handle, &all(), waiting, true, now_ns()) else {
        panic!("a waiting caller drives");
    };
    let id = driver.request_id().to_owned();
    let mut clock = now_ns;
    let finished = loop {
        match driver.advance(&handle, &mut clock) {
            Advance::Done(finished) => break finished,
            Advance::Progressed => {}
            Advance::Wait(_) => {
                handle.cancel_thread_index_request(&id, now_ns()).unwrap();
            }
        }
    };
    assert_eq!(finished, Finished::Cancelled);
    assert!(matches!(
        handle
            .thread_index_request_status(&id, now_ns())
            .unwrap()
            .progress,
        RequestProgress::Cancelled
    ));
}

/// A store whose WAL high-water is held by another connection and whose only
/// WAL headroom is one byte, with a reader pinning that WAL: the index cannot
/// admit any step until the reader leaves.
struct PinnedWal {
    _dir: tempfile::TempDir,
    storage: Storage,
    reader: Option<rusqlite::Connection>,
    _pressure: rusqlite::Connection,
}

impl PinnedWal {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("pinned.db");
        let config = StorageConfig {
            db_path: db_path.clone(),
            ..StorageConfig::default()
        };
        {
            let storage = Storage::open(config.clone()).unwrap();
            source(&storage.handle(), "native", "/project", 20, true);
        }
        // Above the merge bound (8,437,792 B) plus one commit, so a truncated
        // WAL admits every step; the high-water alone does not.
        let pressure = rusqlite::Connection::open(&db_path).unwrap();
        pressure
            .pragma_update(None, "wal_autocheckpoint", 0)
            .unwrap();
        pressure
            .execute_batch(
                "CREATE TABLE wal_pressure(payload BLOB); \
                 INSERT INTO wal_pressure VALUES(zeroblob(12582912));",
            )
            .unwrap();
        let high_water = std::fs::metadata(dir.path().join("pinned.db-wal"))
            .unwrap()
            .len();
        assert!(high_water > 12 << 20);
        let storage = Storage::open(StorageConfig {
            hard_limits: storage::HardLimits {
                max_wal_bytes: high_water + 1,
                ..storage::HardLimits::default()
            },
            ..config
        })
        .unwrap();
        let reader = rusqlite::Connection::open(&db_path).unwrap();
        reader
            .execute_batch("BEGIN; SELECT COUNT(*) FROM run_contexts;")
            .unwrap();
        Self {
            _dir: dir,
            storage,
            reader: Some(reader),
            _pressure: pressure,
        }
    }

    fn release(&mut self) {
        if let Some(reader) = self.reader.take() {
            reader.execute_batch("COMMIT").unwrap();
        }
    }
}

/// Runs a foreground request; `on_pause` may change the world and returns how
/// far the injected clock moves. Every requested pause stays within one second.
fn pinned_foreground(
    pinned: &mut PinnedWal,
    mut on_pause: impl FnMut(&mut PinnedWal, usize) -> i64,
) -> StorageResult {
    let handle = pinned.storage.handle();
    let offset = Cell::new(0_i64);
    let pauses = Cell::new(0_usize);
    let pinned = std::cell::RefCell::new(pinned);
    unwrap_outcome(execute(
        &handle,
        &all(),
        &owner("cli", IndexOwnerKind::Cli),
        true,
        || now_ns() + offset.get(),
        |duration| {
            assert!(duration <= Duration::from_secs(1), "{duration:?}");
            let index = pauses.get();
            pauses.set(index + 1);
            assert!(index < 100_000, "request must reach terminal");
            let jump = on_pause(&mut pinned.borrow_mut(), index);
            offset.set(offset.get() + jump + i64::try_from(duration.as_nanos()).unwrap());
        },
    ))
}

#[test]
fn foreground_waits_for_a_pinned_wal_and_completes_without_gaps_when_the_reader_leaves() {
    let mut pinned = PinnedWal::new();
    let result = pinned_foreground(&mut pinned, |pinned, index| {
        if index == 0 {
            pinned.release();
        }
        0
    });
    let StorageResult::Completed { summary, .. } = result else {
        panic!("expected completion, got {result:?}");
    };
    assert_eq!((summary.target_count, summary.succeeded), (1, 1));
    assert!(summary.gaps.is_empty());
}

#[test]
fn foreground_closes_with_capacity_wait_expired_when_the_reader_never_leaves() {
    let mut pinned = PinnedWal::new();
    let limit_ns = 301_000_000_000;
    let result = pinned_foreground(
        &mut pinned,
        |_, index| if index == 0 { limit_ns } else { 0 },
    );
    let StorageResult::CompletedWithGaps { summary, .. } = result else {
        panic!("a wait expiry is a gap, not a failure: {result:?}");
    };
    assert_eq!(summary.gaps.len(), 1);
    assert_eq!(
        summary.gaps[0].reason,
        RequestGapReason::CapacityWaitExpired
    );
    pinned.release();
}
