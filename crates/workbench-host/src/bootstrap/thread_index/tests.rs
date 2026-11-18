//! Real bootstrap/writer lifecycle; channel receipts and injected wall time only.
use crate::bootstrap::{BootstrapOptions, BootstrappedHost, restart::HostRestartPhase};
use rusqlite::{Connection, params};
use std::sync::{
    Arc,
    atomic::{AtomicI64, Ordering},
    mpsc,
};
use storage::{Storage, StorageHandle, thread_index::*};
const S: i64 = 1_000_000_000;
const AT: i64 = 4_000_000_000 * S;
async fn open(dir: &test_socket_dir::SplitDir) -> BootstrappedHost {
    BootstrappedHost::open(BootstrapOptions {
        config_dir: dir.path().into(),
        project_root: dir.path().into(),
        ownership_root: dir.sockets().into(),
    })
    .await
    .unwrap()
}
fn conn(dir: &test_socket_dir::SplitDir) -> Connection {
    Connection::open(dir.path().join("evorch-events.db")).unwrap()
}
fn other() -> IndexOwner {
    IndexOwner {
        id: "legacy-gui".into(),
        kind: IndexOwnerKind::Gui,
    }
}
fn all() -> ReindexRequest {
    ReindexRequest {
        canonical_project: None,
        since_ns: None,
        include_evicted: false,
    }
}
fn fixture(conn: &Connection, id: &str, pending: bool) {
    conn.execute("INSERT INTO thread_index_requests(request_id, include_evicted, state, created_at_ns) VALUES (?1, 0, 'pending', ?2)", params![id, AT]).unwrap();
    conn.execute("INSERT INTO thread_index_request_targets(request_id, run_id, source_revision, extractor_version, accounting_version, work_generation, state, gap_reason) VALUES (?1, 'retained-run', 1, 1, 1, 0, ?2, ?3)", params![id, if pending {"pending"} else {"gap"}, if pending {None} else {Some(r#"{"reason":"capacity_paused"}"#)}]).unwrap();
}
fn state(conn: &Connection, id: &str) -> String {
    conn.query_row(
        "SELECT state FROM thread_index_requests WHERE request_id = ?1",
        [id],
        |r| r.get(0),
    )
    .unwrap()
}
async fn terminal(
    path: std::path::PathBuf,
    id: &str,
    events: mpsc::Receiver<storage::ThreadIndexCounters>,
) {
    let id = id.to_owned();
    tokio::task::spawn_blocking(move || {
        let db = Connection::open(path).unwrap();
        loop {
            events.recv().unwrap();
            if state(&db, &id) != "pending" {
                break;
            }
        }
        assert_eq!(state(&db, &id), "completed_with_gaps");
    })
    .await
    .unwrap();
}
fn writer(host: &BootstrappedHost) -> StorageHandle {
    host.composition.storage_writer.clone()
}

#[tokio::test]
async fn restart_bootstrap_finishes_terminal_wait_without_another_registration() {
    let dir = test_socket_dir::SplitDir::new();
    let mut first = open(&dir).await;
    first.shutdown().await.unwrap();
    drop(first);
    let db = conn(&dir);
    // Durable intermediate state left by capacity/retention, not a new registration.
    fixture(&db, "left-behind", false);
    assert_eq!(state(&db, "left-behind"), "pending");
    let mut host = open(&dir).await;
    let handle = writer(&host);
    let lease = handle
        .thread_index_job_status()
        .unwrap()
        .owner_lease
        .unwrap();
    assert_eq!(lease.owner.kind, IndexOwnerKind::Cli);
    assert!(lease.owner.id.starts_with("core-"));
    terminal(
        host.composition.storage.db_path.clone(),
        "left-behind",
        handle.observe_thread_index_for_test().unwrap(),
    )
    .await;
    assert!(!host.busy(), "passive indexing is not a retention reason");
    host.shutdown().await.unwrap();
}
#[tokio::test]
async fn dead_lease_is_fenced_until_exact_expiry_then_the_core_reacquires() {
    let dir = test_socket_dir::SplitDir::new();
    let mut first = open(&dir).await;
    first.shutdown().await.unwrap();
    drop(first);
    let db = conn(&dir);
    fixture(&db, "dead-owner", false);
    db.execute("UPDATE thread_index_lease SET owner_id = 'dead-gui', owner_kind = 'gui', epoch = epoch + 1, expires_at_ns = ?1 WHERE id = 1", [AT + 10*S]).unwrap();
    let mut host = open(&dir).await;
    let handle = writer(&host);
    let clock = Arc::new(AtomicI64::new(AT));
    handle
        .set_thread_index_clock_for_test(clock.clone())
        .unwrap();
    clock.store(AT + 10 * S - 1, Ordering::SeqCst);
    let before = handle.tick_thread_index_for_test().unwrap();
    assert!(before.owner_lease.is_none());
    assert_eq!(before.counters.steps, 0);
    assert_eq!(state(&db, "dead-owner"), "pending");
    let events = handle.observe_thread_index_for_test().unwrap();
    clock.store(AT + 10 * S, Ordering::SeqCst);
    assert!(
        handle
            .tick_thread_index_for_test()
            .unwrap()
            .owner_lease
            .is_some()
    );
    terminal(
        host.composition.storage.db_path.clone(),
        "dead-owner",
        events,
    )
    .await;
    host.shutdown().await.unwrap();
}
#[tokio::test]
async fn retention_closes_last_pending_target_and_resident_owner_terminalizes_it() {
    let dir = test_socket_dir::SplitDir::new();
    let mut host = open(&dir).await;
    let handle = writer(&host);
    let db = conn(&dir);
    // A retry-wait target cannot be indexed before the actual reclaim operation.
    db.execute_batch("INSERT INTO thread_index_runs(run_id, state, source_revision, snapshot_revision, source_context_sha256, extractor_version, accounting_version, source_updated_at_ns, retention_at_ns, next_attempt_at_ns) VALUES ('retained-run', 'retry_wait', 1, 1, '', 1, 1, 1, 1, 8000000000000000000)").unwrap();
    fixture(&db, "retention", true);
    assert!(host.busy());
    db.execute_batch("INSERT INTO run_tombstones(run_id, thread_id, family_key) VALUES ('retained-run', 'archived', 'family'); INSERT INTO reclaim_jobs(family_key, state, created_at_ns, updated_at_ns) VALUES ('family', 'db_pending', 1, 1)").unwrap();
    let events = handle.observe_thread_index_for_test().unwrap();
    let progress = handle
        .reclaim_step(storage::thread_retention::ReclaimBudget::default())
        .unwrap();
    assert_eq!(progress.last_error, None);
    let target: String = db
        .query_row(
            "SELECT state FROM thread_index_request_targets WHERE request_id = 'retention'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        target, "gap",
        "production reclaim must close the last target"
    );
    // No new registration or owner start. A heartbeat is an explicit timer event.
    handle.tick_thread_index_for_test().unwrap();
    terminal(
        host.composition.storage.db_path.clone(),
        "retention",
        events,
    )
    .await;
    assert!(!host.busy());
    host.shutdown().await.unwrap();
}
#[tokio::test]
async fn only_pending_targets_retain_the_idle_core() {
    let dir = test_socket_dir::SplitDir::new();
    let mut host = open(&dir).await;
    let handle = writer(&host);
    // Hold the DB lease elsewhere, so an owner step cannot race retention assertions.
    handle.stop_thread_index_owner().unwrap();
    handle
        .acquire_thread_index_lease(&other(), AT, 100 * S)
        .unwrap();
    let db = conn(&dir);
    fixture(&db, "idle", true);
    assert!(host.busy());
    db.execute(r#"UPDATE thread_index_request_targets SET state = 'gap', gap_reason = '{"reason":"deleted"}' WHERE request_id = 'idle'"#, []).unwrap();
    assert!(
        !host.busy(),
        "a pending request without pending targets is not active work"
    );
    host.shutdown().await.unwrap();
}
#[tokio::test]
async fn shutdown_and_planned_stop_release_before_the_barrier_and_never_step_again() {
    for planned in [false, true] {
        let dir = test_socket_dir::SplitDir::new();
        let mut host = open(&dir).await;
        let handle = writer(&host);
        let config = host.composition.storage.clone();
        if planned {
            for producer in host.restart_producers() {
                producer.close();
            }
            host.runtime.pause_learning();
            host.runtime.drain_learning_for_restart().await.unwrap();
            host.runtime.seal_learning_for_capture().unwrap();
            let gate = host.host.lock().unwrap().service().restart_gate();
            gate.transition(1, HostRestartPhase::Capturing);
            let resources = host.take_planned_exit(&gate).unwrap();
            resources.lock().await.first_five(|_| Ok(())).await.unwrap();
            handle.barrier().unwrap();
            let before = handle.thread_index_job_status().unwrap().counters;
            fixture(&conn(&dir), "after-barrier", false);
            let after = handle.tick_thread_index_for_test().unwrap();
            assert!(after.owner_lease.is_none());
            assert_eq!(after.counters, before);
            assert_eq!(state(&conn(&dir), "after-barrier"), "pending");
        } else {
            host.shutdown().await.unwrap();
        }
        let next = Storage::open(config).unwrap();
        let lease = next
            .handle()
            .acquire_thread_index_lease(&other(), 1, S)
            .unwrap();
        assert!(
            matches!(lease, LeaseResult::Acquired(_)),
            "handoff must not wait for old expiry: {lease:?}"
        );
        next.close();
        if planned {
            host.shutdown().await.unwrap();
        }
    }
}

// The child runs exactly the legacy GUI's Storage/StartOwner API, not GUI/CEF.
#[test]
fn legacy_gui_process_helper() {
    use std::io::{BufRead, Write};
    let Some(path) = std::env::var_os("EVORCH_INDEX_OWNER_CHILD_DB") else {
        return;
    };
    let storage = Storage::open(storage::StorageConfig {
        db_path: path.into(),
        ..Default::default()
    })
    .unwrap();
    let handle = storage.handle();
    handle
        .set_thread_index_clock_for_test(Arc::new(AtomicI64::new(AT)))
        .unwrap();
    let LeaseResult::Acquired(lease) = handle.start_thread_index_owner(other()).unwrap() else {
        panic!("GUI must start first");
    };
    let report = |label: &str| {
        println!("INDEX_OWNER {label}");
        std::io::stdout().flush().unwrap();
    };
    report("ready");
    for line in std::io::stdin().lock().lines() {
        match line.unwrap().as_str() {
            "tick" => {
                let status = handle.tick_thread_index_for_test().unwrap();
                report(&format!("steps={}", status.counters.steps));
            }
            "stop" => {
                handle.stop_thread_index_owner().unwrap();
                assert_eq!(
                    handle.step_thread_index(&lease, AT).unwrap(),
                    IndexStepResult::LeaseLost
                );
                report("stopped-and-fenced");
            }
            "exit" => break,
            command => panic!("unexpected IPC command {command}"),
        }
    }
    storage.close();
}
struct GuiChild {
    child: std::process::Child,
    input: std::process::ChildStdin,
    output: std::io::BufReader<std::process::ChildStdout>,
}
impl GuiChild {
    fn start(path: &std::path::Path) -> Self {
        use std::process::{Command, Stdio};
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "bootstrap::thread_index::tests::legacy_gui_process_helper",
                "--nocapture",
            ])
            .env("EVORCH_INDEX_OWNER_CHILD_DB", path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let input = child.stdin.take().unwrap();
        let output = std::io::BufReader::new(child.stdout.take().unwrap());
        Self {
            child,
            input,
            output,
        }
    }
    fn event(&mut self) -> String {
        use std::io::BufRead;
        loop {
            let mut line = String::new();
            assert_ne!(
                self.output.read_line(&mut line).unwrap(),
                0,
                "GUI helper ended"
            );
            if let Some(event) = line.strip_prefix("INDEX_OWNER ") {
                return event.trim().to_owned();
            }
        }
    }
    fn command(&mut self, command: &str) -> String {
        use std::io::Write;
        writeln!(self.input, "{command}").unwrap();
        self.input.flush().unwrap();
        self.event()
    }
}
impl Drop for GuiChild {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[tokio::test]
async fn core_and_legacy_gui_processes_exclude_steps_and_transfer_the_lease() {
    let dir = test_socket_dir::SplitDir::new();
    let mut first = open(&dir).await;
    let path = first.composition.storage.db_path.clone();
    first.shutdown().await.unwrap();
    drop(first);
    let mut gui = GuiChild::start(&path);
    assert_eq!(gui.event(), "ready");
    let mut host = open(&dir).await;
    let handle = writer(&host);
    handle
        .set_thread_index_clock_for_test(Arc::new(AtomicI64::new(AT)))
        .unwrap();
    let before = handle.tick_thread_index_for_test().unwrap();
    assert!(before.owner_lease.is_none());
    assert_eq!(before.counters.steps, 0);
    fixture(&conn(&dir), "gui-owned", false);
    assert!(gui.command("tick").starts_with("steps="));
    assert_eq!(state(&conn(&dir), "gui-owned"), "completed_with_gaps");
    assert_eq!(
        handle.tick_thread_index_for_test().unwrap().counters.steps,
        0
    );
    assert_eq!(gui.command("stop"), "stopped-and-fenced");
    fixture(&conn(&dir), "core-owned", false);
    let status = handle.tick_thread_index_for_test().unwrap();
    assert!(status.owner_lease.is_some());
    assert!(status.counters.steps > 0);
    assert_eq!(state(&conn(&dir), "core-owned"), "completed_with_gaps");
    let stopped_steps = gui.command("tick");
    assert_eq!(
        gui.command("tick"),
        stopped_steps,
        "a stopped GUI cannot step after core takeover"
    );
    host.shutdown().await.unwrap();
}

fn snapshot(run: &str) -> SnapshotWrite {
    SnapshotWrite {
        record: storage::RunContextRecord {
            run_id: run.into(),
            role: "worker".into(),
            name: "worker".into(),
            parent_run_id: None,
            config_json: "{}".into(),
            messages_json:
                r#"[{"role":"user","content":[{"type":"text","text":"index fixture"}]}]"#.into(),
            checkpoints_json: "[]".into(),
            terminal_phase: "Done".into(),
            restorable: true,
            updated_at_ns: 1,
            host_inputs_json: "[]".into(),
        },
        expected_snapshot_revision: None,
        binding: None,
        status: ProvenanceStatus::Verified {
            evidence: ProvenanceEvidence::CompletePositionMap,
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
    }
}
#[tokio::test]
async fn resident_disabled_owner_settles_registration_immediately() {
    let settings = ThreadIndexSettings {
        enabled: false,
        max_bytes: 512 * 1024 * 1024,
    };
    let dir = test_socket_dir::SplitDir::new();
    let mut host = open(&dir).await;
    let handle = writer(&host);
    handle.apply_thread_index_settings(settings).unwrap();
    handle
        .save_snapshot_bundle(&snapshot("paused-target"))
        .unwrap();
    // Disabled discovery does not create the index header; seed the same
    // durable header that an enabled owner would have discovered.
    conn(&dir).execute_batch("INSERT OR IGNORE INTO thread_index_runs(run_id, state, source_revision, snapshot_revision, source_context_sha256, extractor_version, accounting_version, source_updated_at_ns, retention_at_ns) SELECT run_id, 'pending', snapshot_revision, snapshot_revision, '', 1, 1, updated_at_ns, 1 FROM run_contexts WHERE run_id = 'paused-target'").unwrap();
    let core = handle
        .thread_index_job_status()
        .unwrap()
        .owner_lease
        .unwrap();
    let now = core.expires_at_ns - 1;
    let submitted = handle
        .submit_thread_index_request(
            &ReindexRequest {
                include_evicted: true,
                ..all()
            },
            &other(),
            now,
            S,
        )
        .unwrap();
    assert!(matches!(submitted.lease, LeaseResult::HeldByOther(_)));
    let RequestProgress::Terminal(ReindexResult::CompletedWithGaps { summary, .. }) =
        submitted.progress
    else {
        panic!(
            "not terminal with gaps: {:?} with {:?}",
            submitted.progress, settings
        );
    };
    assert_eq!(summary.target_count, 1);
    assert_eq!(summary.succeeded, 0);
    assert!(!host.busy());
    host.shutdown().await.unwrap();
}
#[tokio::test]
async fn restart_with_durable_wal_wait_resumes_without_another_registration() {
    let dir = test_socket_dir::SplitDir::new();
    let mut first = open(&dir).await;
    first.shutdown().await.unwrap();
    drop(first);
    let db = conn(&dir);
    fixture(&db, "wal-wait", false);
    db.execute(
        "UPDATE thread_index_state SET wal_wait_since_ns = 1 WHERE id = 1",
        [],
    )
    .unwrap();
    let mut host = open(&dir).await;
    let handle = writer(&host);
    terminal(
        host.composition.storage.db_path.clone(),
        "wal-wait",
        handle.observe_thread_index_for_test().unwrap(),
    )
    .await;
    assert!(handle.thread_index_job_status().unwrap().wal_wait.is_none());
    host.shutdown().await.unwrap();
}

#[test]
fn resident_capacity_paused_owner_settles_registration_without_stepping() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = storage::StorageConfig {
        db_path: dir.path().join("paused.db"),
        ..Default::default()
    };
    let initial = Storage::open(config.clone()).unwrap();
    initial
        .handle()
        .save_snapshot_bundle(&snapshot("paused-target"))
        .unwrap();
    Connection::open(&config.db_path).unwrap().execute_batch("INSERT OR IGNORE INTO thread_index_runs(run_id, state, source_revision, snapshot_revision, source_context_sha256, extractor_version, accounting_version, source_updated_at_ns, retention_at_ns) SELECT run_id, 'pending', snapshot_revision, snapshot_revision, '', 1, 1, updated_at_ns, 1 FROM run_contexts WHERE run_id = 'paused-target'").unwrap();
    config.hard_limits.max_db_bytes = initial
        .handle()
        .thread_index_capacity()
        .unwrap()
        .reserve_bytes;
    initial.close();
    let storage = Storage::open(config).unwrap();
    let handle = storage.handle();
    let mut owner = super::ThreadIndexOwner::start(handle.clone()).unwrap();
    let lease = handle
        .thread_index_job_status()
        .unwrap()
        .owner_lease
        .unwrap();
    let submitted = handle
        .submit_thread_index_request(
            &ReindexRequest {
                include_evicted: true,
                ..all()
            },
            &other(),
            lease.expires_at_ns - 1,
            S,
        )
        .unwrap();
    assert!(matches!(submitted.lease, LeaseResult::HeldByOther(_)));
    let RequestProgress::Terminal(ReindexResult::CompletedWithGaps { summary, .. }) =
        submitted.progress
    else {
        panic!(
            "capacity admission did not settle: {:?}",
            submitted.progress
        );
    };
    assert_eq!(summary.target_count, 1);
    assert_eq!(
        summary.gaps,
        vec![RequestGapCount {
            reason: RequestGapReason::CapacityPaused,
            count: 1
        }]
    );
    owner.stop().unwrap();
    storage.close();
}
