use super::*;
use workbench_host::bootstrap::{BootstrapOptions, TestSeams};
#[derive(Default)]
struct Model(std::sync::atomic::AtomicUsize);
#[async_trait::async_trait]
impl runtime::AgentModel for Model {
    fn selected_model(&self, _: runtime::Role, _: Option<&str>) -> String {
        "restart-fixture".into()
    }
    fn requires_admission(&self) -> bool {
        false
    }
    async fn admit(
        &self,
        _: &runtime::AgentInvocationContext,
        _: runtime::Role,
    ) -> Result<(), RuntimeError> {
        Ok(())
    }
    async fn complete(
        &self,
        _: &runtime::AgentInvocationContext,
        _: runtime::Role,
        _: &[providers::Message],
        _: &[providers::ToolSpec],
    ) -> Result<providers::ChatResponse, RuntimeError> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(providers::ChatResponse {
            message: providers::Message {
                role: providers::Role::Assistant,
                content: vec![providers::ContentBlock::Text {
                    text: "completed".into(),
                }],
            },
            usage: Default::default(),
            usage_reported: true,
            finish_reason: providers::FinishReason::Stop,
        })
    }
}
struct Direct;
impl runtime::SandboxFactory for Direct {
    fn build(
        &self,
        _: &runtime::ExecutionPolicy,
        _: &runtime::WorkspaceMounts,
    ) -> Result<Arc<dyn sandbox::Sandbox>, sandbox::SandboxError> {
        Ok(Arc::new(sandbox::DirectSandbox::new_unchecked()))
    }
}
async fn boot() -> (tempfile::TempDir, BootstrappedHost) {
    boot_with_model(Arc::new(Model::default())).await
}
async fn boot_with_model(
    model: Arc<dyn runtime::AgentModel>,
) -> (tempfile::TempDir, BootstrappedHost) {
    let dir = tempfile::tempdir_in("/tmp").unwrap();
    let root = dir.path().canonicalize().unwrap();
    let mut sidebar = workspace_state::SidebarState::default();
    sidebar
        .add_project(workspace_state::ProjectId::new("p"), "repo", &root)
        .unwrap();
    std::fs::write(
        root.join("sidebar.json"),
        serde_json::to_vec(&sidebar).unwrap(),
    )
    .unwrap();
    let boot = BootstrappedHost::open_with_test_seams(
        BootstrapOptions {
            config_dir: root.clone(),
            project_root: root.clone(),
            ownership_root: root.join("o"),
        },
        TestSeams {
            model,
            sandbox: Arc::new(Direct),
        },
    )
    .await
    .unwrap();
    assert_owner_socket_paths(&root.join("o"));
    (dir, boot)
}
fn assert_owner_socket_paths(root: &std::path::Path) {
    let sockets: Vec<_> = std::fs::read_dir(root)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "sock")
        })
        .collect();
    assert!(!sockets.is_empty());
    assert!(
        sockets
            .iter()
            .all(|path| path.as_os_str().as_encoded_bytes().len() < 108)
    );
}
async fn phase(core: &RestartCoordinator, expected: RestartPhase) {
    let mut changes = core.subscribe();
    loop {
        if core.snapshot().phase == expected {
            return;
        }
        changes.changed().await.unwrap();
    }
}
#[tokio::test]
async fn shared_request_survives_waiter_drop_and_stale_cancel_cannot_reopen() {
    let (_dir, mut host) = boot().await;
    let core = RestartCoordinator::new(&host);
    let producer = host.runtime.producer_gate();
    let accepted = producer.try_enter().unwrap();
    let first = core.request().unwrap();
    assert_eq!(core.request().unwrap(), first);
    assert_eq!(RestartCoordinator::new(&host).request().unwrap(), first);
    phase(&core, RestartPhase::WaitingSafePoint).await;
    let mut waiter = Box::pin(core.wait_sealed(first));
    assert!(
        std::future::poll_fn(|cx| std::task::Poll::Ready(waiter.as_mut().poll(cx)))
            .await
            .is_pending()
    );
    drop(waiter);
    drop(accepted);
    let proof = core.wait_sealed(first).await.unwrap();
    proof.validate(&host.runtime).unwrap();
    assert!(host.busy());
    core.cancel(first).unwrap();
    assert!(proof.validate(&host.runtime).is_err());
    assert!(producer.try_enter().is_some());
    let second = core.request().unwrap();
    assert_ne!(first, second);
    assert!(core.cancel(first).is_err());
    assert!(producer.try_enter().is_none());
    core.wait_sealed(second).await.unwrap();
    core.cancel(second).unwrap();
    host.shutdown().await.unwrap();
}
#[tokio::test]
async fn shutdown_closes_pending_producers_without_resuming_restart() {
    let (_dir, mut host) = boot().await;
    let core = RestartCoordinator::new(&host);
    let producer = host.runtime.producer_gate();
    let accepted = producer.try_enter().unwrap();
    let id = core.request().unwrap();
    phase(&core, RestartPhase::WaitingSafePoint).await;
    core.transition_to_shutdown();
    drop(accepted);
    assert!(core.wait_sealed(id).await.is_err());
    assert!(core.cancel(id).is_err());
    assert!(producer.enter().await.is_none());
    assert_eq!(core.snapshot().phase, RestartPhase::ShuttingDown);
    host.shutdown().await.unwrap();
}
#[tokio::test]
async fn sealed_ingress_rejects_before_new_thread_reservation_and_requires_explicit_retry() {
    use workbench_protocol::{
        commands::{ComposerRole, SendChat},
        ids::{ProjectId, SubmissionId, ThreadId},
    };
    let (_dir, mut host) = boot().await;
    let core = RestartCoordinator::new(&host);
    let send = SendChat {
        project: ProjectId::new("p").unwrap(),
        thread: ThreadId::new("rejected").unwrap(),
        submission_id: SubmissionId::new("test").unwrap(),
        composer_role: ComposerRole::Worker,
        text: "refused".into(),
        skills: vec![],
        images: vec![],
        model_preference: None,
        fork_seed: None,
    };
    let id = core.request().unwrap();
    core.wait_sealed(id).await.unwrap();
    assert_eq!(
        host.host
            .lock()
            .unwrap()
            .reserve_new_chat_thread(&send)
            .unwrap_err()
            .code,
        workbench_protocol::errors::ErrorCode::Busy
    );
    assert!(
        host.runtime
            .with_storage_reader(|db| db.thread("rejected"))
            .unwrap()
            .unwrap()
            .is_none()
    );
    core.cancel(id).unwrap();
    assert!(
        host.runtime
            .with_storage_reader(|db| db.thread("rejected"))
            .unwrap()
            .unwrap()
            .is_none()
    );
    host.host
        .lock()
        .unwrap()
        .reserve_new_chat_thread(&send)
        .unwrap();
    assert!(
        host.runtime
            .with_storage_reader(|db| db.thread("rejected"))
            .unwrap()
            .unwrap()
            .is_some()
    );
    host.shutdown().await.unwrap();
}
#[test]
fn dto_bounds_blockers_and_omits_internal_incarnations() {
    let snapshot = RestartSnapshot {
        id: None,
        phase: RestartPhase::FailedClosed,
        revision: 9,
        active_producers: 0,
        blockers: (0..70)
            .map(|_| runtime::quiesce::RestartBlocker {
                run: runtime::RunId::new(1),
                generation: 1,
                incarnation: "private-authority".into(),
                kind: runtime::quiesce::RestartBlockerKind::ShellRunning,
            })
            .collect(),
    };
    let dto = snapshot.to_dto();
    dto.validate().unwrap();
    assert_eq!(dto.blockers.len(), 64);
    assert_eq!((dto.blocker_total, dto.blocker_omitted), (70, 6));
    let wire = serde_json::to_string(&dto).unwrap();
    assert!(!wire.contains("private-authority"));
    assert!(!wire.contains("incarnation"));
}

#[tokio::test]
async fn preparing_stop_joins_same_attempt_without_reopening_producers() {
    let (_dir, mut host) = boot().await;
    let core = RestartCoordinator::new(&host);
    let gate = host.runtime.producer_gate();
    let producer = gate.try_enter().unwrap();
    let run = host.runtime.delegate_background(
        runtime::Role::Worker,
        "never call provider".into(),
        runtime::RunConfig {
            keep_alive: true,
            ..Default::default()
        },
    );
    let id = core.request().unwrap();
    // Stop an actually parked LoopState, not a never-polled finalizer fixture.
    host.runtime.wait_quiesced().await.unwrap();
    core.stop(run, runtime::StopScope::SelfOnly).unwrap();
    host.runtime.wait(run).await.unwrap();
    assert_eq!(id, core.request().unwrap());
    assert!(gate.try_enter().is_none());
    drop(producer);
    core.wait_sealed(id).await.unwrap_or_else(|error| {
        panic!(
            "{error:?}; {:?}; {:?}",
            core.snapshot(),
            host.runtime.quiesce_state()
        )
    });
    core.cancel(id).unwrap();
    host.shutdown().await.unwrap();
}
#[tokio::test]
async fn checkpoint_write_failure_remains_failed_closed_even_after_stop() {
    let (_dir, mut host) = boot().await;
    let core = RestartCoordinator::new(&host);
    let run = host.runtime.delegate_background(
        runtime::Role::Worker,
        "checkpoint must fail".into(),
        runtime::RunConfig {
            keep_alive: true,
            ..Default::default()
        },
    );
    let db = rusqlite::Connection::open(&host.composition.storage.db_path).unwrap();
    db.execute_batch("CREATE TRIGGER reject_restart_update BEFORE UPDATE ON resume_intents BEGIN SELECT RAISE(ABORT, 'checkpoint failure'); END; CREATE TRIGGER reject_restart_insert BEFORE INSERT ON resume_intents BEGIN SELECT RAISE(ABORT, 'checkpoint failure'); END;").unwrap();
    let id = core.request().unwrap();
    assert!(core.wait_sealed(id).await.is_err());
    assert_eq!(core.snapshot().phase, RestartPhase::FailedClosed);
    assert!(core.cancel(id).is_err());
    assert!(host.runtime.producer_gate().try_enter().is_none());
    db.execute_batch("DROP TRIGGER reject_restart_update; DROP TRIGGER reject_restart_insert;")
        .unwrap();
    core.stop(run, runtime::StopScope::SelfOnly).unwrap();
    host.runtime.wait(run).await.unwrap();
    assert_eq!(core.snapshot().phase, RestartPhase::FailedClosed);
    assert!(host.runtime.producer_gate().try_enter().is_none());
    assert!(host.shutdown().await.is_err());
}

#[tokio::test]
async fn storage_failure_before_request_rejects_only_restart_not_host_work() {
    let (_dir, mut host) = boot().await;
    let db = rusqlite::Connection::open(&host.composition.storage.db_path).unwrap();
    let run = host.runtime.delegate_background(
        runtime::Role::Worker,
        "unavailable fixture provider".into(),
        runtime::RunConfig::default(),
    );
    // Registration has committed. Only terminal intent retirement now fails, outside an attempt.
    db.execute_batch("CREATE TRIGGER reject_terminal_insert BEFORE INSERT ON resume_intents BEGIN SELECT RAISE(ABORT, 'terminal intent failure'); END; CREATE TRIGGER reject_terminal_update BEFORE UPDATE ON resume_intents BEGIN SELECT RAISE(ABORT, 'terminal intent failure'); END;").unwrap();
    host.runtime.wait(run).await.unwrap();
    assert!(matches!(
        host.runtime.quiesce_state(),
        runtime::quiesce::QuiesceState::Failed { .. }
    ));
    let core = RestartCoordinator::new(&host);
    assert!(core.request().is_err());
    assert_eq!(core.snapshot().phase, RestartPhase::Running);
    assert!(host.runtime.producer_gate().try_enter().is_some());
    assert!(
        host.host
            .lock()
            .unwrap()
            .service()
            .restart_gate()
            .enter(workbench_host::bootstrap::restart::CommandAccess::NewWork)
            .is_ok()
    );
    db.execute_batch("DROP TRIGGER reject_terminal_insert; DROP TRIGGER reject_terminal_update;")
        .unwrap();
    assert!(host.shutdown().await.is_err());
}

#[tokio::test]
async fn sealing_stop_during_host_permit_drain_cancels_before_runtime_tail_drain() {
    let (_dir, mut host) = boot().await;
    let core = RestartCoordinator::new(&host);
    let gate = host.host.lock().unwrap().service().restart_gate();
    let input = gate
        .enter(workbench_host::bootstrap::restart::CommandAccess::ExistingInput)
        .unwrap();
    let (sent, mut received) = tokio::sync::mpsc::unbounded_channel();
    host.runtime
        .set_terminal_registered_probe(move |run, lease| {
            sent.send((run, lease)).ok();
        })
        .unwrap();
    let prior = host.runtime.delegate_background(
        runtime::Role::Worker,
        "prior terminal tail".into(),
        Default::default(),
    );
    host.runtime
        .stop(prior, runtime::StopScope::SelfOnly)
        .unwrap();
    let (registered, live_tail) = received.recv().await.unwrap();
    assert_eq!(registered, prior);
    let run = host.runtime.delegate_background(
        runtime::Role::Worker,
        "park before first provider call".into(),
        runtime::RunConfig::default(),
    );
    let id = core.request().unwrap();
    phase(&core, RestartPhase::Sealing).await;
    assert!(
        gate.enter(workbench_host::bootstrap::restart::CommandAccess::ExistingInput)
            .is_err()
    );
    core.stop(run, runtime::StopScope::SelfOnly).unwrap();
    assert_eq!(core.snapshot().phase, RestartPhase::Running);
    assert!(core.wait_sealed(id).await.is_err());
    drop(input);
    host.runtime.wait(run).await.unwrap();
    let next = core.request().unwrap();
    phase(&core, RestartPhase::Sealing).await;
    let mut sealed = Box::pin(core.wait_sealed(next));
    assert!(
        std::future::poll_fn(|cx| std::task::Poll::Ready(sealed.as_mut().poll(cx)))
            .await
            .is_pending()
    );
    // A live guard surviving cancellation must still account in the same epoch.
    drop(live_tail);
    // The target's own terminal tail has been queued by the probe as well.
    drop(received.recv().await.unwrap());
    sealed.await.unwrap();
    core.cancel(next).unwrap();
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn sealed_stop_retains_real_terminal_tail_until_next_attempt_drains_it() {
    let model = Arc::new(Model::default());
    let (_dir, mut host) = boot_with_model(model.clone()).await;
    let core = RestartCoordinator::new(&host);
    let target = host.runtime.delegate_background(
        runtime::Role::Orchestrator,
        "target".into(),
        runtime::RunConfig::default(),
    );
    let other = host.runtime.delegate_background(
        runtime::Role::Worker,
        "unaffected".into(),
        runtime::RunConfig::default(),
    );
    let a = core.request().unwrap();
    // This queued child has no provider admission; subtree Stop must never start it.
    let child = host
        .runtime
        .delegate_background_as_child(
            target,
            runtime::Role::Worker,
            "queued child",
            runtime::RunConfig::default(),
        )
        .unwrap();
    let proof = core.wait_sealed(a).await.unwrap();
    assert_eq!(model.0.load(std::sync::atomic::Ordering::SeqCst), 0);
    let (sent, mut received) = tokio::sync::mpsc::unbounded_channel();
    host.runtime
        .set_terminal_registered_probe(move |run, lease| {
            if run == target {
                sent.send(lease).ok();
            }
        })
        .unwrap();
    core.stop(target, runtime::StopScope::Subtree).unwrap();
    let tail = received.recv().await.unwrap();
    assert!(proof.validate(&host.runtime).is_err());
    assert_eq!(core.snapshot().phase, RestartPhase::Running);
    assert_eq!(
        host.runtime.wait(target).await.unwrap(),
        event_bus::AgentRunPhase::Stopped
    );
    assert!(matches!(
        host.runtime.wait(child).await,
        Err(RuntimeError::RunTerminated { .. })
    ));
    // Only the unaffected live future resumes and reaches its provider result.
    assert_eq!(
        host.runtime.wait(other).await.unwrap(),
        event_bus::AgentRunPhase::Done
    );
    assert_eq!(model.0.load(std::sync::atomic::Ordering::SeqCst), 1);
    // Duplicate Stop must neither create a new run nor discard the held tail.
    core.stop(target, runtime::StopScope::Subtree).unwrap();
    let b = core.request().unwrap();
    assert_ne!(a, b);
    phase(&core, RestartPhase::Sealing).await;
    let mut sealed = Box::pin(core.wait_sealed(b));
    assert!(
        std::future::poll_fn(|cx| std::task::Poll::Ready(sealed.as_mut().poll(cx).is_pending()))
            .await
    );
    // Not terminal phase/stop return, but release of the actual registered guard permits sealing.
    drop(tail);
    sealed.await.unwrap();
    core.cancel(b).unwrap();
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn host_seal_failure_after_marker_keeps_stop_terminal_only() {
    let model = Arc::new(Model::default());
    let (_dir, mut host) = boot_with_model(model.clone()).await;
    let core = RestartCoordinator::new(&host);
    let gate = host.host.lock().unwrap().service().restart_gate();
    let accepted = gate
        .enter(workbench_host::bootstrap::restart::CommandAccess::ExistingInput)
        .unwrap();
    let target = host.runtime.delegate_background(
        runtime::Role::Worker,
        "stop target".into(),
        Default::default(),
    );
    let other = host.runtime.delegate_background(
        runtime::Role::Worker,
        "must stay parked".into(),
        Default::default(),
    );
    let id = core.request().unwrap();
    phase(&core, RestartPhase::Sealing).await;
    // Inject a host-side sealing refusal after the synchronous runtime marker.
    // No runtime failure exists yet: drive must propagate this refusal to runtime.
    assert!(gate.transition(id.generation(), HostRestartPhase::FailedClosed));
    drop(accepted);
    phase(&core, RestartPhase::FailedClosed).await;
    let failure = host.runtime.quiesce_state();
    assert!(matches!(
        failure,
        runtime::quiesce::QuiesceState::Failed { .. }
    ));
    assert!(core.cancel(id).is_err());
    core.stop(target, runtime::StopScope::SelfOnly).unwrap();
    assert_eq!(
        host.runtime.wait(target).await.unwrap(),
        event_bus::AgentRunPhase::Stopped
    );
    assert_eq!(host.runtime.quiesce_state(), failure);
    assert_eq!(core.snapshot().phase, RestartPhase::FailedClosed);
    let mut waiting = Box::pin(host.runtime.wait(other));
    assert!(
        std::future::poll_fn(|cx| std::task::Poll::Ready(waiting.as_mut().poll(cx)))
            .await
            .is_pending()
    );
    drop(waiting);
    assert_eq!(model.0.load(std::sync::atomic::Ordering::SeqCst), 0);
    core.transition_to_shutdown();
    assert!(host.shutdown().await.is_err());
    assert_eq!(model.0.load(std::sync::atomic::Ordering::SeqCst), 0);
}

#[tokio::test]
async fn shutdown_during_host_permit_drain_never_reopens_or_calls_provider() {
    let model = Arc::new(Model::default());
    let (_dir, mut host) = boot_with_model(model.clone()).await;
    let core = RestartCoordinator::new(&host);
    let gate = host.host.lock().unwrap().service().restart_gate();
    let accepted = gate
        .enter(workbench_host::bootstrap::restart::CommandAccess::ExistingInput)
        .unwrap();
    let run = host.runtime.delegate_background(
        runtime::Role::Worker,
        "parked".into(),
        Default::default(),
    );
    let id = core.request().unwrap();
    phase(&core, RestartPhase::Sealing).await;
    core.transition_to_shutdown();
    assert_eq!(gate.phase(), HostRestartPhase::ShuttingDown);
    drop(accepted);
    assert!(core.wait_sealed(id).await.is_err());
    host.shutdown().await.unwrap();
    assert_eq!(
        host.runtime.wait(run).await.unwrap(),
        event_bus::AgentRunPhase::Stopped
    );
    assert_eq!(model.0.load(std::sync::atomic::Ordering::SeqCst), 0);
}

#[derive(Default)]
struct CompactionBarrier {
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}
#[async_trait::async_trait]
impl runtime::AgentModel for CompactionBarrier {
    fn selected_model(&self, _: runtime::Role, _: Option<&str>) -> String {
        "compaction-barrier".into()
    }
    async fn complete(
        &self,
        invocation: &runtime::AgentInvocationContext,
        _: runtime::Role,
        _: &[providers::Message],
        _: &[providers::ToolSpec],
    ) -> Result<providers::ChatResponse, RuntimeError> {
        if invocation.purpose == event_bus::RequestPurpose::Compaction {
            self.entered.notify_one();
            self.release.notified().await;
        }
        Ok(providers::ChatResponse {
            message: providers::Message {
                role: providers::Role::Assistant,
                content: vec![providers::ContentBlock::Text {
                    text: "saved older analysis ".repeat(30),
                }],
            },
            usage: Default::default(),
            usage_reported: true,
            finish_reason: providers::FinishReason::Stop,
        })
    }
}
#[tokio::test]
async fn accepted_saved_compaction_retains_its_ingress_permit_until_persisted() {
    for protocol in [false, true] {
        let dir = tempfile::tempdir_in("/tmp").unwrap();
        let config = storage::StorageConfig {
            db_path: dir.path().join("saved.db"),
            ..Default::default()
        };
        let store = storage::Storage::open(config.clone()).unwrap();
        let model = Arc::new(CompactionBarrier::default());
        let bus = Arc::new(event_bus::EventBus::new(128));
        let mut events = bus.subscribe();
        let runtime = AgentRuntime::new(
            bus.clone(),
            Arc::new(tools::ToolExecutor::new(bus)),
            model.clone(),
        )
        .with_run_store(runtime::RunStore::open(&config, store.handle()).unwrap())
        .with_compaction(config::CompactionConfig {
            context_window_tokens: 1_000_000,
            keep_recent_tokens: 1,
            ..Default::default()
        });
        let run = runtime.delegate_background(
            runtime::Role::Worker,
            "older turn".into(),
            Default::default(),
        );
        runtime.wait(run).await.unwrap();
        runtime
            .continue_goal(run, "recent turn".into(), Default::default())
            .unwrap();
        loop {
            if matches!(events.recv().await.unwrap().kind, event_bus::EventKind::Lifecycle(
                event_bus::LifecycleEvent::AgentRunStateChanged { run_id, to: event_bus::AgentRunPhase::Waiting, .. }
            ) if run_id == run.to_string())
            {
                break;
            }
        }
        runtime.stop(run, StopScope::SelfOnly).unwrap();
        runtime.wait(run).await.unwrap();
        let mut host =
            workbench_host::CommandHost::new(workbench_host::commands::CommandService::new(
                runtime.clone(),
                tokio::runtime::Handle::current(),
            ));
        host.service_mut()
            .bind_goal_context("thread", "project", &run.to_string());
        let gate = host.service().restart_gate();
        let core = RestartCoordinator::from_parts(
            runtime.clone(),
            gate.clone(),
            vec![runtime.producer_gate()],
        );
        let provenance = workbench_host::provenance::local_desktop();
        if protocol {
            host.handle_command(
                workbench_protocol::commands::Command::Conversation(
                    workbench_protocol::namespaces::ConversationCommand::CompactChat(
                        workbench_protocol::commands::ThreadTarget {
                            thread: workbench_protocol::ids::ThreadId::new("thread").unwrap(),
                        },
                    ),
                ),
                provenance,
            )
            .unwrap();
        } else {
            let result = host.submit_local(
                workbench_host::commands::HostCommand::CompactChat {
                    thread_id: "thread".into(),
                },
                &provenance,
            );
            assert!(
                matches!(result.as_slice(), [workbench_host::commands::HostEvent::ChatNotice { text, .. }]
                if text.starts_with("Compacting saved"))
            );
        }
        model.entered.notified().await;
        let id = core.request().unwrap();
        phase(&core, RestartPhase::Sealing).await;
        let mut sealed = Box::pin(core.wait_sealed(id));
        assert!(
            std::future::poll_fn(|cx| std::task::Poll::Ready(
                sealed.as_mut().poll(cx).is_pending()
            ))
            .await
        );
        assert_eq!(gate.phase(), HostRestartPhase::Sealing);
        model.release.notify_one();
        let proof = sealed.await.unwrap();
        proof.validate(&runtime).unwrap();
        let result = host.service_mut().poll();
        assert!(
            matches!(result.as_slice(), [workbench_host::commands::HostEvent::ChatNotice { text, .. }]
            if text.contains("complete. Use /continue"))
        );
        core.cancel(id).unwrap();
    }
}
async fn direct_runtime_stop_reconciles(sealing: bool) {
    let model = Arc::new(Model::default());
    let (_dir, mut host) = boot_with_model(model.clone()).await;
    let core = RestartCoordinator::new(&host);
    let gate = host.host.lock().unwrap().service().restart_gate();
    let input = sealing.then(|| {
        gate.enter(workbench_host::bootstrap::restart::CommandAccess::ExistingInput)
            .unwrap()
    });
    let target = host.runtime.delegate_background(
        runtime::Role::Worker,
        "target".into(),
        Default::default(),
    );
    let other =
        host.runtime
            .delegate_background(runtime::Role::Worker, "other".into(), Default::default());
    let id = core.request().unwrap();
    let attempt = core.0.state.lock().unwrap().attempt.clone().unwrap();
    let proof = if sealing {
        phase(&core, RestartPhase::Sealing).await;
        None
    } else {
        Some(core.wait_sealed(id).await.unwrap())
    };
    assert_eq!(model.0.load(std::sync::atomic::Ordering::SeqCst), 0);
    host.runtime.stop(target, StopScope::SelfOnly).unwrap();
    phase(&core, RestartPhase::Running).await;
    assert_eq!(
        host.runtime.quiesce_state(),
        runtime::quiesce::QuiesceState::Running
    );
    assert_eq!(gate.phase(), HostRestartPhase::Open);
    assert!(
        host.restart_producers()
            .iter()
            .all(|producer| producer.try_enter().is_some())
    );
    assert!(core.snapshot().id.is_none());
    if let Some(proof) = proof {
        assert!(proof.validate(&host.runtime).is_err());
    }
    assert!(core.wait_sealed(id).await.is_err());
    assert_eq!(
        host.runtime.wait(target).await.unwrap(),
        event_bus::AgentRunPhase::Stopped
    );
    assert_eq!(
        host.runtime.wait(other).await.unwrap(),
        event_bus::AgentRunPhase::Done
    );
    assert_eq!(model.0.load(std::sync::atomic::Ordering::SeqCst), 1);
    drop(input);
    let next = core.request().unwrap();
    core.wait_sealed(next).await.unwrap();
    assert_eq!(
        host.runtime.restart_attempt_state(&attempt),
        runtime::quiesce::RestartAttemptState::Superseded
    );
    assert!(core.cancel(id).is_err());
    assert_eq!(core.snapshot().phase, RestartPhase::Sealed);
    assert_eq!(gate.phase(), HostRestartPhase::Sealed);
    assert!(host.runtime.producer_gate().try_enter().is_none());
    core.cancel(next).unwrap();
    host.shutdown().await.unwrap();
}
#[tokio::test]
async fn direct_runtime_stop_after_seal_reopens_core_host_and_producers() {
    direct_runtime_stop_reconciles(false).await;
}
#[tokio::test]
async fn direct_runtime_stop_in_host_permit_window_is_cancellation_not_failure() {
    direct_runtime_stop_reconciles(true).await;
}
#[tokio::test]
async fn direct_runtime_stop_invalidation_failure_does_not_reopen_host_or_producers() {
    let model = Arc::new(Model::default());
    let (_dir, mut host) = boot_with_model(model.clone()).await;
    let core = RestartCoordinator::new(&host);
    let target = host.runtime.delegate_background(
        runtime::Role::Worker,
        "target".into(),
        Default::default(),
    );
    let other =
        host.runtime
            .delegate_background(runtime::Role::Worker, "other".into(), Default::default());
    let id = core.request().unwrap();
    core.wait_sealed(id).await.unwrap();
    let db = rusqlite::Connection::open(&host.composition.storage.db_path).unwrap();
    db.execute_batch("CREATE TRIGGER reject_cancel BEFORE UPDATE ON resume_intents BEGIN SELECT RAISE(ABORT, 'cancel failed'); END;").unwrap();
    assert!(host.runtime.stop(target, StopScope::SelfOnly).is_err());
    phase(&core, RestartPhase::FailedClosed).await;
    assert!(host.runtime.producer_gate().try_enter().is_none());
    assert_eq!(
        host.host.lock().unwrap().service().restart_gate().phase(),
        HostRestartPhase::FailedClosed
    );
    assert_eq!(model.0.load(std::sync::atomic::Ordering::SeqCst), 0);
    // Parking retains the public Running phase; completion/provider execution must stay blocked.
    let mut waiting = Box::pin(host.runtime.wait(other));
    assert!(
        std::future::poll_fn(|cx| std::task::Poll::Ready(waiting.as_mut().poll(cx).is_pending()))
            .await
    );
    assert!(matches!(
        host.runtime.quiesce_state(),
        runtime::quiesce::QuiesceState::Failed { .. }
    ));
    drop(waiting);
    db.execute_batch("DROP TRIGGER reject_cancel;").unwrap();
    assert!(host.shutdown().await.is_err());
}

#[tokio::test]
async fn accepted_unpolled_mcp_operation_finishes_before_seal_and_late_request_is_busy() {
    use workbench_protocol::{
        commands::Command,
        ids::{ProjectId, SessionId},
        namespaces::mcp::{self, McpCommand, McpQuery, McpReply},
        queries::{Query, QueryReply},
    };
    let (_dir, mut host) = boot().await;
    let core = RestartCoordinator::new(&host);
    let operation_id = mcp::McpOperationId::new("accepted-mcp").unwrap();
    let command = |operation_id| {
        Command::Mcp(McpCommand::Refresh(mcp::Refresh {
            operation_id,
            project: ProjectId::new("p").unwrap(),
        }))
    };
    host.host
        .lock()
        .unwrap()
        .handle_command(
            command(operation_id.clone()),
            workbench_host::provenance::local_desktop(),
        )
        .unwrap();
    // The current-thread executor has not polled the accepted operation yet.
    let id = core.request().unwrap();
    let gate = host.host.lock().unwrap().service().restart_gate();
    let mut drained = Box::pin(gate.drained());
    assert!(
        std::future::poll_fn(|cx| std::task::Poll::Ready(drained.as_mut().poll(cx).is_pending()))
            .await
    );
    core.wait_sealed(id).await.unwrap();
    let result = host
        .host
        .lock()
        .unwrap()
        .handle_query(
            &SessionId::new("test").unwrap(),
            &Query::Mcp(McpQuery::GetOperation { operation_id }),
        )
        .unwrap();
    assert!(matches!(result, QueryReply::Mcp(McpReply::Operation(view))
        if matches!(view.state, mcp::McpOperationState::Succeeded { .. })));
    let late = host
        .host
        .lock()
        .unwrap()
        .handle_command(
            command(mcp::McpOperationId::new("late-mcp").unwrap()),
            workbench_host::provenance::local_desktop(),
        )
        .unwrap_err();
    assert_eq!(late.code, workbench_protocol::errors::ErrorCode::Busy);
    core.cancel(id).unwrap();
    host.shutdown().await.unwrap();
}
