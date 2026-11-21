use super::*;
use crate::admin_composition::{
    Inputs,
    restart::tests::composed::{ComposedCore, masked_child},
};
use crate::restart::tests::checked_target;

#[tokio::test]
async fn metadata_uses_current_hello_epoch_and_full_builtin_inventory() {
    if masked_child().await {
        return;
    }
    let mut core = ComposedCore::new().await;
    let handle = core.driver.finalization();
    let owner = ProductFinalizationOwner::new(
        &Inputs::new(&core.host, &core.backend),
        handle,
        &core.context,
    )
    .unwrap();
    let management_epoch = core.host.admin.epoch();
    let original_hello = core.backend.projection.instance_epoch();
    core.backend.projection.invalidate();
    let hello = core.backend.projection.instance_epoch();
    assert_ne!(hello, original_hello);
    assert_eq!(core.host.admin.epoch(), management_epoch);
    let id = core
        .driver
        .restart
        .request_planned(Default::default())
        .unwrap();
    core.driver.restart.wait_sealed(id).await.unwrap();
    let (current, target) = checked_target::checked_target(core.dir.path()).await;
    let tx = owner.prepare(id, &current, &target).unwrap();
    let m = tx.metadata;
    assert_eq!(m.restart_id, id.public_id().as_str());
    assert_eq!(m.config_identity, core.context.instance().identity);
    assert_eq!(m.old_epoch, hello.0);
    assert_ne!(m.old_epoch, management_epoch.0);
    assert_eq!(m.successor_epoch, hello.0 + 1);
    assert_eq!(
        m.old_build,
        crate::handoff_description::describe()
            .unwrap()
            .build_identity
    );
    assert_eq!(
        m.successor_build,
        target.request().description().build_identity
    );
    assert_eq!(m.target_identity, target.request().id());
    assert_eq!(
        m.old_builtin_tool_names,
        core.host.runtime.restart_builtin_tool_names()
    );
    assert!(m.old_builtin_tool_names.len() > runtime::compiled_standard_tool_catalog().len());
    assert_eq!(
        m.storage_version as i64,
        storage::Database::open(core.host.storage_config())
            .unwrap()
            .pragma_i64("user_version")
            .unwrap()
    );
    assert_eq!(tx.phase, RestartPhase::Preparing);
    assert!(!m.capture_id.is_empty());
    core.driver.restart.cancel(id).unwrap();
    core.host.shutdown().await.unwrap();
}

#[tokio::test]
async fn create_ack_loss_reads_same_id_and_never_accepts_absent_changed_or_later_phase() {
    if masked_child().await {
        return;
    }
    for fault in [None, Some(false), Some(true)] {
        let mut core = ComposedCore::new().await;
        let id = core
            .driver
            .restart
            .request_planned(Default::default())
            .unwrap();
        core.driver.restart.wait_sealed(id).await.unwrap();
        let owner = ProductFinalizationOwner::new(
            &Inputs::new(&core.host, &core.backend),
            core.driver.finalization(),
            &core.context,
        )
        .unwrap();
        let (current, target) = checked_target::checked_target(core.dir.path()).await;
        if let Some(after_commit) = fault {
            core.host
                .storage_writer()
                .inject_restart_write_fault(after_commit)
                .unwrap();
        }
        let result = owner.prepare(id, &current, &target);
        let db = storage::Database::open(core.host.storage_config()).unwrap();
        let saved = db.restart_transactions().unwrap();
        if fault == Some(false) {
            assert!(result.is_err());
            assert!(saved.is_empty());
        } else {
            let tx = result.unwrap();
            assert_eq!(saved.len(), 1);
            assert_eq!(saved[0], tx);
            let mut changed = tx.metadata.clone();
            changed.old_build = "different".into();
            assert!(create_and_read_back(core.host.storage_writer(), &db, changed).is_err());
            core.host
                .storage_writer()
                .mutate_restart(
                    tx.metadata.restart_id.clone(),
                    Some(tx.revision),
                    RestartMutation::Invalidate {
                        reason: "fixture".into(),
                    },
                )
                .unwrap();
            assert!(create_and_read_back(core.host.storage_writer(), &db, tx.metadata).is_err());
        }
        core.driver.restart.cancel(id).unwrap();
        core.host.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn failed_create_consumes_delivery_and_never_recreates_on_a_second_finalize() {
    if masked_child().await {
        return;
    }
    let mut core = ComposedCore::new().await;
    let handle = core.driver.finalization();
    handle.install_environment_builder(environment).unwrap();
    let owner = ProductFinalizationOwner::new(
        &Inputs::new(&core.host, &core.backend),
        handle.clone(),
        &core.context,
    )
    .unwrap();
    let id = core
        .driver
        .restart
        .request_planned(Default::default())
        .unwrap();
    core.driver.restart.wait_sealed(id).await.unwrap();
    let (current, target) = checked_target::checked_target(core.dir.path()).await;
    core.host
        .storage_writer()
        .inject_restart_write_fault(false)
        .unwrap();
    assert!(
        owner
            .finalize(core.driver.restart.clone(), id, current, target)
            .await
            .is_err()
    );
    let (current, target) = checked_target::checked_target(core.dir.path()).await;
    assert!(
        owner
            .finalize(core.driver.restart.clone(), id, current, target)
            .await
            .is_err()
    );
    assert!(
        storage::Database::open(core.host.storage_config())
            .unwrap()
            .restart_transactions()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        core.driver.restart.snapshot().phase,
        crate::restart::RestartPhase::Running
    );
    core.host.shutdown().await.unwrap();
}

#[tokio::test]
async fn accepted_start_releases_continuation_before_actual_workers_join_and_is_single_use() {
    if masked_child().await {
        return;
    }
    let mut core = ComposedCore::new().await;
    let handle = core.driver.finalization();
    let owner = ProductFinalizationOwner::new(
        &Inputs::new(&core.host, &core.backend),
        handle.clone(),
        &core.context,
    )
    .unwrap();
    handle.install_environment_builder(environment).unwrap();
    let id = core
        .driver
        .restart
        .request_planned(Default::default())
        .unwrap();
    core.driver.restart.wait_sealed(id).await.unwrap();
    let coordinator = core.driver.restart.clone();
    let (current, target) = checked_target::checked_target(core.dir.path()).await;
    // Stage 6 must fail deterministically AFTER WorkersJoined, never exec the
    // test process. The checked capability was real when it was prepared.
    std::fs::remove_file(core.dir.path().join("described-elf")).unwrap();
    let continuation = owner.finalize(coordinator.clone(), id, current, target);
    let listener = tokio::net::UnixListener::bind(core.dir.sockets().join("loop")).unwrap();
    {
        let running = core.driver.run_owned_with_web(
            &mut core.host,
            core.backend.clone(),
            listener,
            None,
            core.context.signals().clone(),
            &core.context,
        );
        tokio::pin!(running);
        tokio::select! {
            result = &mut running => panic!("premature loop exit {result:?}"),
            result = continuation => assert!(matches!(result, Ok(workbench_host::admin::AdminExecutionOutcome::AwaitRestartRecovery))),
        }
        assert!(matches!(handle.claim(id), Err(FinalizationError::Conflict)));
        tokio::select! {
            result = &mut running => panic!("premature loop exit {result:?}"),
            _ = coordinator.wait_finalization(id) => (),
        }
        crate::restart::tests::finalization::assert_workers_joined(&coordinator);
        rustix::process::kill_process(rustix::process::getpid(), rustix::process::Signal::TERM)
            .unwrap();
        running.await.unwrap();
    }
    assert!(core.host.shutdown().await.is_err());
}
