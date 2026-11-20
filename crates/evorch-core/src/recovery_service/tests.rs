use super::*;
use runtime::{AgentInvocationContext, AgentModel, Role, RuntimeError};
use storage::recovery::graph_control::GraphEffectKey;
use workbench_host::bootstrap::{
    BootstrapOptions,
    core_startup::{CoreBootMode, RecoveryAdminError, RecoveryAdminResult, RecoveryResumeRequest},
};

struct NoProvider;
#[async_trait::async_trait]
impl AgentModel for NoProvider {
    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "unused".into()
    }
    async fn complete(
        &self,
        _: &AgentInvocationContext,
        _: Role,
        _: &[providers::Message],
        _: &[providers::ToolSpec],
    ) -> Result<providers::ChatResponse, RuntimeError> {
        panic!("integration seam must not call a provider")
    }
}

struct Fixture {
    inputs: CoreStartupInputs,
    _storage: storage::Storage,
    _leases: storage::SharedLeases,
    _owners: test_socket_dir::SocketDir,
    _root: tempfile::TempDir,
}
impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let owners = test_socket_dir::SocketDir::new();
        let options = BootstrapOptions {
            config_dir: root.path().to_path_buf(),
            project_root: root.path().to_path_buf(),
            ownership_root: owners.path().to_path_buf(),
        };
        let config = storage::StorageConfig {
            db_path: root.path().join("events.db"),
            ..Default::default()
        };
        let leases =
            storage::shared_with_wait(&config.db_path, owners.path(), Duration::ZERO).unwrap();
        let storage =
            storage::Storage::open_with_lease(config.clone(), leases.db_lease().unwrap()).unwrap();
        let writer = storage.handle();
        let bus = Arc::new(event_bus::EventBus::new(256));
        let runtime = runtime::AgentRuntime::new(
            bus.clone(),
            Arc::new(tools::ToolExecutor::new(bus.clone())),
            Arc::new(NoProvider),
        )
        .with_run_store(runtime::RunStore::open(&config, writer.clone()).unwrap())
        .with_learning_origin(runtime::learning_jobs::current_origin(
            storage::learning_jobs::LearningOriginKind::Core,
            Some("service-test".into()),
        ));
        let owner = Arc::new(
            runtime::ownership::OwnerHost::open(owners.path(), Default::default(), bus).unwrap(),
        );
        let host = Arc::new(std::sync::Mutex::new(workbench_host::CommandHost::new(
            workbench_host::commands::CommandService::new(
                runtime.clone(),
                tokio::runtime::Handle::current(),
            ),
        )));
        let inputs = CoreStartupInputs {
            host,
            runtime,
            owner,
            options,
            storage: config.clone(),
            writer,
            mode: CoreBootMode::Cold,
            projects: Default::default(),
            startup: leases
                .recovery_startup_complete(&config.db_path, owners.path())
                .unwrap(),
            work_changed: Default::default(),
        };
        Self {
            inputs,
            _storage: storage,
            _leases: leases,
            _owners: owners,
            _root: root,
        }
    }
    fn startup_inputs(&self) -> CoreStartupInputs {
        let i = &self.inputs;
        CoreStartupInputs {
            host: i.host.clone(),
            runtime: i.runtime.clone(),
            owner: i.owner.clone(),
            options: i.options.clone(),
            storage: i.storage.clone(),
            writer: i.writer.clone(),
            mode: i.mode,
            projects: i.projects.clone(),
            startup: i.startup.try_clone().unwrap(),
            work_changed: i.work_changed.clone(),
        }
    }
    fn request(&self, id: &str) -> RecoveryResumeRequest {
        let effect = GraphEffectKey {
            operation_id: format!("op-{id}"),
            attempt_nonce: "attempt".into(),
        };
        let binding = serde_json::from_value(serde_json::json!({
            "graph": {
                "graph_id": id, "restart_tx_id": "tx", "capture_id": "capture",
                "expected_revision": "1", "expected_core_epoch": "1",
                "owner_identity": null, "owner_generation": null, "operation_id": effect.operation_id,
            },
            "owners": [], "expected_control_revision": "0",
        })).unwrap();
        RecoveryResumeRequest {
            manifest_id: id.into(),
            binding,
            reservation: self
                .inputs
                .runtime
                .reserve_manual_activation(id, effect.clone(), 0)
                .unwrap(),
            effect,
        }
    }
}

#[tokio::test]
async fn accepts_manual_requests_after_the_retry_set_becomes_empty() {
    let fixture = Fixture::new();
    let mut services = BackgroundServices::default();
    let diagnostics = CoreGraphRecovery
        .start(fixture.startup_inputs(), &mut services)
        .await
        .unwrap();
    assert!(
        !diagnostics.has_pending_work(),
        "idle service is not an active producer"
    );
    let port = diagnostics.recovery_admin();
    port.retry_automatic("removed-graph".into())
        .unwrap()
        .await
        .unwrap();
    assert!(diagnostics.has_pending_work());
    // The production retry tick scans and removes this missing graph. Wait for
    // its published state, not elapsed wall-clock time.
    while diagnostics.has_pending_work() {
        fixture.inputs.work_changed.notified().await;
    }
    assert_eq!(
        port.resume(fixture.request("manual")).unwrap().await,
        Err(RecoveryAdminError::Unsupported)
    );
    assert!(!diagnostics.has_pending_work());
    assert!(
        fixture
            .inputs
            .runtime
            .reserve_automatic_activation("manual")
            .is_ok()
    );
    // Open/Blocked work can be re-enqueued even after that first exhaustion.
    port.retry_automatic("another-removed-graph".into())
        .unwrap()
        .await
        .unwrap();
    assert!(diagnostics.has_pending_work());
    services.shutdown().await.unwrap();
    assert!(!diagnostics.has_pending_work());
    assert!(matches!(
        port.resume(fixture.request("stopped")),
        Err(RecoveryAdminError::Stopped)
    ));
}

struct ParkedHandler {
    entered: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<Result<(), StartupBlocked>>>>,
    release: std::sync::Barrier,
}
impl RecoveryAdminHandler for ParkedHandler {
    fn resume(
        &self,
        context: &RecoveryAdminContext,
        request: RecoveryResumeRequest,
    ) -> RecoveryAdminResult {
        let _request = request;
        let ready = context.coordinator().map(|_| ());
        let failed = ready.is_err();
        self.entered
            .lock()
            .unwrap()
            .take()
            .unwrap()
            .send(ready)
            .unwrap();
        if failed {
            return Err(RecoveryAdminError::Unavailable);
        }
        self.release.wait();
        Err(RecoveryAdminError::Unsupported)
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admitted_job_survives_client_cancellation_and_shutdown_joins_it() {
    let fixture = Fixture::new();
    let (entered, started) = tokio::sync::oneshot::channel();
    let handler = Arc::new(ParkedHandler {
        entered: std::sync::Mutex::new(Some(entered)),
        release: std::sync::Barrier::new(2),
    });
    let mut services = BackgroundServices::default();
    let diagnostics = CoreGraphRecovery::with_admin_handler(handler.clone())
        .start(fixture.startup_inputs(), &mut services)
        .await
        .unwrap();
    let reply = diagnostics
        .recovery_admin()
        .resume(fixture.request("parked"))
        .unwrap();
    started.await.unwrap().unwrap();
    assert!(
        diagnostics.has_pending_work(),
        "running manual job must participate in drain"
    );
    drop(reply);
    assert!(
        fixture
            .inputs
            .runtime
            .reserve_automatic_activation("parked")
            .is_err()
    );
    services.request_stop();
    // The worker, not the canceled reply future or stop signal, owns the token.
    assert!(
        fixture
            .inputs
            .runtime
            .reserve_automatic_activation("parked")
            .is_err()
    );
    handler.release.wait();
    services.shutdown().await.unwrap();
    assert!(!diagnostics.has_pending_work());
    assert!(
        fixture
            .inputs
            .runtime
            .reserve_automatic_activation("parked")
            .is_ok()
    );
}

#[tokio::test]
async fn mailbox_accepts_64_requests_while_the_automatic_worker_is_occupied() {
    let fixture = Fixture::new();
    let (actor, receiver) = RecoveryAdminActor::channel();
    let host = fixture.inputs.host.lock().unwrap();
    // Keep the receiver unpolled and host mutex held, exactly as during a
    // synchronous automatic attempt. Mailbox admission cannot wait on either.
    let replies = (0..64)
        .map(|id| {
            actor
                .resume(fixture.request(&format!("graph-{id}")))
                .unwrap()
        })
        .collect::<Vec<_>>();
    assert!(matches!(
        actor.retry_automatic("overflow".into()),
        Err(RecoveryAdminError::Busy)
    ));
    assert!(
        fixture
            .inputs
            .runtime
            .reserve_automatic_activation("graph-0")
            .is_err()
    );
    assert!(
        fixture
            .inputs
            .runtime
            .reserve_automatic_activation("capacity")
            .is_err()
    );
    drop(host);
    drop(receiver);
    for reply in replies {
        assert_eq!(reply.await, Err(RecoveryAdminError::Stopped));
    }
    assert!(
        fixture
            .inputs
            .runtime
            .reserve_automatic_activation("capacity")
            .is_ok()
    );
}

#[tokio::test]
async fn automatic_attempt_cannot_prepare_or_decline_a_manual_reserved_graph() {
    let fixture = Fixture::new();
    let coordinator = StartupCoordinator::new(
        StartupInputs {
            runtime: &fixture.inputs.runtime,
            owner: &fixture.inputs.owner,
            storage: &fixture.inputs.storage,
            writer: &fixture.inputs.writer,
            options: &fixture.inputs.options,
            projects: &fixture.inputs.projects,
        },
        fixture.inputs.startup.try_clone().unwrap(),
        RecoveryCatalog::default(),
    )
    .unwrap();
    let manual = fixture.request("contested");
    let component = ComponentStatus {
        manifest_id: "contested".into(),
        runs: vec![],
        disposition: Disposition::AwaitingAuthority,
    };
    assert!(matches!(
        attempt(
            &coordinator,
            &fixture.inputs.runtime,
            &fixture.inputs.host,
            &component
        ),
        AutomaticAttempt::Contended
    ));
    drop(manual);
    let automatic = fixture
        .inputs
        .runtime
        .reserve_automatic_activation("contested")
        .unwrap();
    assert!(
        fixture
            .inputs
            .runtime
            .reserve_manual_activation(
                "contested",
                GraphEffectKey {
                    operation_id: "manual-loser".into(),
                    attempt_nonce: "attempt".into(),
                },
                0
            )
            .is_err()
    );
    fixture
        .inputs
        .runtime
        .check_automatic_activation(&automatic)
        .unwrap();
    drop(automatic);
    assert!(
        fixture
            .inputs
            .runtime
            .reserve_manual_activation(
                "contested",
                GraphEffectKey {
                    operation_id: "manual-after-return".into(),
                    attempt_nonce: "attempt".into(),
                },
                0
            )
            .is_ok()
    );
    // Also exercise simultaneous admissions. Keep both results alive until
    // both calls return, so releasing the winner cannot let the loser win too.
    let start = std::sync::Barrier::new(2);
    let acquired = std::sync::Barrier::new(2);
    let runtime = &fixture.inputs.runtime;
    let (automatic, manual) = std::thread::scope(|scope| {
        let automatic = scope.spawn(|| {
            start.wait();
            let result = runtime.reserve_automatic_activation("concurrent");
            acquired.wait();
            result
        });
        let manual = scope.spawn(|| {
            start.wait();
            let result = runtime.reserve_manual_activation(
                "concurrent",
                GraphEffectKey {
                    operation_id: "concurrent-manual".into(),
                    attempt_nonce: "attempt".into(),
                },
                0,
            );
            acquired.wait();
            result
        });
        (automatic.join().unwrap(), manual.join().unwrap())
    });
    assert_eq!(
        usize::from(automatic.is_ok()) + usize::from(manual.is_ok()),
        1
    );
    for error in [automatic.err(), manual.err()].into_iter().flatten() {
        assert!(matches!(
            error,
            runtime::RuntimeError::ActivationReservationUnavailable {
                reason: runtime::ActivationReservationProblem::Contended
            }
        ));
    }
    assert!(
        storage::Database::open(&fixture.inputs.storage)
            .unwrap()
            .recovery_claims()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn pending_work_counts_retry_and_running_jobs_not_historical_manual_display() {
    let (actor, _receiver) = RecoveryAdminActor::channel();
    let diagnostics = RecoveryDiagnostics {
        report: Arc::new(RwLock::new(RecoveryStartupReport {
            scan: Default::default(),
            components: vec![RecoveryDiagnostic {
                manifest_id: "manual-idle".into(),
                runs: vec![],
                state: RecoveryState::Blocked,
                reason: Some(StartupBlocked::OwnerBlocked),
                manual_continue: false,
            }],
        })),
        retry_count: Default::default(),
        jobs: Default::default(),
        admin: Arc::new(actor),
    };
    assert!(!diagnostics.has_pending_work());
    diagnostics.retry_count.store(1, Ordering::Release);
    assert!(diagnostics.has_pending_work());
    diagnostics.retry_count.store(0, Ordering::Release);
    let job = admin::JobGuard::start(&diagnostics.jobs, &Default::default());
    assert!(diagnostics.has_pending_work());
    drop(job);
    assert!(!diagnostics.has_pending_work());
}

#[tokio::test]
async fn consecutive_job_contexts_reuse_the_initial_fence_without_reinstalling_it() {
    let fixture = Fixture::new();
    let coordinator = StartupCoordinator::new(
        StartupInputs {
            runtime: &fixture.inputs.runtime,
            owner: &fixture.inputs.owner,
            storage: &fixture.inputs.storage,
            writer: &fixture.inputs.writer,
            options: &fixture.inputs.options,
            projects: &fixture.inputs.projects,
        },
        fixture.inputs.startup.try_clone().unwrap(),
        RecoveryCatalog::default(),
    )
    .unwrap();
    for _ in 0..2 {
        let job = RecoveryAdminContext::for_job(&coordinator, &fixture.inputs.host).unwrap();
        let coordinator = job.coordinator().unwrap();
        assert!(coordinator.scan().unwrap().components.is_empty());
    }
    assert!(
        fixture.inputs.runtime.fence_graph_lane_recovery().is_err(),
        "original immutable fence is still installed"
    );
}
