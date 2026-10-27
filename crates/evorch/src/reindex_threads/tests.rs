use std::cell::Cell;

use super::*;
use serde_json::json;
use storage::thread_index::{
    BlockOrigin, BlockProvenance, IndexState, MessageProvenance, PROVENANCE_FORMAT_VERSION,
    ProvenanceEvidence, ProvenanceMap, ProvenanceStatus, RequestGapReason, SnapshotWrite,
};

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

fn foreground(handle: &StorageHandle, request: &ReindexRequest, wait: bool) -> ReindexResult {
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

fn unwrap_outcome(outcome: ReindexOutcome) -> ReindexResult {
    match outcome {
        ReindexOutcome::Result(result) => result,
        other => panic!("expected a persisted result, got {other:?}"),
    }
}

fn summary(result: ReindexResult) -> storage::thread_index::RequestSummary {
    match result {
        ReindexResult::Completed { summary, .. }
        | ReindexResult::CompletedWithGaps { summary, .. } => summary,
        other => panic!("expected terminal result, got {other:?}"),
    }
}

#[test]
fn parse_filters_and_reject_bad_boundaries() {
    let args = parse_args(
        [
            "reindex-threads",
            "--db",
            "test.db",
            "--project",
            ".",
            "--since",
            "0",
            "--include-evicted",
            "--wait",
        ]
        .map(str::to_owned)
        .into_iter(),
    )
    .unwrap();
    assert_eq!(args.db, Some("test.db".into()));
    assert_eq!(args.project, Some(".".into()));
    assert_eq!(args.since_ns, Some(0));
    assert!(args.include_evicted && args.wait);
    for tail in [
        vec!["--since", "-1"],
        vec!["--since", "9223372036854775808"],
        vec!["--since", "2026-10-25"],
        vec!["--project"],
        vec!["--db", "--wait"],
        vec!["--wait", "--wait"],
        vec!["--unknown"],
    ] {
        assert!(
            parse_args(
                std::iter::once("reindex-threads")
                    .chain(tail)
                    .map(str::to_owned)
            )
            .is_err()
        );
    }
    assert_eq!(
        parse_args(
            ["reindex-threads", "--since", "9223372036854775807"]
                .map(str::to_owned)
                .into_iter()
        )
        .unwrap()
        .since_ns,
        Some(i64::MAX)
    );
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
    let request = request(&ReindexArgs {
        project: Some(dir.path().join(".")),
        since_ns: Some(20),
        ..Default::default()
    })
    .unwrap();
    assert_eq!(request.canonical_project.as_deref(), Some(project));
    let result = foreground(&handle, &request, false);
    assert!(matches!(result, ReindexResult::Completed { .. }));
    let counts = summary(result);
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
        summary(foreground(
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
            ReindexResult::Completed { .. }
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
        let ReindexResult::Accepted { request_id } = result else {
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
        assert!(matches!(result, ReindexResult::Completed { .. }));
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
    assert!(matches!(result, ReindexResult::Completed { .. }));
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
    assert!(matches!(result, ReindexResult::CompletedWithGaps { .. }));
    assert_eq!(
        summary(result).gaps[0].reason,
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
    assert!(matches!(result, ReindexResult::CompletedWithGaps { .. }));
    assert_eq!(
        summary(result).gaps[0].reason,
        RequestGapReason::OversizedBudget
    );
    handle
        .apply_thread_index_settings(config.thread_index)
        .unwrap();
    assert_eq!(summary(foreground(&handle, &all(), false)).target_count, 0);
    let result = foreground(
        &handle,
        &ReindexRequest {
            include_evicted: true,
            ..all()
        },
        false,
    );
    assert!(matches!(result, ReindexResult::Completed { .. }));
    handle
        .apply_thread_index_settings(ThreadIndexSettings {
            enabled: false,
            ..config.thread_index
        })
        .unwrap();
    let result = foreground(&handle, &all(), false);
    assert!(matches!(result, ReindexResult::CompletedWithGaps { .. }));
    assert_eq!(
        summary(result).gaps[0].reason,
        RequestGapReason::IndexDisabled
    );
}

#[test]
fn unknown_provenance_is_a_reasoned_all_gap_not_a_failure() {
    let (_dir, _config, storage) = fixture();
    source(&storage.handle(), "unknown", "/project", 20, false);
    let result = foreground(&storage.handle(), &all(), false);
    assert!(matches!(result, ReindexResult::CompletedWithGaps { .. }));
    let counts = summary(result);
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
        ReindexResult::Failed {
            request_id: None,
            reason: RequestFailure::InvalidArguments
        }
    );
}

#[test]
fn cli_open_uses_user_settings_without_starting_an_owner_daemon() {
    let (dir, config, storage) = fixture();
    source(&storage.handle(), "native", "/project", 20, true);
    drop(storage);
    std::fs::write(
        dir.path().join("config.toml"),
        "version = 3\n[storage.thread_index]\nenabled = false\nmax_bytes = 1048576\n",
    )
    .unwrap();
    let options = config::LoadOptions {
        user_config_dir: Some(dir.path().into()),
        read_env: false,
        ..Default::default()
    };
    let result = unwrap_outcome(
        run_with_options(
            ReindexArgs {
                db: Some(config.db_path),
                ..Default::default()
            },
            &options,
        )
        .unwrap(),
    );
    assert!(matches!(result, ReindexResult::CompletedWithGaps { .. }));
    assert_eq!(
        summary(result).gaps[0].reason,
        RequestGapReason::IndexDisabled
    );
    let error = run_with_options(
        ReindexArgs {
            db: Some(dir.path().join("missing.db")),
            ..Default::default()
        },
        &options,
    )
    .unwrap_err();
    assert_eq!(error.reason, RequestFailure::DatabaseUnavailable);
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
    let submitted = handle
        .submit_thread_index_request(&all(), &waiting, now_ns(), THREAD_INDEX_LEASE_NS)
        .unwrap();
    let id = submitted.request_id;
    let outcome = drive(
        &handle,
        &waiting,
        &id,
        submitted.progress,
        &mut None,
        &mut now_ns,
        &mut |_| {
            handle.cancel_thread_index_request(&id, now_ns()).unwrap();
        },
    );
    assert!(matches!(outcome, ReindexOutcome::Cancelled { request_id } if request_id == id));
    assert!(matches!(
        handle
            .thread_index_request_status(&id, now_ns())
            .unwrap()
            .progress,
        RequestProgress::Cancelled
    ));
}
