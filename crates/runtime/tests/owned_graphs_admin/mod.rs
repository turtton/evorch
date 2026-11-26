use super::fixture::{self, Fixture};
pub mod shell;
use runtime::{
    RunId,
    ownership::{OwnerState, ThreadOwner},
};
use rusqlite::{Connection, params};
use storage::recovery::{graph_control::GraphEffectKey, graph_progress::GraphDisposalStep};

fn effect(id: &str) -> GraphEffectKey {
    GraphEffectKey {
        operation_id: id.into(),
        attempt_nonce: format!("nonce-{id}"),
    }
}
fn identity(row: &ThreadOwner) -> (&str, u64, OwnerState) {
    (&row.lease.owner_id, row.lease.generation, row.state)
}
fn save(f: &Fixture, row: &ThreadOwner) {
    Connection::open(f.root.join("owners/owners.db"))
        .unwrap()
        .execute(
            "UPDATE thread_owners SET state=?2 WHERE thread_id=?1",
            params![row.thread_id, serde_json::to_string(row).unwrap()],
        )
        .unwrap();
}
fn stored_bindings(f: &Fixture) -> Vec<(String, String)> {
    Connection::open(&f.config.db_path).unwrap().prepare(
        "SELECT thread_id, root_run_id FROM threads WHERE root_run_id IS NOT NULL UNION SELECT thread_id, root_run_id FROM thread_runs ORDER BY 1,2"
    ).unwrap().query_map([], |row| Ok((row.get(0)?, row.get(1)?))).unwrap().collect::<Result<_, _>>().unwrap()
}

fn dispose(f: &Fixture, id: &str) -> runtime::runtime::recovery_disposal::DisposalResult {
    let cas = f.cas("a");
    let reservation = f
        .runtime
        .reserve_owned_graph_disposal("graph-a", effect(id), cas.control_revision)
        .unwrap();
    let authority = f.runtime.disposal_authority(cas, f.owners("a")).unwrap();
    let before = stored_bindings(f);
    let result = f
        .runtime
        .dispose_owned_graph(&authority, effect(id), &reservation)
        .unwrap();
    assert_eq!(
        stored_bindings(f),
        before,
        "disposal did not publish bindings"
    );
    result
}
pub fn check(f: &Fixture, case: &str) {
    f.retain();
    let bindings_before = stored_bindings(f);
    let original = f.owner.attach("a").unwrap();
    let sibling = f.owner.attach("b").unwrap();
    let scratch = f.owner.thread_scratch();
    if case != "release-unknown" {
        f.runtime.set_thread_scratch(scratch.clone());
    }
    let dir = scratch.thread_dir("a");
    use sha2::Digest;
    use std::os::unix::ffi::OsStrExt;
    let canonical = f.root.join("owners").canonicalize().unwrap();
    let digest = format!(
        "{:x}",
        sha2::Sha256::digest(canonical.as_os_str().as_bytes())
    );
    let disk = sandbox::ScratchRoot::fixed(&digest[..8], &digest).unwrap();
    disk.prepare(dir.file_name().unwrap().to_str().unwrap())
        .unwrap();
    std::fs::write(dir.join("before"), "before").unwrap();
    // An unrelated bound root must not poison cleanup of this graph.
    f.runtime.bind_thread_root("b", RunId::new(91)).unwrap();
    match case {
        "release-bound-root"
        | "release-other-graph"
        | "release-executing"
        | "release-waiting"
        | "release-paused"
        | "release-shell-teardown" => {
            let live = if matches!(case, "release-executing" | "release-waiting") {
                Some(LiveRun::start(f, case == "release-waiting", false))
            } else {
                None
            };
            let paused = if case == "release-paused" {
                let root = f.root.join("owners");
                let leases =
                    storage::shared_with_wait(&f.config.db_path, &root, std::time::Duration::ZERO)
                        .unwrap();
                let startup = leases
                    .recovery_startup_complete(&f.config.db_path, &root)
                    .unwrap();
                let plan = f.runtime.recovery_plan().unwrap();
                let component = plan
                    .components
                    .iter()
                    .find(|c| c.manifest_id == "graph-b")
                    .unwrap_or_else(|| panic!("paused candidate: {:?}", plan.report));
                f.runtime
                    .bind_thread_root("a", fixture::run_id("b"))
                    .unwrap();
                Some((
                    f.runtime
                        .prepare_component(component, startup, &root)
                        .unwrap(),
                    leases,
                ))
            } else {
                None
            };
            if case == "release-other-graph" {
                let mut expected = f.tx().expected;
                let member = expected
                    .iter_mut()
                    .find(|m| m.run_id == fixture::run_id("b").to_string())
                    .unwrap();
                member.thread_id = Some("a".into());
                // Another graph of the same tx now references the shared thread.
                // Target graph membership and retained authority remain unchanged.
                Connection::open(&f.config.db_path)
                    .unwrap()
                    .execute(
                        "UPDATE restart_expected_members SET member_json=?1 WHERE run_id=?2",
                        params![serde_json::to_string(&member).unwrap(), member.run_id],
                    )
                    .unwrap();
                let digest = storage::recovery::restart::restart_digest(&expected).unwrap();
                Connection::open(&f.config.db_path)
                    .unwrap()
                    .execute(
                        "UPDATE restart_transactions SET expected_digest=?1 WHERE restart_id=?2",
                        params![digest, fixture::TX],
                    )
                    .unwrap();
            } else if case == "release-shell-teardown" {
                LiveRun::start(f, false, true).finish();
                // Replace the now-terminal test run root with the captured root.
                // No live execution or unrelated binding masks shell-only use.
                f.runtime
                    .bind_thread_root("a", fixture::run_id("a"))
                    .unwrap();
                // The live test run also left durable binding history. Remove
                // only that terminal fixture run before the disposal baseline,
                // otherwise a bound root would mask the shell-only user.
                let conn = Connection::open(&f.config.db_path).unwrap();
                let captured = fixture::run_id("a").to_string();
                conn.execute(
                    "UPDATE threads SET root_run_id=?1 WHERE thread_id='a'",
                    [&captured],
                )
                .unwrap();
                conn.execute(
                    "DELETE FROM thread_runs WHERE thread_id='a' AND root_run_id != ?1",
                    [&captured],
                )
                .unwrap();
                assert!(
                    !f.db()
                        .owned_graph_shared_threads(&f.cas("a"))
                        .unwrap()
                        .contains("a")
                );
            } else if live.is_none() && paused.is_none() {
                f.runtime.bind_thread_root("a", RunId::new(90)).unwrap();
            }
            let result = dispose(f, case);
            assert!(result.complete(), "{result:?}");
            assert_eq!(result.threads[0].owners, GraphDisposalStep::NotApplicable);
            assert_eq!(result.threads[0].scratch, GraphDisposalStep::NotApplicable);
            assert_eq!(std::fs::read(dir.join("before")).unwrap(), b"before");
            assert_eq!(
                f.owner.attach("a").unwrap().planned_claim,
                original.planned_claim
            );
            if let Some(live) = live {
                live.finish();
            }
        }
        "release-unused"
        | "release-prepared"
        | "release-completed-absent"
        | "release-completed-released"
        | "release-adopted-released" => {
            let released = matches!(
                case,
                "release-completed-released" | "release-adopted-released"
            );
            let cleanup_only = released || case == "release-completed-absent";
            let _prepared = if case == "release-prepared" || released {
                let root = f.root.join("owners");
                let leases =
                    storage::shared_with_wait(&f.config.db_path, &root, std::time::Duration::ZERO)
                        .unwrap();
                let startup = leases
                    .recovery_startup_complete(&f.config.db_path, &root)
                    .unwrap();
                let plan = f.runtime.recovery_plan().unwrap();
                let component = plan
                    .components
                    .iter()
                    .find(|c| c.manifest_id == "graph-a")
                    .unwrap();
                Some((
                    f.runtime
                        .prepare_component(component, startup, &root)
                        .unwrap(),
                    leases,
                ))
            } else {
                None
            };
            if case == "release-prepared" {
                assert!(matches!(
                    f.cas("a").claim.unwrap().state,
                    storage::recovery::RecoveryClaimState::Prepared
                ));
            }
            if released {
                f.storage
                    .handle()
                    .release_prepared_claim_if_unexecuted(
                        f.cas("a").claim.unwrap().claim_id,
                        "ended before cleanup".into(),
                    )
                    .unwrap();
            }
            if case.starts_with("release-completed-") {
                Connection::open(&f.config.db_path)
                    .unwrap()
                    .execute("UPDATE restart_transactions SET phase='completed'", [])
                    .unwrap();
            }
            let ended_before = f.cas("a");
            if cleanup_only {
                assert!(f.db().recovery_graph_control("graph-a").unwrap().is_none());
            }
            let result = dispose(f, case);
            assert!(result.complete(), "{result:?}");
            if cleanup_only {
                let after = f.cas("a");
                assert_eq!(after.phase, ended_before.phase);
                assert_eq!(after.claim, ended_before.claim);
                assert_eq!(after.component, ended_before.component);
            }
            if case == "release-prepared" {
                assert!(matches!(
                    f.cas("a").claim.unwrap().state,
                    storage::recovery::RecoveryClaimState::Released { .. }
                ));
            }
            assert_eq!(result.threads[0].scratch, GraphDisposalStep::Applied);
            assert!(!dir.join("before").exists());
            assert!(f.owner.attach("a").unwrap().planned_claim.is_none());
        }
        "release-unknown" => {
            let result = dispose(f, case);
            assert!(!result.complete());
            assert_eq!(result.threads[0].scratch, GraphDisposalStep::Unknown);
            assert_eq!(std::fs::read(dir.join("before")).unwrap(), b"before");
        }
        "release-owner-conflict" => {
            let cas = f.cas("a");
            let authority = f
                .runtime
                .disposal_authority(cas.clone(), f.owners("a"))
                .unwrap();
            let reservation = f
                .runtime
                .reserve_owned_graph_disposal("graph-a", effect(case), 0)
                .unwrap();
            let mut changed = original.clone();
            changed.lease.generation += 1;
            save(f, &changed);
            let before = f.tx();
            let result = f
                .runtime
                .dispose_owned_graph(&authority, effect(case), &reservation)
                .unwrap_err();
            assert_eq!(result, runtime::RuntimeError::GraphControlConflict);
            drop(reservation);
            let _retry = f
                .runtime
                .reserve_owned_graph_disposal("graph-a", effect("after-conflict"), 0)
                .expect("a confirmed refusal does not strand admission");
            assert_eq!(f.tx(), before);
            assert!(f.db().recovery_graph_control("graph-a").unwrap().is_none());
            assert!(f.owner.disposal_cleanup_receipt(case).unwrap().is_none());
            assert_eq!(identity(&f.owner.attach("a").unwrap()), identity(&changed));
            assert_eq!(std::fs::read(dir.join("before")).unwrap(), b"before");
            save(f, &original);
        }
        "release-reset-ack-lost" => {
            // Fail durable progress AFTER the real namespace reset. The missing
            // file independently proves the irreversible effect was applied.
            let conn = Connection::open(&f.config.db_path).unwrap();
            conn.execute_batch("CREATE TRIGGER fail_disposal_progress BEFORE INSERT ON recovery_graph_thread_progress BEGIN SELECT RAISE(FAIL,'injected progress failure'); END;").unwrap();
            let cas = f.cas("a");
            let authority = f
                .runtime
                .disposal_authority(cas.clone(), f.owners("a"))
                .unwrap();
            let reservation = f
                .runtime
                .reserve_owned_graph_disposal("graph-a", effect(case), 0)
                .unwrap();
            let result = f
                .runtime
                .dispose_owned_graph(&authority, effect(case), &reservation)
                .unwrap();
            assert!(result.unknown);
            assert!(!result.complete());
            assert!(!dir.join("before").exists());
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("after"), "new scratch").unwrap();
            conn.execute_batch("DROP TRIGGER fail_disposal_progress")
                .unwrap();
            // Original supervisor readback settles policy, not cleanup.
            assert!(
                f.runtime
                    .reconcile_owned_graph_disposal(&effect(case), &reservation)
                    .unwrap()
            );
            assert_eq!(std::fs::read(dir.join("after")).unwrap(), b"new scratch");
            let observed = f.owner.disposal_cleanup_receipt(case).unwrap().unwrap();
            assert_eq!(observed.effect, effect(case));
            drop(reservation);
            let retried = dispose(f, "release-new-id");
            assert!(retried.complete(), "{retried:?}");
            assert_eq!(std::fs::read(dir.join("after")).unwrap(), b"new scratch");
            assert!(dispose(f, "release-third-id").complete());
            assert_eq!(std::fs::read(dir.join("after")).unwrap(), b"new scratch");
        }
        _ => unreachable!(),
    }
    assert_eq!(
        f.runtime.list_agents().len(),
        usize::from(matches!(
            case,
            "release-executing" | "release-waiting" | "release-shell-teardown"
        )),
        "disposal registered no execution"
    );
    // Independent readbacks. Fixture provider panics on any request.
    assert_eq!(identity(&f.owner.attach("a").unwrap()), identity(&original));
    assert_eq!(identity(&f.owner.attach("b").unwrap()), identity(&sibling));
    assert_eq!(
        f.owner.attach("b").unwrap().planned_claim,
        sibling.planned_claim
    );
    assert!(
        f.db()
            .recovery_claims()
            .unwrap()
            .iter()
            .all(|c| c.state != storage::recovery::RecoveryClaimState::Activated)
    );
    assert!(
        f.tx()
            .components
            .iter()
            .all(|c| c.disposition != storage::recovery::restart::RestartDisposition::Activated)
    );
    assert!(f.runtime.list_agents().iter().all(|a| !matches!(
        a.phase,
        event_bus::AgentRunPhase::Running
            | event_bus::AgentRunPhase::Waiting
            | event_bus::AgentRunPhase::Pending
    )));
    assert_eq!(f.runtime.has_paused_recovery(), case == "release-paused");
    if !matches!(
        case,
        "release-executing" | "release-waiting" | "release-shell-teardown"
    ) {
        assert_eq!(stored_bindings(f), bindings_before);
    }
}

struct LiveModel {
    waiting: bool,
    shell_teardown: bool,
    entered: std::sync::mpsc::Sender<usize>,
    calls: std::sync::atomic::AtomicUsize,
}
#[async_trait::async_trait]
impl runtime::AgentModel for LiveModel {
    fn selected_model(&self, _: runtime::Role, _: Option<&str>) -> String {
        "live".into()
    }
    async fn complete(
        &self,
        _: &runtime::AgentInvocationContext,
        _: runtime::Role,
        _: &[providers::Message],
        _: &[providers::ToolSpec],
    ) -> Result<providers::ChatResponse, runtime::RuntimeError> {
        let call = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
        if self.shell_teardown && call == 1 {
            return Ok(shell::request());
        }
        if !self.waiting {
            self.entered.send(call).unwrap();
            return std::future::pending().await;
        }
        Ok(providers::ChatResponse {
            message: providers::Message {
                role: providers::Role::Assistant,
                content: vec![],
            },
            usage: Default::default(),
            usage_reported: true,
            finish_reason: providers::FinishReason::Stop,
        })
    }
}
struct LiveRun {
    cancel: tokio::sync::oneshot::Sender<()>,
    worker: std::thread::JoinHandle<()>,
    model: std::sync::Arc<LiveModel>,
    baseline: usize,
}
impl LiveRun {
    fn start(f: &Fixture, waiting: bool, shell_teardown: bool) -> Self {
        let (ready, entered) = std::sync::mpsc::channel();
        let model = std::sync::Arc::new(LiveModel {
            waiting,
            shell_teardown,
            entered: ready.clone(),
            calls: Default::default(),
        });
        let selected = model.clone();
        let runtime = f
            .runtime
            .clone()
            .with_project_models(std::sync::Arc::new(move |_| Some(selected.clone())));
        let project = f.root.join("project");
        let runtime = if shell_teardown {
            std::fs::create_dir_all(&project).unwrap();
            runtime.with_sandbox_root(project.clone())
        } else {
            runtime
        };
        let worker_model = model.clone();
        let (cancel, mut cancelled) = tokio::sync::oneshot::channel();
        let worker = std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
                let run = runtime.reserve_run_id();
                runtime.bind_thread_root("a", run).unwrap();
                let mut phase = runtime.event_bus().subscribe();
                runtime.spawn_reserved(run, None, runtime::Role::Worker, "live sharing", runtime::RunConfig {
                    interactive: true, keep_alive: true, project_root: Some(project), ..Default::default()
                }).unwrap();
                if waiting {
                    loop {
                        let event = phase.recv().await.unwrap();
                        if matches!(event.kind, event_bus::EventKind::Lifecycle(event_bus::LifecycleEvent::AgentRunStateChanged { run_id, to: event_bus::AgentRunPhase::Waiting, .. }) if run_id == run.to_string()) { break; }
                    }
                    ready.send(1).unwrap();
                }
                if shell_teardown {
                    loop {
                        tokio::select! {
                            _ = &mut cancelled => break,
                            event = phase.recv() => {
                                let event = event.unwrap();
                                if matches!(event.kind, event_bus::EventKind::Lifecycle(event_bus::LifecycleEvent::AgentRunStateChanged { to: event_bus::AgentRunPhase::Error | event_bus::AgentRunPhase::Stopped | event_bus::AgentRunPhase::Done, .. })) {
                                    ready.send(worker_model.calls.load(std::sync::atomic::Ordering::SeqCst)).unwrap();
                                    cancelled.await.unwrap();
                                    break;
                                }
                            }
                        }
                    }
                    let _ = runtime.cancel(run);
                    // Unknown teardown may make normal finalization fail closed.
                    let _ = runtime.wait(run).await;
                } else {
                    cancelled.await.unwrap();
                    runtime.cancel(run).unwrap();
                    runtime.wait(run).await.unwrap();
                }
            });
        });
        let baseline = entered.recv().unwrap();
        assert_eq!(
            model.calls.load(std::sync::atomic::Ordering::SeqCst),
            baseline
        );
        Self {
            cancel,
            worker,
            model,
            baseline,
        }
    }
    fn finish(self) {
        assert_eq!(
            self.model.calls.load(std::sync::atomic::Ordering::SeqCst),
            self.baseline,
            "disposal made no provider request"
        );
        self.cancel.send(()).unwrap();
        self.worker.join().unwrap();
    }
}
