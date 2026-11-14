use super::*;
use runtime::{
    AgentInvocationContext, AgentModel, Role, RunConfig, RunStore, RuntimeError,
    quiesce::{CORE_RECOVERY_SETTINGS_REFERENCE, RecoveryRecipe, ResumePolicy},
};
use std::{
    collections::BTreeMap,
    sync::atomic::{AtomicUsize, Ordering},
};
use workbench_host::{bootstrap::services::BackgroundServices, projects::StaticProjects};
struct ParkedModel {
    admissions: AtomicUsize,
    entered: tokio::sync::Notify,
    release: tokio::sync::Semaphore,
}
impl Default for ParkedModel {
    fn default() -> Self {
        Self {
            admissions: AtomicUsize::new(0),
            entered: tokio::sync::Notify::new(),
            release: tokio::sync::Semaphore::new(0),
        }
    }
}
#[async_trait::async_trait]
impl AgentModel for ParkedModel {
    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "test/model".into()
    }
    fn requires_admission(&self) -> bool {
        true
    }
    async fn admit(&self, _: &AgentInvocationContext, _: Role) -> Result<(), RuntimeError> {
        self.admissions.fetch_add(1, Ordering::SeqCst);
        self.entered.notify_one();
        self.release.acquire().await.unwrap().forget();
        Ok(())
    }
    async fn complete(
        &self,
        _: &AgentInvocationContext,
        _: Role,
        _: &[providers::Message],
        _: &[providers::ToolSpec],
    ) -> Result<providers::ChatResponse, RuntimeError> {
        panic!("parked startup must not execute a model")
    }
}
fn runtime(
    config: &storage::StorageConfig,
    writer: &storage::StorageHandle,
    model: Arc<ParkedModel>,
) -> AgentRuntime {
    let bus = Arc::new(event_bus::EventBus::new(256));
    AgentRuntime::new(bus.clone(), Arc::new(tools::ToolExecutor::new(bus)), model)
        .with_run_store(RunStore::open(config, writer.clone()).unwrap())
}
async fn available_catalog(options: &BootstrapOptions) -> RecoveryCatalog {
    let directory = options.config_dir.join("catalog");
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::copy(
        test_support::model_catalog_dir().join("models-dev.json"),
        directory.join("models-dev.json"),
    )
    .unwrap();
    let mut services = BackgroundServices::default();
    let catalog = RecoveryCatalog::preload(options, &Default::default(), &mut services)
        .await
        .unwrap();
    services.shutdown().await.unwrap();
    catalog
}
#[tokio::test]
async fn prepared_adoption_preserves_claim_and_fences_release_or_post_ack_replay() {
    for activate in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let owners = test_socket_dir::SocketDir::new();
        let options = BootstrapOptions {
            config_dir: root.clone(),
            project_root: root.clone(),
            ownership_root: owners.path().to_path_buf(),
        };
        let config = storage::StorageConfig {
            db_path: root.join("events.db"),
            ..Default::default()
        };
        let leases = storage::shared_with_wait(
            &config.db_path,
            &options.ownership_root,
            std::time::Duration::ZERO,
        )
        .unwrap();
        let storage =
            storage::Storage::open_with_lease(config.clone(), leases.db_lease().unwrap()).unwrap();
        let writer = storage.handle();
        let startup = leases
            .recovery_startup_complete(&config.db_path, &options.ownership_root)
            .unwrap();
        let model = Arc::new(ParkedModel::default());
        let source = runtime(&config, &writer, model.clone());
        let run = source
            .delegate_chat(
                "thread",
                Role::Orchestrator,
                "park".into(),
                RunConfig {
                    project_root: Some(root.clone()),
                    interactive: true,
                    keep_alive: true,
                    conversation: true,
                    ..Default::default()
                },
            )
            .unwrap();
        model.entered.notified().await;
        source.request_quiesce();
        model.release.add_permits(1);
        source.wait_quiesced().await.unwrap();
        let fence = source.fence_dispatch_restart().await.unwrap();
        source.drain_dispatch_tails(&fence).await.unwrap();
        let records = source
            .capture_recovery_records(
                &fence,
                &BTreeMap::from([(
                    run,
                    RecoveryRecipe {
                        settings_reference: CORE_RECOVERY_SETTINGS_REFERENCE.into(),
                        policy: ResumePolicy::CoreOwned,
                        goal_id: None,
                        team: None,
                        lane: None,
                        descriptor: None,
                    },
                )]),
            )
            .unwrap();
        let manifests = source
            .save_recovery_graph("coordinator-test", &records)
            .unwrap();
        assert_eq!(manifests.len(), 1);
        writer
            .upsert_thread_runtime(&storage::threads::ThreadUpsert {
                thread_id: "thread".into(),
                project_root: Some(root.to_string_lossy().into_owned()),
                root_run_id: Some(run.to_string()),
                chat_role: Some(Role::Orchestrator.name().into()),
                ..Default::default()
            })
            .unwrap();
        let mut projects = StaticProjects::default();
        projects.insert(
            &workbench_protocol::ids::ProjectId::new("project").unwrap(),
            root.clone(),
        );
        let catalog = available_catalog(&options).await;
        let first = runtime(&config, &writer, model.clone());
        let owner = Arc::new(
            OwnerHost::open(
                &options.ownership_root,
                Default::default(),
                first.event_bus().clone(),
            )
            .unwrap(),
        );
        let coordinator = StartupCoordinator::new(
            StartupInputs {
                runtime: &first,
                owner: &owner,
                storage: &config,
                writer: &writer,
                options: &options,
                projects: &projects,
            },
            startup.try_clone().unwrap(),
            catalog.clone(),
        )
        .unwrap();
        assert_eq!(
            coordinator.scan().unwrap().components[0].disposition,
            Disposition::AwaitingAuthority
        );
        let before = model.admissions.load(Ordering::SeqCst);
        let prepared = coordinator.prepare(&manifests[0]).unwrap();
        let claim = storage::Database::open(&config)
            .unwrap()
            .recovery_claims()
            .unwrap()
            .remove(0);
        assert_eq!(prepared.claim_id(), claim.claim_id);
        assert_eq!(claim.state, RecoveryClaimState::Prepared);
        assert!(first.list_agents().is_empty());
        assert!(first.has_paused_recovery());
        assert_eq!(model.admissions.load(Ordering::SeqCst), before);
        let second = runtime(&config, &writer, model.clone());
        let successor = Arc::new(
            OwnerHost::open(
                &options.ownership_root,
                Default::default(),
                second.event_bus().clone(),
            )
            .unwrap(),
        );
        let reopened = StartupCoordinator::new(
            StartupInputs {
                runtime: &second,
                owner: &successor,
                storage: &config,
                writer: &writer,
                options: &options,
                projects: &projects,
            },
            startup,
            catalog,
        )
        .unwrap();
        assert_eq!(
            reopened.prepare(&manifests[0]).err().unwrap(),
            StartupBlocked::OwnerBlocked
        );
        assert_eq!(
            storage::Database::open(&config)
                .unwrap()
                .recovery_claims()
                .unwrap()
                .as_slice(),
            std::slice::from_ref(&claim)
        );
        assert!(!owner.begin_quiesce().unwrap());
        assert!(!owner.release_ready().unwrap());
        let adopted = reopened.prepare(&manifests[0]).unwrap();
        assert_eq!(adopted.claim_id(), prepared.claim_id());
        assert_eq!(
            storage::Database::open(&config)
                .unwrap()
                .recovery_claims()
                .unwrap(),
            [claim]
        );
        assert!(
            prepared.release(&first).is_err(),
            "stale current owner cannot release the claim"
        );
        assert_eq!(model.admissions.load(Ordering::SeqCst), before);
        if activate {
            assert_eq!(adopted.activate(&second).unwrap(), [run]);
            model.entered.notified().await;
            assert_eq!(model.admissions.load(Ordering::SeqCst), before + 1);
            let after_ack = leases
                .recovery_startup_complete(&config.db_path, &options.ownership_root)
                .unwrap();
            let observer = runtime(&config, &writer, model.clone());
            let observer_owner = Arc::new(
                OwnerHost::open(
                    &options.ownership_root,
                    Default::default(),
                    observer.event_bus().clone(),
                )
                .unwrap(),
            );
            let observed = StartupCoordinator::new(
                StartupInputs {
                    runtime: &observer,
                    owner: &observer_owner,
                    storage: &config,
                    writer: &writer,
                    options: &options,
                    projects: &projects,
                },
                after_ack,
                RecoveryCatalog::default(),
            )
            .unwrap();
            assert_eq!(
                observed.scan().unwrap().components[0].disposition,
                Disposition::AlreadyActivated
            );
            assert_eq!(
                observed.prepare(&manifests[0]).err().unwrap(),
                StartupBlocked::AlreadyActivated
            );
            assert_eq!(model.admissions.load(Ordering::SeqCst), before + 1);
            second.stop(run, runtime::StopScope::Subtree).unwrap();
            second.wait(run).await.unwrap();
        } else {
            adopted.release(&second).unwrap();
            assert!(!second.has_paused_recovery());
            assert_eq!(
                reopened.scan().unwrap().components[0].disposition,
                Disposition::Released
            );
            assert_eq!(
                reopened.prepare(&manifests[0]).err().unwrap(),
                StartupBlocked::Released
            );
            assert_eq!(model.admissions.load(Ordering::SeqCst), before);
        }
    }
}

#[tokio::test]
async fn wrong_startup_evidence_or_missing_high_watermarks_do_not_install_the_lane_fence() {
    let dir = tempfile::tempdir().unwrap();
    let owners = test_socket_dir::SocketDir::new();
    let root = dir.path().canonicalize().unwrap();
    let options = BootstrapOptions {
        config_dir: root.clone(),
        project_root: root.clone(),
        ownership_root: owners.path().to_path_buf(),
    };
    let config = storage::StorageConfig {
        db_path: root.join("events.db"),
        ..Default::default()
    };
    let leases = storage::shared_with_wait(
        &config.db_path,
        &options.ownership_root,
        std::time::Duration::ZERO,
    )
    .unwrap();
    let storage =
        storage::Storage::open_with_lease(config.clone(), leases.db_lease().unwrap()).unwrap();
    let writer = storage.handle();
    let startup = leases
        .recovery_startup_complete(&config.db_path, &options.ownership_root)
        .unwrap();
    let model = Arc::new(ParkedModel::default());
    let bus = Arc::new(event_bus::EventBus::new(32));
    let missing = AgentRuntime::new(
        bus.clone(),
        Arc::new(tools::ToolExecutor::new(bus.clone())),
        model.clone(),
    );
    let owner =
        Arc::new(OwnerHost::open(&options.ownership_root, Default::default(), bus).unwrap());
    let projects = StaticProjects::default();
    assert_eq!(
        StartupCoordinator::new(
            StartupInputs {
                runtime: &missing,
                owner: &owner,
                storage: &config,
                writer: &writer,
                options: &options,
                projects: &projects
            },
            startup.try_clone().unwrap(),
            RecoveryCatalog::default()
        )
        .err()
        .unwrap(),
        StartupBlocked::NeedsAttention
    );
    let attached = runtime(&config, &writer, model);
    let other = storage::StorageConfig {
        db_path: root.join("other.db"),
        ..Default::default()
    };
    // Current writer/runtime evidence must not authorize a separately supplied
    // catalog/binding scan against a different database.
    assert_eq!(
        StartupCoordinator::new(
            StartupInputs {
                runtime: &attached,
                owner: &owner,
                storage: &other,
                writer: &writer,
                options: &options,
                projects: &projects
            },
            startup.try_clone().unwrap(),
            RecoveryCatalog::default()
        )
        .err()
        .unwrap(),
        StartupBlocked::StorageUnavailable
    );
    let foreign = storage::shared_with_wait(
        &other.db_path,
        &options.ownership_root,
        std::time::Duration::ZERO,
    )
    .unwrap();
    let foreign_startup = foreign
        .recovery_startup_complete(&other.db_path, &options.ownership_root)
        .unwrap();
    assert_eq!(
        StartupCoordinator::new(
            StartupInputs {
                runtime: &attached,
                owner: &owner,
                storage: &config,
                writer: &writer,
                options: &options,
                projects: &projects
            },
            foreign_startup,
            RecoveryCatalog::default()
        )
        .err()
        .unwrap(),
        StartupBlocked::StorageUnavailable
    );
    let current = StartupCoordinator::new(
        StartupInputs {
            runtime: &attached,
            owner: &owner,
            storage: &config,
            writer: &writer,
            options: &options,
            projects: &projects,
        },
        startup,
        RecoveryCatalog::default(),
    )
    .unwrap();
    assert!(current.scan().unwrap().components.is_empty());
    assert!(
        storage::Database::open(&config)
            .unwrap()
            .recovery_claims()
            .unwrap()
            .is_empty()
    );
    assert!(matches!(
        owner.attach("thread"),
        Err(runtime::ownership::RegistryError::Absent)
    ));
}

#[tokio::test]
async fn corrupt_peer_claim_does_not_block_prepare_or_same_claim_adoption() {
    let dir = test_socket_dir::SplitDir::new();
    let root = dir.path().canonicalize().unwrap();
    let options = BootstrapOptions {
        config_dir: root.clone(),
        project_root: root.clone(),
        ownership_root: dir.sockets().to_path_buf(),
    };
    let config = storage::StorageConfig {
        db_path: root.join("events.db"),
        ..Default::default()
    };
    let leases = storage::shared_with_wait(
        &config.db_path,
        &options.ownership_root,
        std::time::Duration::ZERO,
    )
    .unwrap();
    let storage =
        storage::Storage::open_with_lease(config.clone(), leases.db_lease().unwrap()).unwrap();
    let writer = storage.handle();
    let model = Arc::new(ParkedModel::default());
    let source = runtime(&config, &writer, model.clone());
    let mut recipes = BTreeMap::new();
    let mut runs = vec![];
    for thread in ["bad", "good"] {
        let run = source
            .delegate_chat(
                thread,
                Role::Orchestrator,
                "park".into(),
                RunConfig {
                    project_root: Some(root.clone()),
                    interactive: true,
                    keep_alive: true,
                    conversation: true,
                    ..Default::default()
                },
            )
            .unwrap();
        model.entered.notified().await;
        runs.push(run);
        recipes.insert(
            run,
            RecoveryRecipe {
                settings_reference: CORE_RECOVERY_SETTINGS_REFERENCE.into(),
                policy: ResumePolicy::CoreOwned,
                goal_id: None,
                team: None,
                lane: None,
                descriptor: None,
            },
        );
        writer
            .upsert_thread_runtime(&storage::threads::ThreadUpsert {
                thread_id: thread.into(),
                project_root: Some(root.to_string_lossy().into_owned()),
                root_run_id: Some(run.to_string()),
                chat_role: Some(Role::Orchestrator.name().into()),
                ..Default::default()
            })
            .unwrap();
    }
    source.request_quiesce();
    model.release.add_permits(2);
    source.wait_quiesced().await.unwrap();
    let fence = source.fence_dispatch_restart().await.unwrap();
    source.drain_dispatch_tails(&fence).await.unwrap();
    let records = source.capture_recovery_records(&fence, &recipes).unwrap();
    source
        .save_recovery_graph("mixed-claims", &records)
        .unwrap();
    let manifests = storage::Database::open(&config)
        .unwrap()
        .recovery_manifests()
        .unwrap();
    let manifest = |run: RunId| {
        manifests
            .iter()
            .find(|m| {
                m.record
                    .as_ref()
                    .unwrap()
                    .members
                    .iter()
                    .any(|member| member.run_id == run.to_string())
            })
            .unwrap()
            .manifest_id
            .clone()
    };
    let bad = manifest(runs[0]);
    let good = manifest(runs[1]);
    writer
        .claim_recovery_manifest(storage::recovery::RecoveryClaimRequest {
            claim_id: "bad-claim".into(),
            manifest_id: bad.clone(),
            incarnations: vec![(runs[0].to_string(), "bad-new-incarnation".into())],
        })
        .unwrap();
    rusqlite::Connection::open(&config.db_path)
        .unwrap()
        .execute(
            "UPDATE recovery_claim_members SET old_incarnation=x'ff' WHERE claim_id='bad-claim'",
            [],
        )
        .unwrap();
    let mut projects = StaticProjects::default();
    projects.insert(
        &workbench_protocol::ids::ProjectId::new("project").unwrap(),
        root.clone(),
    );
    let first = runtime(&config, &writer, model.clone());
    let owner = Arc::new(
        OwnerHost::open(
            &options.ownership_root,
            Default::default(),
            first.event_bus().clone(),
        )
        .unwrap(),
    );
    let coordinator = StartupCoordinator::new(
        StartupInputs {
            runtime: &first,
            owner: &owner,
            storage: &config,
            writer: &writer,
            options: &options,
            projects: &projects,
        },
        leases
            .recovery_startup_complete(&config.db_path, &options.ownership_root)
            .unwrap(),
        available_catalog(&options).await,
    )
    .unwrap();
    let scan = coordinator.scan().unwrap();
    assert!(scan.unreadable_claims);
    assert_eq!(
        scan.components
            .iter()
            .find(|c| c.manifest_id == bad)
            .unwrap()
            .disposition,
        Disposition::NeedsAttention
    );
    assert_eq!(
        coordinator.prepare(&bad).err(),
        Some(StartupBlocked::NeedsAttention)
    );
    let prepared = coordinator.prepare(&good).unwrap();
    let claim_id = prepared.claim_id().to_owned();
    let claim = storage::Database::open(&config)
        .unwrap()
        .recovery_claim_loads()
        .unwrap()
        .into_iter()
        .find(|(m, _)| m == &good)
        .unwrap()
        .1
        .unwrap();
    assert!(!owner.begin_quiesce().unwrap());
    assert!(!owner.release_ready().unwrap());
    let second = runtime(&config, &writer, model.clone());
    let successor = Arc::new(
        OwnerHost::open(
            &options.ownership_root,
            Default::default(),
            second.event_bus().clone(),
        )
        .unwrap(),
    );
    let reopened = StartupCoordinator::new(
        StartupInputs {
            runtime: &second,
            owner: &successor,
            storage: &config,
            writer: &writer,
            options: &options,
            projects: &projects,
        },
        leases
            .recovery_startup_complete(&config.db_path, &options.ownership_root)
            .unwrap(),
        available_catalog(&options).await,
    )
    .unwrap();
    let adopted = reopened.prepare(&good).unwrap();
    assert_eq!(adopted.claim_id(), claim_id);
    assert_eq!(
        storage::Database::open(&config)
            .unwrap()
            .recovery_claim_loads()
            .unwrap()
            .into_iter()
            .find(|(m, _)| m == &good)
            .unwrap()
            .1
            .unwrap(),
        claim
    );
    assert_eq!(model.admissions.load(Ordering::SeqCst), 2);
    adopted.release(&second).unwrap();
}
