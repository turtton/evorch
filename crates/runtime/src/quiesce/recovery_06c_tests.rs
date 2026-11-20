//! Prepared adoption exercises the production writer proof, not invented IDs.
#[path = "recovery_model_tests.rs"]
mod models;
#[path = "recovery_owner_tests.rs"]
mod prepared_owner;
use super::*;
use crate::CurrentRecoveryAuthority;

pub(super) fn authority(
    f: &Fixture,
    run: RunId,
) -> (crate::ownership::OwnerHost, CurrentRecoveryAuthority) {
    let owner = crate::ownership::OwnerHost::open(
        f._dir.sockets(),
        Default::default(),
        Arc::new(EventBus::new(64)),
    )
    .unwrap();
    let authority = CurrentRecoveryAuthority {
        members: BTreeMap::from([(
            run,
            RunConfig {
                ownership: Some(owner.start(&run.to_string()).unwrap()),
                interactive: true,
                keep_alive: true,
                ..Default::default()
            },
        )]),
    };
    (owner, authority)
}
pub(super) fn evidence(
    f: &Fixture,
    claim: &str,
) -> (
    storage::recovery::ValidatedPreparedClaim,
    storage::RecoveryStartupComplete,
    std::path::PathBuf,
) {
    let root = f._dir.path().join("ownership");
    let leases =
        storage::shared_with_wait(&f.config.db_path, &root, std::time::Duration::ZERO).unwrap();
    let startup = leases
        .recovery_startup_complete(&f.config.db_path, &root)
        .unwrap();
    let proof = f
        ._storage
        .handle()
        .validate_prepared_claim(claim.into(), &startup, &root)
        .unwrap();
    (proof, startup, root)
}

#[tokio::test]
async fn same_adopted_successor_reattaches_the_same_prepared_claim_before_activation() {
    let f = Fixture::new(0);
    let run = saved(&f).await;
    let first = new_process(&f);
    let paused = prepare(&f, &first);
    let claim = paused.claim().clone();
    drop(paused);
    let db = storage::Database::open(&f.config).unwrap();
    let before = db.run_context(&run.to_string()).unwrap().unwrap();
    let runtime = new_process(&f);
    let (_owner, authority) = authority(&f, run);
    let (proof, startup, root) = evidence(&f, &claim.claim_id);
    let mut adopted = runtime
        .adopt_prepared_claim(proof, startup, &root, &authority)
        .unwrap();
    assert_eq!(adopted.claim(), &claim);
    assert_eq!(
        runtime
            .execution_snapshot(run)
            .unwrap()
            .publication()
            .incarnation(),
        claim.members[0].new_incarnation
    );
    assert_eq!(
        db.resume_intent_revision(&run.to_string()).unwrap(),
        Some(claim.members[0].reserved_revision)
    );
    assert_eq!(db.run_context(&run.to_string()).unwrap().unwrap(), before);
    assert!(runtime.list_agents().is_empty());
    assert_eq!(f.model.calls.load(Ordering::SeqCst), 0);
    assert_eq!(f.model.admission_calls.load(Ordering::SeqCst), 0);
    runtime
        .restore_relations_and_continuations(&mut adopted)
        .unwrap();
    ({
        let reservation = runtime
            .reserve_automatic_activation(&(adopted).claim().manifest_id)
            .unwrap();
        runtime.activate_component(adopted, authority, reservation)
    })
    .unwrap();
    let row = db.resume_intent(&run.to_string()).unwrap().unwrap();
    assert_eq!(row.incarnation, claim.members[0].new_incarnation);
    assert_eq!(
        db.resume_intent_revision(&run.to_string()).unwrap(),
        Some(claim.members[0].reserved_revision + 1)
    );
    runtime.cancel(run).unwrap();
}

#[tokio::test]
async fn failed_activation_can_reacquire_the_exact_local_control() {
    let f = Fixture::new(0);
    let run = saved(&f).await;
    let runtime = new_process(&f);
    let paused = prepare(&f, &runtime);
    let id = paused.claim().claim_id.clone();
    let control = runtime.execution_snapshot(run).unwrap();
    let (_owner, authority) = authority(&f, run);
    // Missing relations is an ordinary, pre-transaction activation failure.
    assert!(
        ({
            let reservation = runtime
                .reserve_automatic_activation(&(paused).claim().manifest_id)
                .unwrap();
            runtime.activate_component(
                paused,
                CurrentRecoveryAuthority {
                    members: BTreeMap::new(),
                },
                reservation,
            )
        })
        .is_err()
    );
    let (proof, startup, root) = evidence(&f, &id);
    drop(proof);
    let adopted = match runtime
        .recover_recovery_claim(&id, startup, &root, &authority)
        .unwrap()
    {
        crate::RecoveryClaimHandle::Retryable(paused) => paused,
        crate::RecoveryClaimHandle::Quarantined(_) => {
            panic!("valid Prepared claim must remain retryable")
        }
    };
    assert!(Arc::ptr_eq(
        &control,
        &runtime.execution_snapshot(run).unwrap()
    ));
    runtime
        .release_prepared_component(&adopted, "manual retry")
        .unwrap();
    assert!(!runtime.has_paused_recovery());
    assert_eq!(f.model.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn adoption_rechecks_stale_evidence_and_current_authority_without_registration() {
    let f = Fixture::new(0);
    let run = saved(&f).await;
    let first = new_process(&f);
    let paused = prepare(&f, &first);
    let id = paused.claim().claim_id.clone();
    let runtime = new_process(&f);
    let (proof, startup, root) = evidence(&f, &id);
    assert!(
        runtime
            .adopt_prepared_claim(
                proof,
                startup,
                &root,
                &CurrentRecoveryAuthority {
                    members: BTreeMap::new()
                }
            )
            .is_err()
    );
    assert!(!runtime.has_paused_recovery());
    let (proof, startup, root) = evidence(&f, &id);
    let (_owner, authority) = authority(&f, run);
    let (current_proof, current_startup, current_root) = evidence(&f, &id);
    let _bound = first
        .adopt_prepared_claim(current_proof, current_startup, &current_root, &authority)
        .unwrap();
    first.stop(run, StopScope::SelfOnly).unwrap();
    let db = storage::Database::open(&f.config).unwrap();
    let stopped = db.resume_intent(&run.to_string()).unwrap();
    let revision = db.resume_intent_revision(&run.to_string()).unwrap();
    assert!(
        runtime
            .adopt_prepared_claim(proof, startup, &root, &authority)
            .is_err()
    );
    assert!(!runtime.has_paused_recovery());
    assert_eq!(db.resume_intent(&run.to_string()).unwrap(), stopped);
    assert_eq!(
        db.resume_intent_revision(&run.to_string()).unwrap(),
        revision
    );
    assert_eq!(f.model.calls.load(Ordering::SeqCst), 0);
    first
        .release_prepared_component(&paused, "stopped")
        .unwrap();
}

fn lose_ack(f: &Fixture, runtime: &AgentRuntime, authority: CurrentRecoveryAuthority) -> String {
    let mut paused = prepare(f, runtime);
    let id = paused.claim().claim_id.clone();
    runtime
        .restore_relations_and_continuations(&mut paused)
        .unwrap();
    paused.lose_activation_ack_for_test();
    assert!(
        ({
            let reservation = runtime
                .reserve_automatic_activation(&(paused).claim().manifest_id)
                .unwrap();
            runtime.activate_component(paused, authority, reservation)
        })
        .is_err()
    );
    assert_eq!(
        storage::Database::open(&f.config)
            .unwrap()
            .recovery_claim_snapshot(&id)
            .unwrap()
            .claim
            .state,
        RecoveryClaimState::Activated
    );
    assert!(runtime.list_agents().is_empty());
    assert!(runtime.has_paused_recovery());
    assert_eq!(f.model.calls.load(Ordering::SeqCst), 0);
    assert_eq!(f.model.admission_calls.load(Ordering::SeqCst), 0);
    id
}
fn cleanup_handle(f: &Fixture, runtime: &AgentRuntime, id: &str) -> crate::RecoveryCleanupHandle {
    let root = f._dir.path().join("ownership");
    let leases =
        storage::shared_with_wait(&f.config.db_path, &root, std::time::Duration::ZERO).unwrap();
    let startup = leases
        .recovery_startup_complete(&f.config.db_path, &root)
        .unwrap();
    match runtime
        .recover_recovery_claim(
            id,
            startup,
            &root,
            &CurrentRecoveryAuthority {
                members: BTreeMap::new(),
            },
        )
        .unwrap()
    {
        crate::RecoveryClaimHandle::Quarantined(h) => *h,
        crate::RecoveryClaimHandle::Retryable(_) => panic!("activated state cannot be retried"),
    }
}

#[tokio::test]
async fn committed_ack_loss_cleanup_keeps_durable_activation_and_restores_manual_continue() {
    let f = Fixture::new(0);
    let run = saved(&f).await;
    let runtime = new_process(&f);
    let (_owner, authority) = authority(&f, run);
    let id = lose_ack(&f, &runtime, authority);
    let handle = cleanup_handle(&f, &runtime, &id);
    assert_eq!(
        handle.state(),
        crate::RecoveryCleanupState::ActivatedUnpublished
    );
    let db = storage::Database::open(&f.config).unwrap();
    let before = db.recovery_claim_snapshot(&id).unwrap();
    let snapshot = db.run_context(&run.to_string()).unwrap();
    assert!(runtime.stop(run, StopScope::SelfOnly).is_err());
    runtime.cleanup_recovery_claim(&handle).unwrap();
    assert!(!runtime.has_paused_recovery());
    assert!(!runtime.shared.quiesce.lock().intents.contains_key(&run));
    assert_eq!(db.recovery_claim_snapshot(&id).unwrap(), before);
    assert_eq!(db.run_context(&run.to_string()).unwrap(), snapshot);
    assert_eq!(f.model.calls.load(Ordering::SeqCst), 0);
    runtime
        .continue_goal(
            run,
            "manual input after ACK loss".into(),
            RunConfig::default(),
        )
        .unwrap();
    let resumed = db.resume_intent(&run.to_string()).unwrap().unwrap();
    assert_ne!(resumed.incarnation, before.claim.members[0].new_incarnation);
    runtime.cancel(run).unwrap();
    runtime.wait(run).await.unwrap();
}

#[tokio::test]
async fn shutdown_cleans_activated_ack_loss_without_releasing_the_claim() {
    let f = Fixture::new(0);
    let run = saved(&f).await;
    let runtime = new_process(&f);
    let (_owner, authority) = authority(&f, run);
    let id = lose_ack(&f, &runtime, authority);
    let db = storage::Database::open(&f.config).unwrap();
    let before = db.recovery_claim_snapshot(&id).unwrap();
    let fence = runtime
        .fence_dispatch_shutdown(|| async { Ok::<(), String>(()) })
        .await
        .unwrap();
    runtime.drain_dispatch_tails(&fence).await.unwrap();
    assert!(!runtime.has_paused_recovery());
    assert_eq!(db.recovery_claim_snapshot(&id).unwrap(), before);
    assert_eq!(before.claim.state, RecoveryClaimState::Activated);
    assert_eq!(f.model.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn old_cleanup_and_shutdown_never_mutate_a_successor_after_owner_handoff() {
    let f = Fixture::new(0);
    let run = saved(&f).await;
    let runtime = new_process(&f);
    let (owner, authority) = authority(&f, run);
    let old_permit = authority.members[&run].ownership.clone().unwrap();
    let stale_authority = CurrentRecoveryAuthority {
        members: authority.members.clone(),
    };
    let id = lose_ack(&f, &runtime, authority);
    let handle = cleanup_handle(&f, &runtime, &id);
    let next_owner = crate::ownership::OwnerHost::open(
        f._dir.sockets(),
        Default::default(),
        Arc::new(EventBus::new(64)),
    )
    .unwrap();
    let next_permit = owner.handoff(&old_permit, &next_owner).unwrap();
    assert!(old_permit.mutation_guard().is_err());
    assert!(
        runtime
            .stop_quarantined_recovery(&handle, &stale_authority, StopReason::OperatorStopped)
            .is_err()
    );
    let current_authority = CurrentRecoveryAuthority {
        members: BTreeMap::from([(
            run,
            RunConfig {
                ownership: Some(next_permit.clone()),
                interactive: true,
                keep_alive: true,
                ..Default::default()
            },
        )]),
    };
    let successor = new_process(&f);
    successor
        .continue_goal(
            run,
            "new owner input".into(),
            RunConfig {
                ownership: Some(next_permit),
                ..Default::default()
            },
        )
        .unwrap();
    let db = storage::Database::open(&f.config).unwrap();
    let third = db.resume_intent(&run.to_string()).unwrap().unwrap();
    let revision = db.resume_intent_revision(&run.to_string()).unwrap();
    let control = successor.execution_snapshot(run).unwrap();
    assert!(!control.cancelled_for_test());
    assert!(runtime.stop(run, StopScope::SelfOnly).is_err());
    assert!(
        runtime
            .stop_quarantined_recovery(&handle, &current_authority, StopReason::OperatorStopped)
            .is_err()
    );
    assert!(runtime.cleanup_recovery_claim(&handle).is_err());
    assert!(runtime.release_shutdown_recovery().is_err());
    assert!(runtime.has_paused_recovery());
    assert_eq!(db.resume_intent(&run.to_string()).unwrap(), Some(third));
    assert_eq!(
        db.resume_intent_revision(&run.to_string()).unwrap(),
        revision
    );
    assert!(!control.cancelled_for_test());
    successor.cancel(run).unwrap();
    successor.wait(run).await.unwrap();
}

#[tokio::test]
async fn released_cleanup_uses_readback_without_a_second_release_write() {
    let f = Fixture::new(0);
    let run = saved(&f).await;
    let runtime = new_process(&f);
    let paused = prepare(&f, &runtime);
    let id = paused.claim().claim_id.clone();
    f._storage
        .handle()
        .release_prepared_claim_if_unexecuted(id.clone(), "external resolution".into())
        .unwrap();
    drop(paused);
    let handle = cleanup_handle(&f, &runtime, &id);
    assert_eq!(handle.state(), crate::RecoveryCleanupState::Released);
    let db = storage::Database::open(&f.config).unwrap();
    let before = db.recovery_claim_snapshot(&id).unwrap();
    runtime.cleanup_recovery_claim(&handle).unwrap();
    assert_eq!(db.recovery_claim_snapshot(&id).unwrap(), before);
    assert!(!runtime.has_paused_recovery());
    runtime
        .continue_goal(
            run,
            "manual input after external release".into(),
            RunConfig::default(),
        )
        .unwrap();
    runtime.cancel(run).unwrap();
    runtime.wait(run).await.unwrap();
}

#[tokio::test]
async fn unresolved_stop_requires_live_owner_and_exact_claim_new_identity() {
    let f = Fixture::new(0);
    let run = saved(&f).await;
    let runtime = new_process(&f);
    let (_owner, authority) = authority(&f, run);
    let current = CurrentRecoveryAuthority {
        members: authority.members.clone(),
    };
    let id = lose_ack(&f, &runtime, authority);
    let db = storage::Database::open(&f.config).unwrap();
    // An ordinary revision is not evidence that activation can be retried.
    let row = db.resume_intent(&run.to_string()).unwrap().unwrap();
    f._storage
        .handle()
        .save_resume_intent(row.clone(), Some(row.incarnation))
        .unwrap();
    let handle = cleanup_handle(&f, &runtime, &id);
    assert_eq!(handle.state(), crate::RecoveryCleanupState::Unresolved);
    let revision = db
        .resume_intent_revision(&run.to_string())
        .unwrap()
        .unwrap();
    runtime
        .stop_quarantined_recovery(&handle, &current, StopReason::OperatorStopped)
        .unwrap();
    let stopped =
        ResumeIntent::decode(db.resume_intent(&run.to_string()).unwrap().unwrap()).unwrap();
    assert!(!stopped.active);
    assert!(!stopped.resume_requested);
    assert_eq!(stopped.stop_reason, Some(StopReason::OperatorStopped));
    assert_eq!(
        db.resume_intent_revision(&run.to_string()).unwrap(),
        Some(revision + 1)
    );
    assert_eq!(
        db.recovery_claim_snapshot(&id).unwrap().claim.state,
        RecoveryClaimState::Activated
    );
    assert!(runtime.has_paused_recovery());
    assert!(runtime.cleanup_recovery_claim(&handle).is_err());
    assert!(runtime.release_shutdown_recovery().is_err());
    assert_eq!(f.model.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn storage_prepared_proof_does_not_authorize_a_corrupt_runtime_root() {
    use storage::recovery::restart::{RestartExpectedMember, RestartMetadata, RestartMutation};

    let f = Fixture::new(0);
    let run = saved(&f).await;
    let db = storage::Database::open(&f.config).unwrap();
    let manifest = db
        .recovery_manifests()
        .unwrap()
        .pop()
        .unwrap()
        .record
        .unwrap();
    let member = &manifest.members[0];
    let record: RunRecoveryRecord = serde_json::from_str(&member.record_json).unwrap();
    let handle = f._storage.handle();
    let id = "corrupt-runtime-root";
    // Bind the healthy expected identities, but seal the malformed opaque runtime
    // payload below. A valid storage digest/proof is not runtime graph authority.
    let mut tx = handle
        .mutate_restart(
            id.into(),
            None,
            RestartMutation::Create(Box::new(RestartMetadata {
                restart_id: id.into(),
                capture_id: "corrupt-runtime-root/capture".into(),
                config_identity: "fixture-config".into(),
                old_epoch: 1,
                successor_epoch: 2,
                old_build: "fixture-build".into(),
                successor_build: "fixture-build".into(),
                target_identity: "fixture-target".into(),
                handoff_version: 1,
                snapshot_version: 1,
                storage_version: 62,
                old_builtin_tool_names: Default::default(),
                old_compiled_catalog_fingerprint: None,
                new_compiled_catalog_fingerprint: None,
                old_effective_tools_fingerprint: None,
                new_effective_tools_fingerprint: None,
            })),
        )
        .unwrap();
    tx = handle
        .mutate_restart(
            id.into(),
            Some(tx.revision),
            RestartMutation::BeginCapture(vec![RestartExpectedMember {
                run_id: member.run_id.clone(),
                incarnation: member.incarnation.clone(),
                root_run_id: record.root.to_string(),
                thread_id: record.thread.clone(),
                intent_revision: member.intent_revision,
                snapshot: member.snapshot.clone(),
                owner_id: None,
                owner_generation: None,
                lane_id: None,
            }]),
        )
        .unwrap();
    tx = handle
        .mutate_restart(
            id.into(),
            Some(tx.revision),
            RestartMutation::BindComponent {
                manifest_id: manifest.manifest_id.clone(),
            },
        )
        .unwrap();
    let conn = rusqlite::Connection::open(&f.config.db_path).unwrap();
    let mut corrupt: serde_json::Value = serde_json::from_str(&member.record_json).unwrap();
    let wrong_root = RunId::new(u64::MAX);
    assert_ne!(wrong_root, run);
    corrupt["root"] = serde_json::json!(wrong_root);
    assert_eq!(
        conn.execute(
            "UPDATE recovery_members SET record_json=?2 WHERE manifest_id=?1",
            rusqlite::params![
                manifest.manifest_id,
                serde_json::to_string(&corrupt).unwrap()
            ],
        )
        .unwrap(),
        1
    );
    tx = handle
        .mutate_restart(id.into(), Some(tx.revision), RestartMutation::Commit)
        .unwrap();
    handle
        .mutate_restart(
            id.into(),
            Some(tx.revision),
            RestartMutation::Arm {
                nonce_hash: "fixture-nonce".into(),
            },
        )
        .unwrap();
    let successor = new_process(&f);
    db.validate_restart_capture_digest(id).unwrap();

    // Only storage prepares this claim: runtime preparation would itself inspect
    // the malformed graph instead of testing the prepared-proof adoption guard.
    let root = f._dir.path().join("ownership");
    let leases =
        storage::shared_with_wait(&f.config.db_path, &root, std::time::Duration::ZERO).unwrap();
    let startup = leases
        .recovery_startup_complete(&f.config.db_path, &root)
        .unwrap();
    let claim = handle
        .claim_recovery_component(
            storage::recovery::RecoveryClaimRequest {
                claim_id: "corrupt-runtime-root/claim".into(),
                manifest_id: manifest.manifest_id.clone(),
                incarnations: vec![(
                    run.to_string(),
                    crate::runtime::ExecutionControl::new(false)
                        .publication()
                        .incarnation(),
                )],
            },
            &startup,
            &root,
        )
        .unwrap();
    let (proof, startup, root) = evidence(&f, &claim.claim_id);
    let before = db.recovery_claim_snapshot(&claim.claim_id).unwrap();
    let (_owner, authority) = authority(&f, run);
    let error = successor
        .adopt_prepared_claim(proof, startup, &root, &authority)
        .err()
        .expect("corrupt root must be rejected");
    assert_eq!(
        error,
        crate::RuntimeError::Model {
            reason: "durable intent: prepared root mismatch".into()
        }
    );
    assert!(!successor.has_paused_recovery());
    assert!(successor.list_agents().is_empty());
    assert_eq!(db.recovery_claim_snapshot(&claim.claim_id).unwrap(), before);
    assert_eq!(f.model.calls.load(Ordering::SeqCst), 0);
    assert_eq!(f.model.admission_calls.load(Ordering::SeqCst), 0);

    // A later payload change must still be rejected by the earlier digest fence,
    // even when that change repairs the root. Never rewrite the sealed digest.
    assert_eq!(
        conn.execute(
            "UPDATE recovery_members SET record_json=?2 WHERE manifest_id=?1",
            rusqlite::params![manifest.manifest_id, member.record_json],
        )
        .unwrap(),
        1
    );
    let (proof, startup, root) = evidence(&f, &claim.claim_id);
    // The snapshot includes the payload changed by this second fault injection.
    let before = db.recovery_claim_snapshot(&claim.claim_id).unwrap();
    let error = successor
        .adopt_prepared_claim(proof, startup, &root, &authority)
        .err()
        .expect("post-Commit tampering must be rejected");
    assert_eq!(
        error,
        crate::RuntimeError::Model {
            reason: "durable intent: serialization failed: restart manifest digest changed".into()
        }
    );
    assert!(!successor.has_paused_recovery());
    assert!(successor.list_agents().is_empty());
    assert_eq!(db.recovery_claim_snapshot(&claim.claim_id).unwrap(), before);
    assert_eq!(f.model.calls.load(Ordering::SeqCst), 0);
    assert_eq!(f.model.admission_calls.load(Ordering::SeqCst), 0);
}
