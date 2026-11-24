use super::*;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use storage::goal_usage_test_support::{Point, install};

fn charge(tokens: u64) -> Usage {
    Usage {
        input_tokens: tokens * 4 / 5,
        output_tokens: tokens - tokens * 4 / 5,
        ..Default::default()
    }
}

fn fixture(
    durable: bool,
) -> (
    AgentRuntime,
    Arc<Model>,
    event_bus::EventReceiver,
    RunId,
    ThreadGoalSnapshot,
) {
    let (runtime, model, events) = harness_with_store(vec![], vec![], durable);
    let root = runtime.reserve_run_id();
    runtime.bind_thread_root("fixture", root).unwrap();
    let goal = runtime
        .create_thread_goal(
            "fixture",
            root,
            "Research the implementation and verify the resulting contract. ".repeat(16),
            vec!["Verified evidence".into(), "Reproducible comparison".into()],
        )
        .unwrap();
    runtime
        .set_goal_checks_paused("fixture", &goal.goal_id, true)
        .unwrap();
    (runtime, model, events, root, goal)
}

fn reopened(runtime: &AgentRuntime, model: &Model) -> AgentRuntime {
    let config = storage::StorageConfig {
        db_path: model._dir.path().join("review.db"),
        ..Default::default()
    };
    AgentRuntime::new(
        runtime.shared.bus.clone(),
        runtime.shared.executor.lock().unwrap().clone(),
        // No run is started during restoration.
        Arc::new(IdleModel),
    )
    .with_run_store(crate::RunStore::open(&config, model._storage.handle()).unwrap())
}

struct IdleModel;
#[async_trait::async_trait]
impl AgentModel for IdleModel {
    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "test".into()
    }
    async fn complete(
        &self,
        _: &AgentInvocationContext,
        _: Role,
        _: &[Message],
        _: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        unreachable!("restoring a goal must not start a run")
    }
}

#[derive(Debug)]
struct Measured {
    count: u64,
    bytes: u64,
    max_payload: u64,
    rows: u64,
    upserts: usize,
    initial_files: [u64; 3],
    final_files: [u64; 3],
    usage_value_bytes: u64,
    snapshot: ThreadGoalSnapshot,
    restored: ThreadGoalSnapshot,
}

fn file_sizes(path: &std::path::Path) -> [u64; 3] {
    ["", "-wal", "-shm"].map(|suffix| {
        match std::fs::metadata(format!("{}{suffix}", path.display())) {
            Ok(meta) => meta.len(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
            Err(error) => panic!("fixture size measurement failed: {error}"),
        }
    })
}

fn workload(tokens: &[u64], durable: bool) -> Measured {
    let (runtime, model, mut events, root, goal) = fixture(durable);
    let initial_files = file_sizes(&model._dir.path().join("review.db"));
    let writes = Arc::new(AtomicUsize::new(0));
    let _hook = install(&goal.goal_id, {
        let writes = writes.clone();
        move |point| {
            if point == Point::BeforeCommit {
                writes.fetch_add(1, Ordering::SeqCst);
            }
            Ok(())
        }
    });
    assert_eq!(
        runtime.shared.run_store.get().map(|s| s
            .with_database(|db| db.thread_goal_usage(&goal.goal_id))
            .unwrap()),
        durable.then_some(None)
    );
    for &tokens in tokens {
        assert!(
            runtime
                .goal_model_request(root, crate::RunPurpose::General, None)
                .unwrap()
        );
        runtime
            .goal_usage(root, crate::RunPurpose::General, charge(tokens))
            .unwrap();
    }
    let snapshot = runtime.thread_goal("fixture").unwrap();
    let (pending, lag) = events.drain_pending_snapshot_counting_lag();
    assert_eq!(lag, 0);
    let mut count = 0;
    let mut latest = None;
    for event in pending {
        if let event_bus::EventKind::Orchestrator(OrchestratorEvent::ThreadGoalUpdated {
            snapshot,
        }) = &event.kind
        {
            latest = Some(snapshot.clone());
            model._storage.handle().append_event(None, &event).unwrap();
            count += 1;
        }
    }
    let conn = rusqlite::Connection::open_with_flags(
        model._dir.path().join("review.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let (bytes, max_payload) = conn.query_row("SELECT SUM(length(CAST(payload AS BLOB))), MAX(length(CAST(payload AS BLOB))) FROM events", [],
        |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
    let rows = conn
        .query_row("SELECT COUNT(*) FROM thread_goal_usage_current", [], |r| {
            r.get(0)
        })
        .unwrap();
    let usage_value_bytes = conn.query_row("SELECT COALESCE(SUM(length(CAST(goal_id AS BLOB)) + 24), 0) FROM thread_goal_usage_current", [], |row| row.get(0)).unwrap();
    let final_files = file_sizes(&model._dir.path().join("review.db"));
    let before_restore = writes.load(Ordering::SeqCst);
    let restored_runtime = if durable {
        reopened(&runtime, &model)
    } else {
        let bus = Arc::new(event_bus::EventBus::new(32));
        AgentRuntime::new(
            bus.clone(),
            Arc::new(::tools::ToolExecutor::new(bus)),
            Arc::new(IdleModel),
        )
    };
    restored_runtime
        .restore_thread_goal(latest.unwrap())
        .unwrap();
    assert_eq!(
        writes.load(Ordering::SeqCst),
        before_restore,
        "restore must not initialize a usage row"
    );
    let restored = restored_runtime.thread_goal("fixture").unwrap();
    Measured {
        count,
        bytes,
        max_payload,
        rows,
        upserts: before_restore,
        initial_files,
        final_files,
        usage_value_bytes,
        snapshot,
        restored,
    }
}

#[tokio::test]
async fn representative_workloads_reduce_actual_payload_and_restore_equivalently() {
    let fixture_path = std::path::Path::new(&std::env::var("CARGO_MANIFEST_DIR").unwrap())
        .join("fixtures/thread-goal-usage/measured-mixture.json");
    let mixture: Vec<u64> = serde_json::from_slice(&std::fs::read(fixture_path).unwrap()).unwrap();
    let mut cases: Vec<(String, Vec<u64>)> = [5_000, 124_671, 200_000, 342_668, 500_000]
        .into_iter()
        .map(|n| (n.to_string(), vec![n; 1024]))
        .collect();
    cases.push(("measured-mixture".into(), mixture));
    for (name, tokens) in cases {
        let mut legacy = workload(&tokens, false);
        let mut durable = workload(&tokens, true);
        let total: u128 = tokens.iter().copied().map(u128::from).sum();
        let bound = 2 + (tokens.len() as u128).div_ceil(16) + total / 8_000_000;
        assert_eq!(legacy.count, 2050);
        assert!(u128::from(durable.count) <= bound);
        assert!(durable.count * 10 <= legacy.count);
        assert!(durable.bytes * 10 <= legacy.bytes);
        assert!(u128::from(durable.bytes) <= bound * u128::from(durable.max_payload));
        assert_eq!((durable.rows, durable.upserts), (1, 1024));
        assert_eq!((legacy.rows, legacy.upserts), (0, 0));
        for snapshot in [
            &mut legacy.snapshot,
            &mut durable.snapshot,
            &mut legacy.restored,
            &mut durable.restored,
        ] {
            snapshot.root_run_id = "run-1".into();
        }
        legacy.snapshot.goal_id = "normalized".into();
        durable.snapshot.goal_id = "normalized".into();
        legacy.restored.goal_id = "normalized".into();
        durable.restored.goal_id = "normalized".into();
        assert_eq!(legacy.snapshot, durable.snapshot);
        assert_eq!(legacy.restored, durable.restored);
        println!(
            "GOAL_FILES {name} legacy_before={:?} legacy_after={:?} durable_before={:?} durable_after={:?} usage_value_bytes={} writer_transactions_legacy={} writer_transactions_durable={}",
            legacy.initial_files,
            legacy.final_files,
            durable.initial_files,
            durable.final_files,
            durable.usage_value_bytes,
            legacy.count + legacy.upserts as u64,
            durable.count + durable.upserts as u64
        );
        println!(
            "GOAL_REDUCTION {name} legacy_count={} legacy_bytes={} durable_count={} durable_bytes={} bound={bound} pmax={} usage_rows={} upserts={}",
            legacy.count,
            legacy.bytes,
            durable.count,
            durable.bytes,
            durable.max_payload,
            durable.rows,
            durable.upserts
        );
    }
}

#[tokio::test]
async fn short_long_tail_and_oversized_workloads_obey_the_bound_not_a_universal_reduction_claim() {
    for tokens in [
        vec![5_000],
        vec![6_233_459],
        vec![8_000_001],
        vec![4_000_001; 20],
    ] {
        let legacy = workload(&tokens, false);
        let durable = workload(&tokens, true);
        let total: u128 = tokens.iter().copied().map(u128::from).sum();
        let bound = 2 + (tokens.len() as u128).div_ceil(16) + total / 8_000_000;
        assert!(u128::from(durable.count) <= bound);
        assert_eq!(durable.upserts, tokens.len());
        assert_eq!(durable.snapshot.usage, legacy.snapshot.usage);
        println!(
            "GOAL_EDGE requests={} tokens={total} old_count={} new_count={} old_bytes={} new_bytes={} bound={bound}",
            tokens.len(),
            legacy.count,
            durable.count,
            legacy.bytes,
            durable.bytes
        );
    }
    let (runtime, _, mut events, root, _) = fixture(true);
    for _ in 0..40 {
        runtime.goal_user_input(root, "Keep researching the same goal");
    }
    let semantic_count = events
        .drain_pending_snapshot()
        .into_iter()
        .filter(|event| {
            matches!(
                event.kind,
                event_bus::EventKind::Orchestrator(OrchestratorEvent::ThreadGoalUpdated { .. })
            )
        })
        .count();
    assert_eq!(
        semantic_count, 42,
        "semantic-heavy workloads are intentionally not thinned"
    );
}

#[tokio::test]
async fn token_threshold_semantics_and_saturation() {
    let (runtime, _, mut events, root, goal) = fixture(true);
    events.drain_pending_snapshot();
    assert!(
        runtime
            .goal_model_request(root, crate::RunPurpose::General, Some(u64::MAX))
            .unwrap()
    );
    assert_eq!(
        events.drain_pending_snapshot().len(),
        1,
        "initial max_tokens is semantic"
    );
    runtime
        .goal_usage(root, crate::RunPurpose::General, charge(8_000_001))
        .unwrap();
    assert_eq!(
        events.drain_pending_snapshot().len(),
        1,
        "token-only checkpoint"
    );
    // Each component's delta must still be visible after the cumulative total saturates.
    {
        let mut goals = runtime.goal_lock();
        let entry = goals.goals.get_mut("fixture").unwrap();
        entry.snapshot.usage.input_tokens = u64::MAX;
        entry.persistence.checkpoint = entry.snapshot.usage.clone();
        entry.snapshot.max_tokens = None;
    }
    runtime
        .goal_usage(
            root,
            crate::RunPurpose::General,
            Usage {
                output_tokens: 8_000_000,
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(events.drain_pending_snapshot().len(), 1);
    // Semantic-only publication does not reset the usage checkpoint.
    runtime
        .set_goal_checks_paused("fixture", &goal.goal_id, false)
        .unwrap();
    events.drain_pending_snapshot();
    for _ in 0..16 {
        assert!(
            runtime
                .goal_model_request(root, crate::RunPurpose::General, None)
                .unwrap()
        );
    }
    assert_eq!(events.drain_pending_snapshot().len(), 1);
}

#[tokio::test]
async fn simultaneous_budget_and_threshold_emit_once_and_equal_budget_allows_response() {
    let (runtime, _, mut events, root, _) = fixture(true);
    runtime
        .goal_model_request(root, crate::RunPurpose::General, Some(8_000_000))
        .unwrap();
    events.drain_pending_snapshot();
    runtime
        .goal_usage(root, crate::RunPurpose::General, charge(8_000_001))
        .unwrap();
    assert_eq!(events.drain_pending_snapshot().len(), 1);
    let blocked = runtime.thread_goal("fixture").unwrap();
    assert_eq!(blocked.phase, ThreadGoalPhase::Blocked);
    assert!(
        !runtime
            .goal_model_request(root, crate::RunPurpose::General, None)
            .unwrap()
    );
    assert!(
        !runtime
            .goal_model_request(root, crate::RunPurpose::General, None)
            .unwrap()
    );
    assert!(
        events.drain_pending_snapshot().is_empty(),
        "identical block is not semantic"
    );
}

#[tokio::test]
async fn concurrent_requests_share_the_fifteen_unsaved_bound() {
    let (runtime, model, _, root, goal) = fixture(true);
    for _ in 0..15 {
        assert!(
            runtime
                .goal_model_request(root, crate::RunPurpose::General, None)
                .unwrap()
        );
    }
    assert_eq!(
        runtime
            .shared
            .run_store
            .get()
            .unwrap()
            .with_database(|db| db.thread_goal_usage(&goal.goal_id))
            .unwrap(),
        None
    );
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let release_rx = Mutex::new(release_rx);
    let first = AtomicBool::new(true);
    let _hook = install(&goal.goal_id, move |point| {
        if point == Point::BeforeCommit && first.swap(false, Ordering::SeqCst) {
            entered_tx.send(()).unwrap();
            release_rx.lock().unwrap().recv().unwrap();
        }
        Ok(())
    });
    std::thread::scope(|scope| {
        let waiting =
            scope.spawn(|| runtime.goal_model_request(root, crate::RunPurpose::General, None));
        entered_rx.recv().unwrap();
        assert_eq!(
            runtime.thread_goal("fixture").unwrap().usage.model_requests,
            15
        );
        runtime
            .set_goal_checks_paused("fixture", &goal.goal_id, false)
            .unwrap();
        let others: Vec<_> = (0..7)
            .map(|n| {
                let runtime = &runtime;
                scope.spawn(move || {
                    runtime.goal_model_request(
                        RunId::new(100 + n),
                        crate::RunPurpose::ThreadGoalReview {
                            root_run_id: root,
                            epoch: 0,
                        },
                        None,
                    )
                })
            })
            .collect();
        release_tx.send(()).unwrap();
        assert!(waiting.join().unwrap().unwrap());
        for other in others {
            assert!(other.join().unwrap().unwrap());
        }
    });
    let goals = runtime.goal_lock();
    let entry = &goals.goals["fixture"];
    assert_eq!(entry.snapshot.usage.model_requests, 23);
    assert!(entry.snapshot.usage.model_requests - entry.persistence.acked.model_requests <= 15);
    let current_requests = entry.snapshot.usage.model_requests;
    drop(goals);
    let restored = reopened(&runtime, &model);
    // Simulate process loss before any usage checkpoint reached the recorder:
    // the semantic seed is old, but the commit-ACKed row survives.
    restored.restore_thread_goal(goal).unwrap();
    let persisted_requests = restored
        .thread_goal("fixture")
        .unwrap()
        .usage
        .model_requests;
    assert!((15..=current_requests).contains(&persisted_requests));
    assert!(current_requests - persisted_requests <= 15);
}

#[tokio::test]
async fn persistence_failure_latches_without_changing_semantics_and_recovers_same_goal() {
    let (runtime, _, _, root, goal) = fixture(true);
    runtime
        .goal_model_request(root, crate::RunPurpose::General, Some(1000))
        .unwrap();
    let fail = Arc::new(AtomicBool::new(true));
    let _hook = install(&goal.goal_id, {
        let fail = fail.clone();
        move |point| {
            if point == Point::BeforeCommit && fail.load(Ordering::SeqCst) {
                Err(storage::StorageError::Io("injected".into()))
            } else {
                Ok(())
            }
        }
    });
    assert!(
        runtime
            .goal_usage(root, crate::RunPurpose::General, charge(100))
            .is_err()
    );
    let before = runtime.thread_goal("fixture").unwrap();
    assert_eq!(before.phase, ThreadGoalPhase::Working);
    assert!(
        runtime
            .goal_model_request(root, crate::RunPurpose::General, None)
            .is_err()
    );
    assert_eq!(runtime.thread_goal("fixture").unwrap(), before);
    // Stop/input operations and another goal do not wait on a persistence latch.
    runtime.goal_work_stopped(root);
    let other = runtime.reserve_run_id();
    runtime.bind_thread_root("other", other).unwrap();
    runtime
        .create_thread_goal("other", other, "Other".into(), vec!["Evidence".into()])
        .unwrap();
    assert!(
        runtime
            .goal_model_request(other, crate::RunPurpose::General, None)
            .unwrap()
    );
    fail.store(false, Ordering::SeqCst);
    assert!(
        runtime
            .goal_model_request(root, crate::RunPurpose::General, None)
            .unwrap()
    );
    let after = runtime.thread_goal("fixture").unwrap();
    assert_eq!(after.goal_id, before.goal_id);
    assert_eq!(after.usage.model_requests, before.usage.model_requests + 1);
    assert_eq!(after.usage.input_tokens, before.usage.input_tokens);
    assert_eq!(
        (after.phase, after.reason, after.checks, after.max_tokens),
        (
            before.phase,
            before.reason,
            before.checks,
            before.max_tokens
        )
    );
}

#[tokio::test]
async fn read_failure_is_goal_local_and_does_not_ack_unsaved_memory() {
    let (runtime, model, _, root, mut goal) = fixture(true);
    goal.usage.model_requests = 7;
    goal.usage.input_tokens = 100;
    model
        ._storage
        .handle()
        .upsert_thread_goal_usage(
            &goal.goal_id,
            "fixture",
            &root.to_string(),
            &ThreadGoalUsage {
                model_requests: 9,
                input_tokens: 50,
                output_tokens: 70,
            },
        )
        .unwrap();
    let fail = Arc::new(AtomicBool::new(true));
    let _hook = install(&goal.goal_id, {
        let fail = fail.clone();
        move |point| {
            if point == Point::Read && fail.load(Ordering::SeqCst) {
                Err(storage::StorageError::Io("read".into()))
            } else {
                Ok(())
            }
        }
    });
    let restored = reopened(&runtime, &model);
    restored.restore_thread_goal(goal.clone()).unwrap();
    assert!(restored.thread_goal("fixture").is_some());
    assert!(
        restored
            .goal_model_request(root, crate::RunPurpose::General, None)
            .is_err()
    );
    assert!(
        restored
            .goal_usage(root, crate::RunPurpose::General, charge(1000))
            .is_err()
    );
    fail.store(false, Ordering::SeqCst);
    assert!(
        restored
            .goal_model_request(root, crate::RunPurpose::General, None)
            .unwrap()
    );
    let goals = restored.goal_lock();
    let entry = &goals.goals["fixture"];
    assert_eq!(entry.snapshot.usage.model_requests, 10);
    assert_eq!(entry.snapshot.usage.input_tokens, 900);
    assert_eq!(
        entry.persistence.acked.input_tokens, 100,
        "read recovery cannot ACK memory-only usage"
    );
    assert_eq!(entry.persistence.acked.output_tokens, 70);
}

#[test]
fn nonce_id_is_bounded_and_random_failure_is_not_a_fallback() {
    let run = format!("run-{}", "z".repeat(26));
    let incarnation = format!("inc-{}", "z".repeat(26));
    let id = new_goal_id(&run, Some(&incarnation), u64::MAX, |bytes| {
        bytes.fill(0xff);
        Ok(())
    })
    .unwrap();
    assert_eq!(id.len(), 127);
    assert!(!id.chars().any(char::is_control));
    assert!(id.ends_with(&"f".repeat(32)));
    assert!(new_goal_id(&run, None, 0, |_| Err("no entropy".into())).is_err());
    assert!(new_goal_id("bad\nid", None, 0, |_| Ok(())).is_err());
    assert!(new_goal_id(&"x".repeat(200), None, 0, |_| Ok(())).is_err());
}

#[tokio::test]
async fn failed_nonce_preserves_old_goal_and_saturated_sequence_never_reuses_id() {
    let (runtime, _, _, root, mut goal) = fixture(true);
    goal.epoch = u64::MAX;
    goal.phase = ThreadGoalPhase::Blocked;
    runtime.restore_thread_goal(goal).unwrap();
    let old = runtime.thread_goal("fixture").unwrap();
    assert!(
        runtime
            .create_goal_random(
                "fixture",
                root,
                "Replacement".into(),
                vec!["Evidence".into()],
                true,
                |_| Err("entropy unavailable".into())
            )
            .is_err()
    );
    assert_eq!(runtime.thread_goal("fixture").unwrap(), old);
    let replacement = runtime
        .create_goal_inner(
            "fixture",
            root,
            "Replacement".into(),
            vec!["Evidence".into()],
            true,
        )
        .unwrap();
    assert_ne!(replacement.goal_id, old.goal_id);
    assert_eq!(replacement.epoch, u64::MAX);
    let mut completed = replacement.clone();
    completed.phase = ThreadGoalPhase::Complete;
    runtime.restore_thread_goal(completed).unwrap();
    let next = runtime
        .create_thread_goal("fixture", root, "Next".into(), vec!["Evidence".into()])
        .unwrap();
    assert_ne!(next.goal_id, replacement.goal_id);
}

#[tokio::test]
async fn old_ack_follows_a_move_without_republishing_a_stale_snapshot() {
    let (runtime, _, mut events, root, goal) = fixture(true);
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let release_rx = Mutex::new(release_rx);
    let _hook = install(&goal.goal_id, move |point| {
        if point == Point::AfterAck {
            entered_tx.send(()).unwrap();
            release_rx.lock().unwrap().recv().unwrap();
        }
        Ok(())
    });
    let target = runtime.reserve_run_id();
    let thread = event_bus::escalation_thread_id(&target.to_string());
    std::thread::scope(|scope| {
        let saving =
            scope.spawn(|| runtime.goal_usage(root, crate::RunPurpose::General, charge(100)));
        entered_rx.recv().unwrap();
        runtime
            .handoff_thread_goal(root, target, None, |_| {})
            .unwrap();
        let moved = runtime.thread_goal(&thread).unwrap();
        events.drain_pending_snapshot();
        release_tx.send(()).unwrap();
        saving.join().unwrap().unwrap();
        assert_eq!(runtime.thread_goal(&thread).unwrap(), moved);
        assert!(events.drain_pending_snapshot().is_empty());
    });
    let goals = runtime.goal_lock();
    assert_eq!(goals.goals[&thread].persistence.acked.input_tokens, 80);
}

#[tokio::test]
async fn old_failure_cannot_latch_a_re_restored_entry_with_the_same_goal_id() {
    let (runtime, _, _, root, goal) = fixture(true);
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let release_rx = Mutex::new(release_rx);
    let _hook = install(&goal.goal_id, move |point| {
        if point == Point::BeforeCommit {
            return Err(storage::StorageError::Io("old save".into()));
        }
        if point == Point::AfterFailure {
            entered_tx.send(()).unwrap();
            release_rx.lock().unwrap().recv().unwrap();
        }
        Ok(())
    });
    std::thread::scope(|scope| {
        let saving =
            scope.spawn(|| runtime.goal_usage(root, crate::RunPurpose::General, charge(100)));
        entered_rx.recv().unwrap();
        runtime.restore_thread_goal(goal.clone()).unwrap();
        release_tx.send(()).unwrap();
        assert!(saving.join().unwrap().is_err());
    });
    let goals = runtime.goal_lock();
    let entry = &goals.goals["fixture"];
    assert_eq!(entry.snapshot.usage, goal.usage);
    assert!(entry.persistence.write_failure.is_none());
    assert_eq!(entry.persistence.acked, goal.usage);
}

#[tokio::test]
async fn normal_old_ack_does_not_clear_failure_and_recovery_cannot_clear_a_new_generation() {
    let (runtime, _, _, root, goal) = fixture(true);
    let fail = Arc::new(AtomicBool::new(false));
    let hold = Arc::new(AtomicBool::new(true));
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let release_rx = Mutex::new(release_rx);
    let _hook = install(&goal.goal_id, {
        let fail = fail.clone();
        let hold = hold.clone();
        move |point| {
            if point == Point::BeforeCommit && fail.load(Ordering::SeqCst) {
                return Err(storage::StorageError::Io("failure generation".into()));
            }
            if point == Point::AfterAck && hold.swap(false, Ordering::SeqCst) {
                entered_tx.send(()).unwrap();
                release_rx.lock().unwrap().recv().unwrap();
            }
            Ok(())
        }
    });
    std::thread::scope(|scope| {
        let old = scope.spawn(|| runtime.goal_usage(root, crate::RunPurpose::General, charge(100)));
        entered_rx.recv().unwrap();
        fail.store(true, Ordering::SeqCst);
        assert!(
            runtime
                .goal_usage(root, crate::RunPurpose::General, charge(200))
                .is_err()
        );
        release_tx.send(()).unwrap();
        old.join().unwrap().unwrap();
    });
    assert!(
        runtime.goal_lock().goals["fixture"]
            .persistence
            .write_failure
            .is_some()
    );
    hold.store(true, Ordering::SeqCst);
    fail.store(false, Ordering::SeqCst);
    std::thread::scope(|scope| {
        let recovery =
            scope.spawn(|| runtime.goal_model_request(root, crate::RunPurpose::General, None));
        entered_rx.recv().unwrap();
        fail.store(true, Ordering::SeqCst);
        assert!(
            runtime
                .goal_usage(root, crate::RunPurpose::General, charge(300))
                .is_err()
        );
        release_tx.send(()).unwrap();
        assert!(recovery.join().unwrap().is_err());
    });
    assert_eq!(
        runtime.thread_goal("fixture").unwrap().usage.model_requests,
        0
    );
    fail.store(false, Ordering::SeqCst);
    assert!(
        runtime
            .goal_model_request(root, crate::RunPurpose::General, None)
            .unwrap()
    );
    let goals = runtime.goal_lock();
    let entry = &goals.goals["fixture"];
    assert!(entry.persistence.write_failure.is_none());
    assert_eq!(entry.persistence.acked.input_tokens, 480);
}
