//! Owner loss is not permission to stop or release a successor's Prepared claim.
use super::*;

#[tokio::test]
async fn authority_free_preparation_cannot_stop_or_release_during_shutdown() {
    let f = Fixture::new(0);
    let run = saved(&f).await;
    let runtime = new_process(&f);
    let paused = prepare(&f, &runtime);
    let db = storage::Database::open(&f.config).unwrap();
    let before = db
        .recovery_claim_snapshot(&paused.claim().claim_id)
        .unwrap();
    assert!(runtime.stop(run, StopScope::SelfOnly).is_err());
    assert!(runtime.release_shutdown_recovery().is_err());
    assert_eq!(
        db.recovery_claim_snapshot(&paused.claim().claim_id)
            .unwrap(),
        before
    );
    assert!(runtime.has_paused_recovery());
    assert_eq!(f.model.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn prepared_owner_handoff_rejects_old_stop_and_shutdown_but_successor_can_adopt() {
    let f = Fixture::new(0);
    let run = saved(&f).await;
    let runtime = new_process(&f);
    let paused = prepare(&f, &runtime);
    let claim = paused.claim().clone();
    let (old, current) = authority(&f, run);
    let permit = current.members[&run].ownership.as_ref().unwrap();
    let (proof, startup, root) = evidence(&f, &claim.claim_id);
    let _adopted = runtime
        .adopt_prepared_claim(proof, startup, &root, &current)
        .unwrap();
    let next = crate::ownership::OwnerHost::open(
        f._dir.sockets(),
        Default::default(),
        Arc::new(EventBus::new(64)),
    )
    .unwrap();
    let next_permit = old.handoff(permit, &next).unwrap();
    let db = storage::Database::open(&f.config).unwrap();
    let before = db.recovery_claim_snapshot(&claim.claim_id).unwrap();
    assert!(runtime.stop(run, StopScope::SelfOnly).is_err());
    assert!(runtime.release_shutdown_recovery().is_err());
    assert_eq!(db.recovery_claim_snapshot(&claim.claim_id).unwrap(), before);
    assert!(runtime.has_paused_recovery());
    let successor = new_process(&f);
    let mut config = current.members[&run].clone();
    config.ownership = Some(next_permit);
    let authority = CurrentRecoveryAuthority {
        members: BTreeMap::from([(run, config)]),
    };
    let (proof, startup, root) = evidence(&f, &claim.claim_id);
    let mut adopted = successor
        .adopt_prepared_claim(proof, startup, &root, &authority)
        .unwrap();
    assert_eq!(adopted.claim(), &claim);
    successor
        .restore_relations_and_continuations(&mut adopted)
        .unwrap();
    successor.activate_component(adopted, authority).unwrap();
    f.model.entered.notified().await;
    successor.cancel(run).unwrap();
    let _ = successor.wait(run).await;
}

#[tokio::test]
async fn an_explicit_prepared_stop_cannot_be_reclassified_as_retryable() {
    let f = Fixture::new(0);
    let run = saved(&f).await;
    let runtime = new_process(&f);
    let paused = prepare(&f, &runtime);
    let (owner, authority) = authority(&f, run);
    let (proof, startup, root) = evidence(&f, &paused.claim().claim_id);
    let paused = runtime
        .adopt_prepared_claim(proof, startup, &root, &authority)
        .unwrap();
    runtime.stop(run, StopScope::SelfOnly).unwrap();
    // Repeating Stop is safe, but a stopped old incarnation cannot be adopted.
    runtime.stop(run, StopScope::SelfOnly).unwrap();
    let leases =
        storage::shared_with_wait(&f.config.db_path, &root, std::time::Duration::ZERO).unwrap();
    let startup = leases
        .recovery_startup_complete(&f.config.db_path, &root)
        .unwrap();
    assert!(matches!(
        runtime
            .recover_recovery_claim(&paused.claim().claim_id, startup, &root, &authority)
            .unwrap(),
        crate::RecoveryClaimHandle::Quarantined(_)
    ));
    let db = storage::Database::open(&f.config).unwrap();
    let before = db
        .recovery_claim_snapshot(&paused.claim().claim_id)
        .unwrap();
    runtime.release_shutdown_recovery().unwrap();
    let after = db
        .recovery_claim_snapshot(&paused.claim().claim_id)
        .unwrap();
    assert!(matches!(
        after.claim.state,
        storage::recovery::RecoveryClaimState::Released { .. }
    ));
    assert_eq!(after.intents, before.intents);
    assert_eq!(after.manifest, before.manifest);
    assert!(!runtime.has_paused_recovery());
    assert!(runtime.list_agents().is_empty());
    assert_eq!(f.model.calls.load(Ordering::SeqCst), 0);
    drop(owner);
}

#[tokio::test]
async fn stopped_prepared_shutdown_cannot_release_after_owner_handoff() {
    let f = Fixture::new(0);
    let run = saved(&f).await;
    let runtime = new_process(&f);
    let paused = prepare(&f, &runtime);
    let (old, authority) = authority(&f, run);
    let (proof, startup, root) = evidence(&f, &paused.claim().claim_id);
    let _adopted = runtime
        .adopt_prepared_claim(proof, startup, &root, &authority)
        .unwrap();
    runtime.stop(run, StopScope::SelfOnly).unwrap();
    let next = crate::ownership::OwnerHost::open(
        f._dir.sockets(),
        Default::default(),
        Arc::new(EventBus::new(64)),
    )
    .unwrap();
    old.handoff(authority.members[&run].ownership.as_ref().unwrap(), &next)
        .unwrap();
    let db = storage::Database::open(&f.config).unwrap();
    let before = db
        .recovery_claim_snapshot(&paused.claim().claim_id)
        .unwrap();
    assert!(runtime.release_shutdown_recovery().is_err());
    assert_eq!(
        db.recovery_claim_snapshot(&paused.claim().claim_id)
            .unwrap(),
        before
    );
    assert!(runtime.has_paused_recovery());
    assert_eq!(f.model.calls.load(Ordering::SeqCst), 0);
}
