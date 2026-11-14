use super::{fixture::Fixture, *};
use crate::{AgentRuntime, Role, RunConfig, StopScope};
use event_bus::EventBus;
#[path = "recovery_06c_tests.rs"]
mod prepared_adoption;
use std::{collections::BTreeMap, sync::Arc, sync::atomic::Ordering};

async fn saved(f: &Fixture) -> RunId {
    let run = f.start();
    f.runtime.request_quiesce();
    f.runtime.wait_quiesced().await.unwrap();
    let fence = f.runtime.fence_dispatch_restart().await.unwrap();
    f.runtime.drain_dispatch_tails(&fence).await.unwrap();
    let recipe = RecoveryRecipe {
        settings_reference: crate::quiesce::CORE_RECOVERY_SETTINGS_REFERENCE.into(),
        policy: ResumePolicy::CoreOwned,
        goal_id: None,
        team: None,
        lane: None,
        descriptor: None,
    };
    let records = f
        .runtime
        .capture_recovery_records(
            &f.runtime.fence_dispatch_restart().await.unwrap(),
            &BTreeMap::from([(run, recipe)]),
        )
        .unwrap();
    f.runtime
        .save_recovery_graph("paused-test", &records)
        .unwrap();
    run
}
pub(super) fn new_process(f: &Fixture) -> AgentRuntime {
    let bus = Arc::new(EventBus::new(256));
    AgentRuntime::new(
        bus.clone(),
        Arc::new(tools::ToolExecutor::new(bus)),
        f.model.clone(),
    )
    .with_run_store(RunStore::open(&f.config, f._storage.handle()).unwrap())
}
pub(super) fn prepare(f: &Fixture, runtime: &AgentRuntime) -> crate::PausedRecoveryComponent {
    let root = f._dir.path().join("ownership");
    let leases =
        storage::shared_with_wait(&f.config.db_path, &root, std::time::Duration::ZERO).unwrap();
    let startup = leases
        .recovery_startup_complete(&f.config.db_path, &root)
        .unwrap();
    let plan = runtime.recovery_plan().unwrap();
    assert_eq!(plan.components.len(), 1, "{plan:?}");
    runtime
        .prepare_component(&plan.components[0], startup, &root)
        .unwrap()
}

fn adopt_for_stop(
    f: &Fixture,
    runtime: &AgentRuntime,
    paused: crate::PausedRecoveryComponent,
) -> (crate::ownership::OwnerHost, crate::PausedRecoveryComponent) {
    let run = paused.claim().members[0].run_id.parse().unwrap();
    let (owner, authority) = prepared_adoption::authority(f, run);
    let (proof, startup, root) = prepared_adoption::evidence(f, &paused.claim().claim_id);
    let adopted = runtime
        .adopt_prepared_claim(proof, startup, &root, &authority)
        .unwrap();
    (owner, adopted)
}

#[tokio::test]
async fn recovery_rejects_store_attached_after_a_message_was_issued() {
    let f = Fixture::new(0);
    let run = saved(&f).await;
    let bus = Arc::new(EventBus::new(64));
    let late = AgentRuntime::new(
        bus.clone(),
        Arc::new(tools::ToolExecutor::new(bus)),
        f.model.clone(),
    );
    let root = late.delegate_background(Role::Worker, "fresh".into(), RunConfig::default());
    let child = late
        .delegate_background_as_child(root, Role::Worker, "child", RunConfig::default())
        .unwrap();
    late.send_agent_message(
        root,
        child,
        event_bus::AgentMessageKind::Send,
        "early",
        None,
    )
    .unwrap();
    let late = late.with_run_store(RunStore::open(&f.config, f._storage.handle()).unwrap());
    let ownership = f._dir.path().join("ownership");
    let leases =
        storage::shared_with_wait(&f.config.db_path, &ownership, std::time::Duration::ZERO)
            .unwrap();
    let startup = leases
        .recovery_startup_complete(&f.config.db_path, &ownership)
        .unwrap();
    let plan = late.recovery_plan().unwrap();
    assert_eq!(plan.components[0].runs, [run]);
    assert!(
        late.prepare_component(&plan.components[0], startup, &ownership)
            .is_err()
    );
    assert!(
        storage::Database::open(&f.config)
            .unwrap()
            .recovery_claims()
            .unwrap()
            .is_empty()
    );
    late.cancel(root).unwrap();
}

#[tokio::test]
async fn startup_fences_expired_message_ids_before_sends_and_delayed_components() {
    let f = Fixture::new(0);
    let root = f.start();
    let child = f
        .runtime
        .delegate_background_as_child(root, Role::Worker, "child", RunConfig::default())
        .unwrap();
    let other = f.start();
    let recipe = RecoveryRecipe {
        settings_reference: crate::quiesce::CORE_RECOVERY_SETTINGS_REFERENCE.into(),
        policy: ResumePolicy::CoreOwned,
        goal_id: None,
        team: None,
        lane: None,
        descriptor: None,
    };
    let mut expired = Vec::new();
    for _ in 0..2 {
        let id = f
            .runtime
            .send_agent_message(
                root,
                child,
                event_bus::AgentMessageKind::Send,
                "expired request",
                None,
            )
            .unwrap();
        assert_eq!(f.runtime.take_inbox(child).unwrap().len(), 1);
        // The production recovered-timeout branch removes the sent receipt.
        assert!(matches!(
            f.runtime.expire_recovery_reply_wait(&id),
            crate::RuntimeError::ReplyTimeout { .. }
        ));
        expired.push(id);
    }
    f.runtime.request_quiesce();
    f.runtime.wait_quiesced().await.unwrap();
    let fence = f.runtime.fence_dispatch_restart().await.unwrap();
    f.runtime.drain_dispatch_tails(&fence).await.unwrap();
    let records = f
        .runtime
        .capture_recovery_records(
            &f.runtime.fence_dispatch_restart().await.unwrap(),
            &BTreeMap::from([(root, recipe.clone()), (other, recipe)]),
        )
        .unwrap();
    assert!(records.iter().all(|r| r.deliveries.is_empty()));
    assert!(records.iter().all(|r| r.next_message_id > 2));
    f.runtime
        .save_recovery_graph("expired-ids", &records)
        .unwrap();

    let runtime = new_process(&f);
    // A new run can send before *any* recovery component is prepared.
    let fresh = runtime.delegate_background(Role::Worker, "fresh".into(), RunConfig::default());
    let fresh_child = runtime
        .delegate_background_as_child(fresh, Role::Worker, "fresh child", RunConfig::default())
        .unwrap();
    let first = runtime
        .send_agent_message(
            fresh,
            fresh_child,
            event_bus::AgentMessageKind::Send,
            "before activation",
            None,
        )
        .unwrap();
    assert!(!expired.contains(&first));
    let plan = runtime.recovery_plan().unwrap();
    assert_eq!(plan.components.len(), 2);
    let ownership = f._dir.path().join("ownership");
    let leases =
        storage::shared_with_wait(&f.config.db_path, &ownership, std::time::Duration::ZERO)
            .unwrap();
    let owner = crate::ownership::OwnerHost::open(
        &f._dir.path().join("current-owner"),
        Default::default(),
        Arc::new(EventBus::new(64)),
    )
    .unwrap();
    for component in &plan.components {
        let startup = leases
            .recovery_startup_complete(&f.config.db_path, &ownership)
            .unwrap();
        let mut paused = runtime
            .prepare_component(component, startup, &ownership)
            .unwrap();
        runtime
            .restore_relations_and_continuations(&mut paused)
            .unwrap();
        runtime
            .activate_component(
                paused,
                crate::CurrentRecoveryAuthority {
                    members: component
                        .runs
                        .iter()
                        .map(|id| {
                            (
                                *id,
                                RunConfig {
                                    ownership: Some(owner.start(&id.to_string()).unwrap()),
                                    interactive: *id != child,
                                    keep_alive: *id != child,
                                    ..Default::default()
                                },
                            )
                        })
                        .collect(),
                },
            )
            .unwrap();
        let between = runtime
            .send_agent_message(
                fresh,
                fresh_child,
                event_bus::AgentMessageKind::Send,
                "between component activations",
                None,
            )
            .unwrap();
        assert!(!expired.contains(&between));
    }
    let current = runtime
        .send_agent_message(
            root,
            child,
            event_bus::AgentMessageKind::Send,
            "new request",
            None,
        )
        .unwrap();
    assert!(!expired.contains(&current));
    assert!(
        runtime
            .send_agent_message(
                child,
                root,
                event_bus::AgentMessageKind::Reply,
                "late old reply",
                Some(expired[1].clone()),
            )
            .is_err()
    );
    runtime.check_reply_wait(root, &current).unwrap();
    for run in [root, other, fresh] {
        runtime.cancel(run).unwrap();
    }
}

#[tokio::test]
async fn prepare_and_drop_leave_exact_snapshot_and_pending_claim_without_execution() {
    let f = Fixture::new(0);
    let run = saved(&f).await;
    let runtime = new_process(&f);
    let db = storage::Database::open(&f.config).unwrap();
    let before = db.run_context(&run.to_string()).unwrap().unwrap();
    let revision = db
        .resume_intent_revision(&run.to_string())
        .unwrap()
        .unwrap();
    let paused = prepare(&f, &runtime);
    let (_owner, paused) = adopt_for_stop(&f, &runtime, paused);
    assert!(runtime.has_paused_recovery());
    assert!(runtime.list_agents().is_empty());
    assert_eq!(f.model.calls.load(Ordering::SeqCst), 0);
    let after = db.run_context(&run.to_string()).unwrap().unwrap();
    assert_eq!(after.config_json, before.config_json);
    assert_eq!(after.messages_json, before.messages_json);
    assert_eq!(after.updated_at_ns, before.updated_at_ns);
    assert_eq!(
        db.resume_intent_revision(&run.to_string()).unwrap(),
        Some(revision + 1)
    );
    drop(paused);
    assert!(runtime.has_paused_recovery());
    assert_eq!(
        db.run_context(&run.to_string())
            .unwrap()
            .unwrap()
            .config_json,
        before.config_json
    );
    let plan = new_process(&f).recovery_plan().unwrap();
    assert!(plan.components.is_empty());
    assert!(
        plan.report
            .blocked
            .iter()
            .any(|b| b.reason == RecoveryReason::PendingClaim)
    );
    let fence = runtime
        .fence_dispatch_shutdown(|| async { Ok::<(), String>(()) })
        .await
        .unwrap();
    runtime.drain_dispatch_tails(&fence).await.unwrap();
    assert!(!runtime.has_paused_recovery());
    assert_eq!(
        db.recovery_claims().unwrap()[0].state,
        RecoveryClaimState::Released {
            reason: "shutdown".into()
        }
    );
}

#[tokio::test]
async fn paused_stop_cancel_and_subtree_cancel_fence_the_old_incarnation() {
    for mode in 0..3 {
        let f = Fixture::new(0);
        let run = saved(&f).await;
        let runtime = new_process(&f);
        let db = storage::Database::open(&f.config).unwrap();
        let paused = prepare(&f, &runtime);
        let (_owner, paused) = adopt_for_stop(&f, &runtime, paused);
        let old = paused.claim().members[0].old_incarnation.clone();
        let revision = paused.claim().members[0].reserved_revision;
        let reason = if mode == 0 {
            StopReason::OperatorStopped
        } else {
            StopReason::Cancelled
        };
        match mode {
            0 => runtime.stop(run, StopScope::SelfOnly).unwrap(),
            1 => runtime.cancel(run).unwrap(),
            _ => runtime.cancel_subtree(run).unwrap(),
        }
        let intent =
            ResumeIntent::decode(db.resume_intent(&run.to_string()).unwrap().unwrap()).unwrap();
        assert_eq!(intent.incarnation, old);
        assert_eq!(intent.stop_reason, Some(reason));
        assert!(!intent.active);
        assert!(
            db.resume_intent_revision(&run.to_string())
                .unwrap()
                .unwrap()
                > revision
        );
        let expected = if mode == 0 {
            RecoveryReason::OperatorStopped
        } else {
            RecoveryReason::Cancelled
        };
        let plan = new_process(&f).recovery_plan().unwrap();
        assert!(plan.components.is_empty());
        assert!(
            plan.report.skipped.iter().any(|s| s.reason == expected),
            "{plan:?}"
        );
        assert!(
            !plan
                .report
                .blocked
                .iter()
                .any(|s| s.reason == RecoveryReason::IntentRevisionConflict)
        );
        runtime
            .release_prepared_component(&paused, "operator aborted recovery")
            .unwrap();
        assert!(!runtime.has_paused_recovery());
        assert!(!runtime.shared.quiesce.lock().intents.contains_key(&run));
        assert_eq!(f.model.calls.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn released_claim_never_rolls_back_revision_or_blocks_manual_registration_in_memory() {
    let f = Fixture::new(0);
    let run = saved(&f).await;
    let runtime = new_process(&f);
    let paused = prepare(&f, &runtime);
    runtime.spawn_reserved(run, None, Role::Worker, "not allowed", RunConfig::default());
    assert!(runtime.list_agents().is_empty());
    let revision = paused.claim().members[0].reserved_revision;
    assert!(runtime.release_prepared_component(&paused, "").is_err());
    assert!(runtime.has_paused_recovery());
    assert_eq!(
        storage::Database::open(&f.config)
            .unwrap()
            .recovery_claims()
            .unwrap()[0]
            .state,
        RecoveryClaimState::Prepared
    );
    runtime
        .release_prepared_component(&paused, "retry manually")
        .unwrap();
    let db = storage::Database::open(&f.config).unwrap();
    assert_eq!(
        db.resume_intent_revision(&run.to_string()).unwrap(),
        Some(revision)
    );
    assert!(!runtime.has_paused_recovery());
    assert!(!runtime.shared.quiesce.lock().intents.contains_key(&run));
    let plan = runtime.recovery_plan().unwrap();
    assert!(plan.components.is_empty());
    assert!(
        plan.report
            .skipped
            .iter()
            .any(|s| s.reason == RecoveryReason::IntentRevisionConflict)
    );
    // Do not run the old task in the original process after its snapshot was
    // deliberately claimed by this simulated new process.
}

#[tokio::test]
async fn activation_history_is_skipped_and_adopt_does_not_write_the_new_intent_twice() {
    let f = Fixture::new(0);
    let run = saved(&f).await;
    let runtime = new_process(&f);
    let paused = prepare(&f, &runtime);
    let db = storage::Database::open(&f.config).unwrap();
    let claim = paused.claim().clone();
    let mut intent =
        ResumeIntent::decode(db.resume_intent(&run.to_string()).unwrap().unwrap()).unwrap();
    let manifest = db.recovery_manifests().unwrap().remove(0).record.unwrap();
    let record = db.run_context(&run.to_string()).unwrap().unwrap();
    intent.incarnation = claim.members[0].new_incarnation.clone();
    let before = claim.members[0].reserved_revision;
    f._storage
        .handle()
        .activate_recovery_claim(storage::recovery::RecoveryActivationRequest {
            claim_id: claim.claim_id.clone(),
            intents: vec![storage::resume_intent::ResumeIntentRecord {
                run_id: run.to_string(),
                incarnation: intent.incarnation.clone(),
                payload: serde_json::to_string(&intent).unwrap(),
            }],
            snapshots: vec![storage::recovery::RecoverySnapshotExpectation {
                run_id: run.to_string(),
                receipt: manifest.members[0].snapshot.clone().unwrap(),
                config_json: record.config_json.clone(),
            }],
        })
        .unwrap();
    assert_eq!(
        db.resume_intent_revision(&run.to_string()).unwrap(),
        Some(before + 1)
    );
    let plan = new_process(&f).recovery_plan().unwrap();
    assert!(plan.components.is_empty());
    assert!(plan.report.blocked.is_empty(), "{plan:?}");
    assert!(
        plan.report
            .skipped
            .iter()
            .any(|s| s.reason == RecoveryReason::SupersededManifest)
    );
    assert!(
        plan.report
            .skipped
            .iter()
            .any(|s| s.reason == RecoveryReason::NoCommittedManifest)
    );
    assert!(
        runtime
            .release_prepared_component(&paused, "not permitted after activation")
            .is_err()
    );
    assert!(runtime.has_paused_recovery());
    let coordinator = Coordinator::default();
    coordinator.adopt([intent.clone()], true);
    assert_eq!(
        db.resume_intent_revision(&run.to_string()).unwrap(),
        Some(before + 1)
    );
    coordinator
        .update(
            run,
            &intent.incarnation,
            runtime.shared.run_store.get(),
            |next| {
                next.boundary_pending = !next.boundary_pending;
            },
        )
        .unwrap();
    assert_eq!(
        db.resume_intent_revision(&run.to_string()).unwrap(),
        Some(before + 2)
    );
    assert_eq!(
        db.run_context(&run.to_string())
            .unwrap()
            .unwrap()
            .config_json,
        record.config_json
    );
    assert_eq!(f.model.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn planned_restart_does_not_release_or_publish_a_paused_component() {
    let f = Fixture::new(0);
    let run = saved(&f).await;
    let runtime = new_process(&f);
    let paused = prepare(&f, &runtime);
    let fence = runtime.fence_dispatch_restart().await.unwrap();
    runtime.drain_dispatch_tails(&fence).await.unwrap();
    let db = storage::Database::open(&f.config).unwrap();
    assert_eq!(
        db.recovery_claims().unwrap()[0].state,
        RecoveryClaimState::Prepared
    );
    assert!(runtime.has_paused_recovery());
    assert!(runtime.list_agents().is_empty());
    assert_eq!(
        db.resume_intent_revision(&run.to_string()).unwrap(),
        Some(paused.claim().members[0].reserved_revision)
    );
    assert_eq!(f.model.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn activation_commits_once_and_publishes_the_whole_family_before_starting() {
    let f = Fixture::new(0);
    let root = f.runtime.delegate_background(
        Role::Orchestrator,
        "work".into(),
        RunConfig {
            interactive: true,
            keep_alive: true,
            spawn_credits: serde_json::from_value(serde_json::json!({
                "spawned":["old-child"],"observed":[],"unobserved":3
            }))
            .unwrap(),
            ..Default::default()
        },
    );
    let child = f
        .runtime
        .delegate_background_as_child(
            root,
            Role::Worker,
            "child",
            RunConfig {
                interactive: true,
                keep_alive: true,
                ..Default::default()
            },
        )
        .unwrap();
    let old_messages: Vec<_> = (0..8)
        .map(|_| {
            f.runtime
                .send_agent_message(
                    root,
                    child,
                    event_bus::AgentMessageKind::Send,
                    "request",
                    None,
                )
                .unwrap()
        })
        .collect();
    let original_message = old_messages.last().unwrap().clone();
    let question = f
        .runtime
        .request_user_question(child, "blocking recovery question".into(), vec![], true)
        .unwrap();
    f.runtime.request_quiesce();
    f.runtime.wait_quiesced().await.unwrap();
    let fence = f.runtime.fence_dispatch_restart().await.unwrap();
    f.runtime.drain_dispatch_tails(&fence).await.unwrap();
    let records = f
        .runtime
        .capture_recovery_records(
            &f.runtime.fence_dispatch_restart().await.unwrap(),
            &BTreeMap::from([(
                root,
                RecoveryRecipe {
                    settings_reference: crate::quiesce::CORE_RECOVERY_SETTINGS_REFERENCE.into(),
                    policy: ResumePolicy::CoreOwned,
                    goal_id: None,
                    team: None,
                    lane: None,
                    descriptor: None,
                },
            )]),
        )
        .unwrap();
    let question_delivery = records
        .iter()
        .find(|r| r.run_id == root)
        .unwrap()
        .deliveries
        .iter()
        .find(|d| matches!(d.notice, DurableNotice::Question { .. }))
        .unwrap();
    assert_eq!(question_delivery.sender, child);
    assert_eq!(
        question_delivery.message.as_ref().unwrap().sender_run_id,
        "runtime"
    );
    let saved_credits = records
        .iter()
        .find(|r| r.run_id == root)
        .unwrap()
        .recipe
        .descriptor
        .as_ref()
        .unwrap()
        .spawn_credits
        .clone();
    f.runtime
        .save_recovery_graph("activate-family", &records)
        .unwrap();
    let runtime = new_process(&f);
    let mut paused = prepare(&f, &runtime);
    let claim = paused.claim().clone();
    runtime
        .restore_relations_and_continuations(&mut paused)
        .unwrap();
    let owner = crate::ownership::OwnerHost::open(
        &f._dir.path().join("current-owner"),
        Default::default(),
        Arc::new(EventBus::new(64)),
    )
    .unwrap();
    let permit = owner.start("current").unwrap();
    let authority = crate::CurrentRecoveryAuthority {
        members: [root, child]
            .into_iter()
            .map(|id| {
                (
                    id,
                    RunConfig {
                        ownership: Some(permit.clone()),
                        interactive: true,
                        keep_alive: true,
                        ..Default::default()
                    },
                )
            })
            .collect(),
    };
    let execution = runtime.execution_snapshot(root).unwrap();
    let observed = runtime.clone();
    let model = f.model.clone();
    runtime.on_execution(
        crate::runtime::HookPoint::RecoveryActivated,
        &execution,
        move || {
            assert!(observed.list_agents().is_empty());
            assert_eq!(model.calls.load(Ordering::SeqCst), 0);
        },
    );
    let observed = runtime.clone();
    let model = f.model.clone();
    runtime.on_execution(
        crate::runtime::HookPoint::RecoveryPublished,
        &execution,
        move || {
            assert_eq!(observed.list_agents().len(), 2);
            assert_eq!(model.calls.load(Ordering::SeqCst), 0);
        },
    );
    assert_eq!(
        runtime.activate_component(paused, authority).unwrap().len(),
        2
    );
    assert!(!runtime.has_paused_recovery());
    assert_eq!(
        serde_json::to_value(
            runtime
                .shared
                .runs
                .lock()
                .unwrap()
                .get(&root)
                .unwrap()
                .config
                .spawn_credits
                .clone()
        )
        .unwrap(),
        serde_json::to_value(saved_credits).unwrap()
    );
    let new_message = runtime
        .send_agent_message(
            root,
            child,
            event_bus::AgentMessageKind::Send,
            "after recovery",
            None,
        )
        .unwrap();
    assert!(
        !old_messages.contains(&new_message),
        "recovery must reserve all delivery IDs"
    );
    assert_eq!(
        runtime.reply_wait_target(root, &original_message).unwrap(),
        child
    );
    assert_eq!(
        runtime.reply_wait_target(root, &new_message).unwrap(),
        child
    );
    assert!(
        runtime
            .check_reply_wait(child, &question_delivery.message_id)
            .is_err()
    );
    runtime
        .answer_user_question(&question.id, "continue")
        .unwrap();
    let db = storage::Database::open(&f.config).unwrap();
    for member in &claim.members {
        assert_eq!(
            db.resume_intent(&member.run_id)
                .unwrap()
                .unwrap()
                .incarnation,
            member.new_incarnation
        );
        assert_eq!(
            db.resume_intent_revision(&member.run_id).unwrap(),
            Some(member.reserved_revision + 1)
        );
    }
    assert_eq!(
        db.recovery_claims().unwrap()[0].state,
        RecoveryClaimState::Activated
    );
    while f.model.calls.load(Ordering::SeqCst) < 2 {
        f.model.entered.notified().await;
    }
    runtime.cancel_subtree(root).unwrap();
    runtime.wait(root).await.unwrap();
    runtime.wait(child).await.unwrap();
}

#[tokio::test]
async fn activation_failure_after_ack_never_releases_or_replays_the_component() {
    post_activation_fault(crate::runtime::HookPoint::RecoveryActivated, false).await;
}

#[tokio::test]
async fn activation_crash_after_publication_keeps_gate_closed_and_forbids_reactivation() {
    post_activation_fault(crate::runtime::HookPoint::RecoveryPublished, true).await;
}

#[tokio::test]
async fn activation_crash_after_gate_open_never_turns_into_a_prepared_retry() {
    post_activation_fault(crate::runtime::HookPoint::RecoveryGateOpened, true).await;
}

async fn post_activation_fault(point: crate::runtime::HookPoint, published: bool) {
    let f = Fixture::new(0);
    let run = saved(&f).await;
    let runtime = new_process(&f);
    let mut paused = prepare(&f, &runtime);
    runtime
        .restore_relations_and_continuations(&mut paused)
        .unwrap();
    let owner = crate::ownership::OwnerHost::open(
        &f._dir.path().join("current-owner"),
        Default::default(),
        Arc::new(EventBus::new(64)),
    )
    .unwrap();
    let authority = crate::CurrentRecoveryAuthority {
        members: BTreeMap::from([(
            run,
            RunConfig {
                ownership: Some(owner.start("current").unwrap()),
                interactive: true,
                keep_alive: true,
                ..Default::default()
            },
        )]),
    };
    let execution = runtime.execution_snapshot(run).unwrap();
    runtime.on_execution(point, &execution, || {
        panic!("fault at the selected post-ACK checkpoint");
    });
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            runtime.activate_component(paused, authority)
        }))
        .is_err()
    );
    assert_eq!(runtime.list_agents().len(), usize::from(published));
    assert_eq!(f.model.calls.load(Ordering::SeqCst), 0);
    let reopened = new_process(&f);
    let plan = reopened.recovery_plan().unwrap();
    assert!(plan.components.is_empty());
    assert!(
        plan.report
            .skipped
            .iter()
            .any(|s| s.reason == RecoveryReason::SupersededManifest)
    );
    let db = storage::Database::open(&f.config).unwrap();
    let claim = &db.recovery_claims().unwrap()[0];
    assert_eq!(claim.state, RecoveryClaimState::Activated);
    assert!(
        f._storage
            .handle()
            .release_recovery_claim(claim.claim_id.clone(), "retry".into())
            .is_err()
    );
}

#[tokio::test]
async fn released_same_process_continue_commits_input_snapshot_and_new_intent_once() {
    let f = Fixture::new(0);
    let run = saved(&f).await;
    let runtime = new_process(&f);
    let paused = prepare(&f, &runtime);
    let old = paused.claim().members[0].old_incarnation.clone();
    let revision = paused.claim().members[0].reserved_revision;
    runtime
        .release_prepared_component(&paused, "manual Continue")
        .unwrap();
    let db = storage::Database::open(&f.config).unwrap();
    assert_eq!(
        db.resume_intent_revision(&run.to_string()).unwrap(),
        Some(revision)
    );
    let result = runtime.continue_goal(
        run,
        "human input after release".into(),
        RunConfig::default(),
    );
    assert_eq!(result.unwrap(), run);
    let intent =
        ResumeIntent::decode(db.resume_intent(&run.to_string()).unwrap().unwrap()).unwrap();
    assert_ne!(intent.incarnation, old);
    assert!(intent.active);
    assert_eq!(
        db.resume_intent_revision(&run.to_string()).unwrap(),
        Some(revision + 1)
    );
    let record = db.run_context(&run.to_string()).unwrap().unwrap();
    assert!(record.messages_json.contains("human input after release"));
    assert!(!runtime.has_paused_recovery());
    runtime.cancel(run).unwrap();
    runtime.wait(run).await.unwrap();
}

#[tokio::test]
async fn recovery_provider_admission_is_also_behind_the_component_gate() {
    let f = Fixture::with_admission(0, true);
    let run = f.start();
    f.model.admission_entered.notified().await;
    f.runtime.request_quiesce();
    // Let the old admission acknowledge AdmissionQueued under quiesce.
    f.model.admission_release.add_permits(1);
    f.runtime.wait_quiesced().await.unwrap();
    let fence = f.runtime.fence_dispatch_restart().await.unwrap();
    f.runtime.drain_dispatch_tails(&fence).await.unwrap();
    let records = f
        .runtime
        .capture_recovery_records(
            &f.runtime.fence_dispatch_restart().await.unwrap(),
            &BTreeMap::from([(
                run,
                RecoveryRecipe {
                    settings_reference: crate::quiesce::CORE_RECOVERY_SETTINGS_REFERENCE.into(),
                    policy: ResumePolicy::CoreOwned,
                    goal_id: None,
                    team: None,
                    lane: None,
                    descriptor: None,
                },
            )]),
        )
        .unwrap();
    f.runtime
        .save_recovery_graph("admission-gate", &records)
        .unwrap();
    let calls_before = f.model.admission_calls.load(Ordering::SeqCst);
    let runtime = new_process(&f);
    let mut paused = prepare(&f, &runtime);
    runtime
        .restore_relations_and_continuations(&mut paused)
        .unwrap();
    let owner = crate::ownership::OwnerHost::open(
        &f._dir.path().join("current-owner"),
        Default::default(),
        Arc::new(EventBus::new(64)),
    )
    .unwrap();
    let execution = runtime.execution_snapshot(run).unwrap();
    let model = f.model.clone();
    runtime.on_execution(
        crate::runtime::HookPoint::RecoveryPublished,
        &execution,
        move || {
            assert_eq!(model.admission_calls.load(Ordering::SeqCst), calls_before);
            assert_eq!(model.calls.load(Ordering::SeqCst), 0);
        },
    );
    runtime
        .activate_component(
            paused,
            crate::CurrentRecoveryAuthority {
                members: BTreeMap::from([(
                    run,
                    RunConfig {
                        ownership: Some(owner.start("current").unwrap()),
                        interactive: true,
                        keep_alive: true,
                        ..Default::default()
                    },
                )]),
            },
        )
        .unwrap();
    while f.model.admission_calls.load(Ordering::SeqCst) == calls_before {
        f.model.admission_entered.notified().await;
    }
    assert_eq!(f.model.calls.load(Ordering::SeqCst), 0);
    runtime.cancel(run).unwrap();
    runtime.wait(run).await.unwrap();
}
