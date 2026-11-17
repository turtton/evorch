use super::*;
use core_launcher::handoff::{
    CurrentElf, HandoffDescription, ProbeLimits, SchemaRange,
    targets::{RestartTarget, TargetRequirements, TargetSelection},
};
use std::{os::unix::fs::PermissionsExt, path::Path, sync::atomic::Ordering};

fn quote(path: &Path) -> String {
    format!("'{}'", path.to_str().unwrap().replace('\'', "'\\''"))
}
async fn target(dir: &Path) -> (FinalizationTarget, RestartMetadata) {
    // This child only describes a real ELF. The call count independently proves
    // that stage 6 does NOT reprobe/spawn after the final spawn fence.
    static CURRENT: std::sync::OnceLock<Arc<CurrentElf>> = std::sync::OnceLock::new();
    let current = CURRENT
        .get_or_init(|| Arc::new(CurrentElf::record().unwrap()))
        .clone();
    // Use a small real ELF rather than repeatedly hashing this large debug test
    // binary. Neither this describing wrapper nor that ELF is ever execed.
    let elf = dir.join("described-elf");
    std::fs::copy("/bin/sh", &elf).unwrap();
    std::fs::set_permissions(&elf, std::fs::Permissions::from_mode(0o700)).unwrap();
    let d = HandoffDescription {
        version: 1,
        build_identity: core_instance::handoff::elf_identity(&elf).unwrap(),
        elf_path: elf,
        handoff_format: 1,
        snapshot_format: 1,
        schema: SchemaRange {
            minimum: 1,
            maximum: 1000,
        },
        protocol: 1,
        platform: format!("linux-{}", std::env::consts::ARCH),
        tool_catalog: "fixture-catalog".into(),
    };
    let path = dir.join("target");
    let calls = dir.join("probes");
    let json = serde_json::to_string(&d).unwrap().replace('\'', "'\\''");
    std::fs::write(
        &path,
        format!(
            "#!/bin/sh\nprintf 'probe\\n' >> {}\nprintf '%s\\n' '{}'\n",
            quote(&calls),
            json,
        ),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    let checked = RestartTarget::preflight(
        TargetSelection {
            id: "target",
            configured_path: Some(&path),
            timeout: std::time::Duration::from_secs(300),
        },
        &current,
        TargetRequirements {
            handoff: 1,
            snapshot: 1,
            storage_schema: 62,
            protocol: 1,
            platform: &d.platform,
        },
        ProbeLimits::default(),
        std::future::pending(),
    )
    .await
    .unwrap()
    .before_capture(&current, ProbeLimits::default(), std::future::pending())
    .await
    .unwrap();
    let mut metadata = metadata();
    metadata.successor_build = d.build_identity;
    (FinalizationTarget::new(current, checked), metadata)
}
async fn owned_chat(host: &BootstrappedHost, core: &RestartCoordinator) -> (RunId, RestartId) {
    let permit = host.owner.start("active").unwrap();
    host.owner.start("idle").unwrap();
    let events = host.runtime.event_bus().subscribe();
    let run = host
        .runtime
        .delegate_chat(
            "active",
            runtime::Role::Orchestrator,
            "park".into(),
            runtime::RunConfig {
                ownership: Some(permit),
                interactive: true,
                keep_alive: true,
                ..Default::default()
            },
        )
        .unwrap();
    waiting(run, events).await;
    let id = core
        .request_planned(BTreeMap::from([(
            run,
            runtime::quiesce::RecoveryRecipe {
                settings_reference: runtime::quiesce::CORE_RECOVERY_SETTINGS_REFERENCE.into(),
                policy: runtime::quiesce::ResumePolicy::CoreOwned,
                goal_id: None,
                team: None,
                lane: None,
                descriptor: None,
            },
        )]))
        .unwrap();
    core.wait_sealed(id).await.unwrap();
    (run, id)
}
fn create(host: &BootstrappedHost, metadata: RestartMetadata) -> RestartTransaction {
    host.composition
        .storage_writer
        .mutate_restart(
            "restart".into(),
            None,
            RestartMutation::Create(Box::new(metadata)),
        )
        .unwrap()
}
async fn reclaimed(host: &BootstrappedHost, core: &RestartCoordinator, held: bool) {
    let resources = core.0.state.lock().unwrap().finalization.clone().unwrap();
    assert!(resources.lock().await.cleanup().await.is_ok());
    let tx = saved(host);
    assert_eq!(
        tx.phase,
        storage::recovery::restart::RestartPhase::Invalidated
    );
    for thread in ["active", "idle"] {
        let owner = host.owner.attach(thread).unwrap();
        assert!(owner.handoff_hold.is_none());
        assert_eq!(owner.lease.generation, 1);
        assert_eq!(
            owner.state,
            if held {
                runtime::ownership::OwnerState::Quiescing
            } else {
                runtime::ownership::OwnerState::Running
            }
        );
    }
    assert!(
        tx.owner_progress
            .iter()
            .all(|p| p.hold_removed && !p.worker_joined && !p.socket_unlinked)
    );
    assert!(host.restart_producers().iter().all(|gate| !gate.is_open()));
}

#[tokio::test]
async fn stages_six_to_eight_hold_active_and_idle_read_back_arm_then_invalidate_without_exec() {
    let (dir, mut host) = boot().await;
    let (mut target, metadata) = target(dir.path()).await;
    let core = RestartCoordinator::new(&host);
    let (run, id) = owned_chat(&host, &core).await;
    let order = Arc::new(Mutex::new(Vec::new()));
    let record = order.clone();
    target.observe = Some(Arc::new(move |stage| record.lock().unwrap().push(stage)));
    let tx = create(&host, metadata);
    core.start_finalization(id, &mut host, tx, target).unwrap();
    assert!(core.wait_finalization(id).await.is_err());
    assert_eq!(core.snapshot().phase, RestartPhase::FailedClosed);
    assert_eq!(
        core.0.state.lock().unwrap().completed_stage,
        Some(ExitStage::Armed),
        "tx={:?}; owners={:?}",
        saved(&host),
        host.owner.restart_inventory().unwrap()
    );
    assert_eq!(
        *order.lock().unwrap(),
        [
            ExitStage::TargetChecked,
            ExitStage::OwnersHeld,
            ExitStage::Armed
        ]
    );
    reclaimed(&host, &core, true).await;
    let tx = saved(&host);
    assert!(
        tx.ticket_nonce_hash
            .as_ref()
            .unwrap()
            .starts_with("sha256:")
    );
    assert_eq!(tx.owner_progress.len(), 2);
    assert_eq!(tx.expected.len(), 1);
    let active = tx
        .owner_progress
        .iter()
        .find(|p| p.thread_id == "active")
        .unwrap();
    assert!(
        active.active_runs.is_empty(),
        "the live owner turn really checkpointed"
    );
    assert_eq!(active.parked_runs, BTreeSet::from([run.to_string()]));
    assert!(
        tx.owner_progress
            .iter()
            .find(|p| p.thread_id == "idle")
            .unwrap()
            .active_runs
            .is_empty()
    );
    assert!(
        tx.owner_progress
            .iter()
            .all(|p| p.hold_saved && p.hold_read_back)
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("probes")).unwrap(),
        "probe\nprobe\n"
    );
    host.runtime.stop(run, StopScope::SelfOnly).unwrap();
    host.runtime.wait(run).await.unwrap();
    core.transition_to_shutdown();
    assert!(host.shutdown().await.is_err());
}

#[derive(Clone, Copy, Debug)]
enum Race {
    Stop,
    Shutdown,
    Failure,
    FailureStop,
    FailureShutdown,
}

#[tokio::test]
async fn shared_cleanup_retries_unconfirmed_invalidation_and_hold_removal_without_resume() {
    for block_invalidation in [false, true] {
        let model = Arc::new(Model::default());
        let (dir, mut host) = boot_with_model(model.clone()).await;
        let (mut target, metadata) = target(dir.path()).await;
        let core = RestartCoordinator::new(&host);
        let (run, id) = owned_chat(&host, &core).await;
        let path = if block_invalidation {
            host.composition.storage.db_path.clone()
        } else {
            dir.path().join("o/owners.db")
        };
        let fault_path = path.clone();
        target.after = Some(Arc::new(move |stage| {
            if stage == ExitStage::Armed {
                rusqlite::Connection::open(&fault_path).unwrap().execute_batch(
                    if block_invalidation {
                        "CREATE TRIGGER deny_cleanup BEFORE UPDATE ON restart_transactions WHEN NEW.phase='invalidated' BEGIN SELECT RAISE(ABORT,'invalidation unconfirmed'); END;"
                    } else {
                        "CREATE TRIGGER deny_cleanup BEFORE UPDATE ON thread_owners WHEN json_extract(OLD.state,'$.handoff_hold') IS NOT NULL AND json_extract(NEW.state,'$.handoff_hold') IS NULL BEGIN SELECT RAISE(ABORT,'hold removal unconfirmed'); END;"
                    },
                ).unwrap();
            }
        }));
        let tx = create(&host, metadata);
        core.start_finalization(id, &mut host, tx, target).unwrap();
        assert!(core.wait_finalization(id).await.is_err());
        assert_eq!(core.snapshot().phase, RestartPhase::FailedClosed);
        let resources = core.0.state.lock().unwrap().finalization.clone().unwrap();
        let mut resources = resources.lock().await;
        assert!(resources.cleanup().await.is_err());
        assert_eq!(
            saved(&host).phase,
            if block_invalidation {
                storage::recovery::restart::RestartPhase::Armed
            } else {
                storage::recovery::restart::RestartPhase::Invalidated
            }
        );
        for thread in ["active", "idle"] {
            assert!(host.owner.attach(thread).unwrap().handoff_hold.is_some());
        }
        assert!(host.restart_producers().iter().all(|g| !g.is_open()));
        rusqlite::Connection::open(path)
            .unwrap()
            .execute_batch("DROP TRIGGER deny_cleanup")
            .unwrap();
        // The original error remains sticky, but independent reclamation retries.
        assert!(resources.cleanup().await.is_err());
        let tx = saved(&host);
        assert_eq!(
            tx.phase,
            storage::recovery::restart::RestartPhase::Invalidated
        );
        assert!(tx.owner_progress.iter().all(|p| p.hold_removed));
        for thread in ["active", "idle"] {
            let row = host.owner.attach(thread).unwrap();
            assert!(row.handoff_hold.is_none());
            assert_eq!(row.lease.generation, 1);
        }
        assert!(resources.cleanup().await.is_err());
        assert_eq!(saved(&host).revision, tx.revision, "cleanup is idempotent");
        assert_eq!(model.0.load(std::sync::atomic::Ordering::SeqCst), 1);
        drop(resources);
        host.runtime.stop(run, StopScope::SelfOnly).unwrap();
        host.runtime.wait(run).await.unwrap();
        assert!(host.shutdown().await.is_err());
    }
}
async fn matrix(stage: ExitStage) {
    for race in [
        Race::Stop,
        Race::Shutdown,
        Race::Failure,
        Race::FailureStop,
        Race::FailureShutdown,
    ] {
        let model = Arc::new(Model::default());
        let (dir, mut host) = boot_with_model(model.clone()).await;
        let (mut target, metadata) = target(dir.path()).await;
        let (returned, service_returned) = tokio::sync::oneshot::channel();
        host.services
            .start("later-cleanup-witness", |stop| async move {
                stop.await?;
                returned.send(()).unwrap();
                Ok(())
            });
        let core = RestartCoordinator::new(&host);
        let (run, id) = owned_chat(&host, &core).await;
        let (arrived, ready) = tokio::sync::oneshot::channel();
        let (release, receive) = std::sync::mpsc::channel();
        let barrier = Mutex::new(Some((arrived, receive)));
        target.observe = Some(Arc::new(move |now| {
            if now == stage {
                let (arrived, receive) = barrier.lock().unwrap().take().unwrap();
                arrived.send(()).unwrap();
                receive.recv().unwrap();
            }
        }));
        let tx = create(&host, metadata);
        core.start_finalization(id, &mut host, tx, target).unwrap();
        ready.await.unwrap();
        assert_eq!(model.0.load(Ordering::SeqCst), 1);
        if matches!(
            race,
            Race::Failure | Race::FailureStop | Race::FailureShutdown
        ) {
            match stage {
                ExitStage::TargetChecked => {
                    std::fs::write(dir.path().join("target"), "changed").unwrap()
                }
                ExitStage::OwnersHeld => {
                    let db = rusqlite::Connection::open(&host.composition.storage.db_path).unwrap();
                    db.execute_batch("CREATE TRIGGER fail_holds BEFORE INSERT ON restart_owner_progress BEGIN SELECT RAISE(ABORT,'hold progress rejected'); END;").unwrap();
                }
                ExitStage::Armed => {
                    let db = rusqlite::Connection::open(&host.composition.storage.db_path).unwrap();
                    db.execute_batch("CREATE TRIGGER fail_arm BEFORE UPDATE OF phase ON restart_transactions WHEN NEW.phase='armed' BEGIN SELECT RAISE(ABORT,'Arm rejected'); END;").unwrap();
                }
                _ => unreachable!(),
            }
        }
        let shutdown = matches!(race, Race::Shutdown | Race::FailureShutdown);
        if shutdown {
            core.transition_to_shutdown();
            // Unlike capture-time Stop, ShuttingDown delivers terminal-only.
            host.runtime.stop(run, StopScope::SelfOnly).unwrap();
            host.runtime.wait(run).await.unwrap();
            assert_eq!(core.snapshot().phase, RestartPhase::ShuttingDown);
        } else if matches!(race, Race::Stop | Race::FailureStop) {
            for direct in [true, false] {
                let stopped = if direct {
                    host.runtime.stop(run, StopScope::SelfOnly)
                } else {
                    RestartStopHandler::stop(&core, run, StopScope::SelfOnly)
                };
                assert!(
                    matches!(stopped, Err(RuntimeError::RestartBusy)),
                    "{stage:?}/{race:?}"
                );
            }
            assert!(core.cancel(id).is_err());
        }
        release.send(()).unwrap();
        assert!(core.wait_finalization(id).await.is_err());
        let last = core.0.state.lock().unwrap().completed_stage.unwrap();
        let failed = matches!(race, Race::Failure | Race::FailureStop);
        assert_eq!(
            last,
            if failed || shutdown {
                match stage {
                    ExitStage::TargetChecked => ExitStage::StorageDrained,
                    ExitStage::OwnersHeld => ExitStage::TargetChecked,
                    ExitStage::Armed => ExitStage::OwnersHeld,
                    _ => unreachable!(),
                }
            } else {
                ExitStage::Armed
            },
            "{stage:?}/{race:?}; tx={:?}; owners={:?}",
            saved(&host),
            host.owner.restart_inventory().unwrap()
        );
        let held = stage == ExitStage::Armed
            || (stage == ExitStage::OwnersHeld && failed)
            || (!failed && !shutdown);
        reclaimed(&host, &core, held).await;
        assert_eq!(model.0.load(Ordering::SeqCst), 1, "no double resume");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("probes")).unwrap(),
            "probe\nprobe\n"
        );
        core.transition_to_shutdown();
        assert!(host.shutdown().await.is_err());
        service_returned.await.unwrap();
        assert_eq!(model.0.load(Ordering::SeqCst), 1);
    }
}
#[tokio::test]
async fn target_failure_stop_shutdown_matrix_is_failed_closed_and_reclaims() {
    matrix(ExitStage::TargetChecked).await;
}
#[tokio::test]
async fn hold_failure_stop_shutdown_matrix_is_failed_closed_and_reclaims() {
    matrix(ExitStage::OwnersHeld).await;
}
#[tokio::test]
async fn arm_failure_stop_shutdown_matrix_is_failed_closed_and_reclaims() {
    matrix(ExitStage::Armed).await;
}

#[tokio::test]
async fn stop_and_shutdown_after_each_effect_never_resume_or_strand_armed_holds() {
    for stage in [
        ExitStage::TargetChecked,
        ExitStage::OwnersHeld,
        ExitStage::Armed,
    ] {
        for shutdown in [false, true] {
            let model = Arc::new(Model::default());
            let (dir, mut host) = boot_with_model(model.clone()).await;
            let (mut target, metadata) = target(dir.path()).await;
            let core = RestartCoordinator::new(&host);
            let (run, id) = owned_chat(&host, &core).await;
            let (arrived, ready) = tokio::sync::oneshot::channel();
            let (release, receive) = std::sync::mpsc::channel();
            let barrier = Mutex::new(Some((arrived, receive)));
            target.after = Some(Arc::new(move |now| {
                if now == stage {
                    let (arrived, receive) = barrier.lock().unwrap().take().unwrap();
                    arrived.send(()).unwrap();
                    receive.recv().unwrap();
                }
            }));
            let tx = create(&host, metadata);
            core.start_finalization(id, &mut host, tx, target).unwrap();
            ready.await.unwrap();
            let tx = saved(&host);
            assert_eq!(
                tx.phase,
                if stage == ExitStage::Armed {
                    storage::recovery::restart::RestartPhase::Armed
                } else {
                    storage::recovery::restart::RestartPhase::Committed
                }
            );
            assert_eq!(
                tx.owner_progress.len(),
                if stage >= ExitStage::OwnersHeld { 2 } else { 0 }
            );
            assert!(
                host.owner
                    .restart_inventory()
                    .unwrap()
                    .iter()
                    .all(|owner| owner.handoff_hold.is_some() == (stage >= ExitStage::OwnersHeld))
            );
            if shutdown {
                core.transition_to_shutdown();
                host.runtime.stop(run, StopScope::SelfOnly).unwrap();
                host.runtime.wait(run).await.unwrap();
            } else {
                assert!(matches!(
                    host.runtime.stop(run, StopScope::SelfOnly),
                    Err(RuntimeError::RestartBusy)
                ));
                assert!(matches!(
                    RestartStopHandler::stop(&core, run, StopScope::SelfOnly),
                    Err(RuntimeError::RestartBusy)
                ));
            }
            release.send(()).unwrap();
            assert!(core.wait_finalization(id).await.is_err());
            assert_eq!(
                core.0.state.lock().unwrap().completed_stage,
                Some(if shutdown { stage } else { ExitStage::Armed })
            );
            reclaimed(&host, &core, !shutdown || stage >= ExitStage::OwnersHeld).await;
            assert_eq!(
                core.snapshot().phase,
                if shutdown {
                    RestartPhase::ShuttingDown
                } else {
                    RestartPhase::FailedClosed
                }
            );
            assert_eq!(model.0.load(Ordering::SeqCst), 1);
            core.transition_to_shutdown();
            assert!(host.shutdown().await.is_err());
        }
    }
}

#[tokio::test]
async fn independent_readback_rejects_progress_and_arm_nonce_mismatch() {
    for arm in [false, true] {
        let (dir, mut host) = boot().await;
        let (target, metadata) = target(dir.path()).await;
        let core = RestartCoordinator::new(&host);
        let (_run, id) = owned_chat(&host, &core).await;
        let db = rusqlite::Connection::open(&host.composition.storage.db_path).unwrap();
        if arm {
            db.execute_batch("CREATE TRIGGER corrupt_nonce AFTER UPDATE OF phase ON restart_transactions WHEN NEW.phase='armed' BEGIN UPDATE restart_transactions SET ticket_nonce_hash='wrong-nonce' WHERE restart_id=NEW.restart_id; END;").unwrap();
        } else {
            db.execute_batch("CREATE TRIGGER change_progress_readback AFTER INSERT ON restart_owner_progress BEGIN UPDATE restart_owner_progress SET progress_json=json_set(progress_json,'$.hold_read_back',json('false')) WHERE restart_id=NEW.restart_id AND thread_id=NEW.thread_id; END;").unwrap();
        }
        let tx = create(&host, metadata);
        core.start_finalization(id, &mut host, tx, target).unwrap();
        assert!(core.wait_finalization(id).await.is_err());
        assert_eq!(
            core.0.state.lock().unwrap().completed_stage,
            Some(if arm {
                ExitStage::OwnersHeld
            } else {
                ExitStage::TargetChecked
            })
        );
        reclaimed(&host, &core, true).await;
        let tx = saved(&host);
        assert_eq!(tx.owner_progress.len(), if arm { 2 } else { 1 });
        assert_eq!(
            tx.ticket_nonce_hash.as_deref(),
            if arm { Some("wrong-nonce") } else { None }
        );
        assert!(tx.failure_reason.unwrap().contains("readback mismatch"));
        core.transition_to_shutdown();
        assert!(host.shutdown().await.is_err());
    }
}
