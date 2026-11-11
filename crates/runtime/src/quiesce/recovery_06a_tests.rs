use super::fixture::Fixture;
use super::*;
use crate::{Role, RunConfig};
use std::{collections::BTreeMap, sync::atomic::Ordering};

fn recipe(policy: ResumePolicy) -> RecoveryRecipe {
    RecoveryRecipe {
        settings_reference: "current-host-profile".into(),
        policy,
        goal_id: Some("goal".into()),
        team: None,
        lane: None,
        descriptor: None,
    }
}
async fn capture(f: &Fixture, recipes: &BTreeMap<RunId, RecoveryRecipe>) -> Vec<RunRecoveryRecord> {
    let fence = f.runtime.fence_dispatch_restart().await.unwrap();
    f.runtime.drain_dispatch_tails(&fence).await.unwrap();
    f.runtime.capture_recovery_records(recipes).unwrap()
}

#[tokio::test]
async fn core_owned_roundtrip_keeps_provider_prefix_without_activating() {
    let mut f = Fixture::new(1);
    let run = f.start();
    f.model.entered.notified().await;
    let wire = serde_json::to_value(f.model.input.lock().unwrap().as_ref().unwrap()).unwrap();
    f.runtime.request_quiesce();
    f.model.release.add_permits(1);
    f.entered.recv().await.unwrap();
    f.release[0].add_permits(1);
    f.completed.recv().await.unwrap();
    let records = capture(
        &f,
        &BTreeMap::from([(run, recipe(ResumePolicy::CoreOwned))]),
    )
    .await;
    let count = f.model.calls.load(Ordering::SeqCst);
    let ids = f
        .runtime
        .save_recovery_graph("roundtrip", &records)
        .unwrap();
    assert_eq!(ids.len(), 1);
    let plan = f.runtime.recovery_plan().unwrap();
    assert_eq!(plan.components.len(), 1);
    assert_eq!(plan.components[0].runs, vec![run]);
    assert!(plan.report.recovered.is_empty());
    assert_eq!(
        serde_json::to_value(f.intent(run).provider_input.unwrap()).unwrap(),
        wire
    );
    assert_eq!(f.model.calls.load(Ordering::SeqCst), count);
    let loads = storage::Database::open(&f.config)
        .unwrap()
        .recovery_manifests()
        .unwrap();
    assert!(
        !loads[0].record.as_ref().unwrap().members[0]
            .record_json
            .contains("provider_input")
    );
    assert!(!serde_json::to_string(&plan).unwrap().contains("request"));
    f.cleanup(run).await;
}

#[tokio::test]
async fn root_policy_is_inherited_and_unfinished_wait_is_blocked_in_06a() {
    let f = Fixture::new(0);
    let root = f.runtime.delegate_background(
        Role::Orchestrator,
        "root".into(),
        RunConfig {
            keep_alive: true,
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
                keep_alive: true,
                ..Default::default()
            },
        )
        .unwrap();
    let mut records = capture(
        &f,
        &BTreeMap::from([(root, recipe(ResumePolicy::CoreOwned))]),
    )
    .await;
    assert!(
        records
            .iter()
            .all(|r| r.recipe.policy == ResumePolicy::CoreOwned)
    );
    records
        .iter_mut()
        .find(|r| r.run_id == root)
        .unwrap()
        .waits
        .push(DurableWait {
            requester: root,
            targets: vec![child],
            mode: DurableWaitMode::All,
            tool_call_id: "original-wait".into(),
            completions: vec![],
            questions: vec![ObservedGeneration {
                id: "question".into(),
                generation: 2,
            }],
            answers: vec![],
            continuation: WaitContinuation {
                tool_name: "wait".into(),
                legacy_result: false,
                consume_mailbox: true,
            },
        });
    assert_eq!(
        f.runtime
            .save_recovery_graph("wait", &records)
            .unwrap()
            .len(),
        1
    );
    let plan = f.runtime.recovery_plan().unwrap();
    assert!(plan.components.is_empty());
    assert!(
        plan.report
            .blocked
            .iter()
            .any(|b| b.reason == RecoveryReason::UnfinishedWaitUnsupportedIn06a)
    );
    f.cleanup(root).await;
    f.cleanup(child).await;
}

#[tokio::test]
async fn unknown_ownership_cli_and_orphan_intents_never_become_candidates() {
    for policy in [
        ResumePolicy::Unsupported,
        ResumePolicy::CliInvocation {
            invocation_id: "invocation".into(),
            planned_handoff: Some("handoff-id".into()),
        },
    ] {
        let f = Fixture::new(0);
        let run = f.start();
        let records = capture(&f, &BTreeMap::from([(run, recipe(policy.clone()))])).await;
        let before = f.runtime.recovery_plan().unwrap();
        assert!(before.components.is_empty());
        assert!(
            before
                .report
                .skipped
                .iter()
                .any(|s| s.reason == RecoveryReason::NoCommittedManifest)
        );
        f.runtime.save_recovery_graph("policy", &records).unwrap();
        let plan = f.runtime.recovery_plan().unwrap();
        assert!(plan.components.is_empty());
        let expected = if policy == ResumePolicy::Unsupported {
            RecoveryReason::UnknownOwnership
        } else {
            RecoveryReason::CliContinuityRequired
        };
        assert!(plan.report.blocked.iter().any(|b| b.reason == expected));
        f.cleanup(run).await;
    }
}

#[tokio::test]
async fn prepared_claim_after_reopen_is_not_misdecoded_as_a_new_incarnation() {
    let f = Fixture::new(0);
    let run = f.start();
    let records = capture(
        &f,
        &BTreeMap::from([(run, recipe(ResumePolicy::CoreOwned))]),
    )
    .await;
    let ids = f.runtime.save_recovery_graph("claim", &records).unwrap();
    let before = f.intent(run);
    f._storage
        .handle()
        .claim_recovery_manifest(storage::recovery::RecoveryClaimRequest {
            claim_id: "prepared".into(),
            manifest_id: ids[0].clone(),
            incarnations: vec![(run.to_string(), "next-incarnation".into())],
        })
        .unwrap();
    assert_eq!(f.intent(run), before);
    f._storage.close();
    let plan = f.runtime.recovery_plan().unwrap();
    assert!(plan.components.is_empty());
    assert!(
        plan.report
            .blocked
            .iter()
            .any(|b| b.reason == RecoveryReason::PendingClaim)
    );
}

#[tokio::test]
async fn file_backed_reopen_isolates_bad_member_payload_attributes_edges_and_intent() {
    for damage in ["member", "attribute", "edge", "intent", "claim"] {
        let f = Fixture::new(0);
        let bad = f.start();
        let healthy = f.start();
        let records = capture(
            &f,
            &BTreeMap::from([
                (bad, recipe(ResumePolicy::CoreOwned)),
                (healthy, recipe(ResumePolicy::CoreOwned)),
            ]),
        )
        .await;
        f.runtime
            .save_recovery_graph("corruption", &records)
            .unwrap();
        if damage == "claim" {
            let manifests = storage::Database::open(&f.config)
                .unwrap()
                .recovery_manifests()
                .unwrap();
            let manifest_id = manifests
                .iter()
                .find(|m| {
                    m.record
                        .as_ref()
                        .unwrap()
                        .members
                        .iter()
                        .any(|r| r.run_id == bad.to_string())
                })
                .unwrap()
                .manifest_id
                .clone();
            f._storage
                .handle()
                .claim_recovery_manifest(storage::recovery::RecoveryClaimRequest {
                    claim_id: "damaged-claim".into(),
                    manifest_id,
                    incarnations: vec![(bad.to_string(), "new".into())],
                })
                .unwrap();
        }
        f._storage.close();
        let sql = rusqlite::Connection::open(&f.config.db_path).unwrap();
        match damage {
            "claim" => {
                sql.execute(
                    "UPDATE recovery_claim_members SET reserved_revision='broken' WHERE run_id=?1",
                    [bad.to_string()],
                )
                .unwrap();
            }
            "member" => {
                sql.execute(
                    "UPDATE recovery_members SET record_json='broken' WHERE run_id=?1",
                    [bad.to_string()],
                )
                .unwrap();
            }
            "attribute" => {
                sql.execute(
                    "UPDATE recovery_members SET intent_revision='broken' WHERE run_id=?1",
                    [bad.to_string()],
                )
                .unwrap();
            }
            "edge" => {
                sql.execute("UPDATE recovery_manifests SET edge_count=1 WHERE manifest_id IN(SELECT manifest_id FROM recovery_members WHERE run_id=?1)", [bad.to_string()]).unwrap();
            }
            _ => {
                sql.execute(
                    "UPDATE resume_intents SET payload='broken' WHERE run_id=?1",
                    [bad.to_string()],
                )
                .unwrap();
            }
        }
        drop(sql);
        let plan = f.runtime.recovery_plan().unwrap();
        assert_eq!(plan.components.len(), 1, "{damage}: {plan:?}");
        assert_eq!(plan.components[0].runs, vec![healthy]);
        assert!(!plan.report.blocked.is_empty());
        if damage == "intent" {
            let captured = f
                .runtime
                .capture_recovery_records(&BTreeMap::new())
                .unwrap();
            assert!(captured.iter().any(|r| r.run_id == healthy));
            assert!(!captured.iter().any(|r| r.run_id == bad));
        }
    }
}

#[tokio::test]
async fn capture_keeps_mailbox_consumed_receipts_and_user_reset_budget() {
    let f = Fixture::new(0);
    let root = f.runtime.delegate_background(
        Role::Orchestrator,
        "root".into(),
        RunConfig {
            keep_alive: true,
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
                keep_alive: true,
                ..Default::default()
            },
        )
        .unwrap();
    let consumed = f
        .runtime
        .send_agent_message(
            root,
            child,
            event_bus::AgentMessageKind::Send,
            "consumed",
            None,
        )
        .unwrap();
    f.runtime.take_inbox(child).unwrap();
    let held_id = f
        .runtime
        .send_agent_message(root, child, event_bus::AgentMessageKind::Send, "held", None)
        .unwrap();
    let held = f.runtime.take_inbox(child).unwrap().pop().unwrap();
    let queued = f
        .runtime
        .send_agent_message(
            root,
            child,
            event_bus::AgentMessageKind::Send,
            "queued",
            None,
        )
        .unwrap();
    f.runtime.quiesce().await.unwrap();
    f.runtime
        .shared
        .held_messages
        .lock()
        .unwrap()
        .insert(child, vec![crate::mailbox::QueuedMessage::explicit(held)]);
    f.runtime.reset_auto_wakes(root);
    let records = capture(
        &f,
        &BTreeMap::from([(root, recipe(ResumePolicy::CoreOwned))]),
    )
    .await;
    let child_record = records.iter().find(|r| r.run_id == child).unwrap();
    assert!(
        child_record
            .deliveries
            .iter()
            .any(|d| d.message_id == consumed
                && d.state == DurableDeliveryState::Consumed
                && d.consumed_receipt.is_some())
    );
    assert!(
        child_record
            .deliveries
            .iter()
            .any(|d| d.message_id == queued && d.state == DurableDeliveryState::Mailbox)
    );
    assert!(
        records
            .iter()
            .all(|r| r.user_reset_generation == 1 && r.auto_wakes == 0)
    );
    assert!(
        child_record
            .deliveries
            .iter()
            .any(|d| d.message_id == held_id && d.state == DurableDeliveryState::Held)
    );
    assert_eq!(
        f.runtime
            .shared
            .held_messages
            .lock()
            .unwrap()
            .get(&child)
            .unwrap()
            .len(),
        1
    );
    assert_eq!(f.runtime.take_inbox(child).unwrap().len(), 1);
    f.cleanup(root).await;
    f.cleanup(child).await;
}

#[tokio::test]
async fn saving_a_bad_component_does_not_prevent_a_later_healthy_component_commit() {
    for missing_endpoint in [false, true] {
        let f = Fixture::new(0);
        let mut ids = [f.start(), f.start()];
        ids.sort();
        let [bad, healthy] = ids;
        let recipes = BTreeMap::from([
            (bad, recipe(ResumePolicy::CoreOwned)),
            (healthy, recipe(ResumePolicy::CoreOwned)),
        ]);
        let mut records = capture(&f, &recipes).await;
        let broken = records.iter_mut().find(|r| r.run_id == bad).unwrap();
        if missing_endpoint {
            broken.parent = Some(RunId::new(0));
        } else {
            broken.old_incarnation = "wrong".into();
        }
        assert!(
            f.runtime
                .save_recovery_graph("independent", &records)
                .is_err()
        );
        let plan = f.runtime.recovery_plan().unwrap();
        assert_eq!(plan.components.len(), 1);
        assert_eq!(plan.components[0].runs, vec![healthy]);
        assert!(
            plan.report
                .skipped
                .iter()
                .any(|s| s.runs == [bad] && s.reason == RecoveryReason::NoCommittedManifest)
        );
        f.cleanup(bad).await;
        f.cleanup(healthy).await;
    }
}

#[tokio::test]
async fn a_corrupt_snapshot_descriptor_does_not_abort_other_components_capture() {
    let f = Fixture::new(0);
    let bad = f.start();
    let healthy = f.start();
    let recipes = BTreeMap::from([
        (bad, recipe(ResumePolicy::CoreOwned)),
        (healthy, recipe(ResumePolicy::CoreOwned)),
    ]);
    capture(&f, &recipes).await;
    f._storage
        .handle()
        .upsert_run_context(&storage::RunContextRecord {
            run_id: bad.to_string(),
            role: "worker".into(),
            name: "damaged descriptor".into(),
            parent_run_id: None,
            // Storage accepts JSON; typed descriptor validation belongs to runtime.
            config_json: "{}".into(),
            messages_json: "[]".into(),
            checkpoints_json: "[]".into(),
            terminal_phase: "Stopped".into(),
            restorable: true,
            updated_at_ns: 1,
            host_inputs_json: "[]".into(),
        })
        .unwrap();
    let captured = f.runtime.capture_recovery_records(&recipes).unwrap();
    assert_eq!(
        captured.iter().map(|r| r.run_id).collect::<Vec<_>>(),
        vec![healthy]
    );
    f.runtime
        .save_recovery_graph("descriptor-isolation", &captured)
        .unwrap();
    let plan = f.runtime.recovery_plan().unwrap();
    assert_eq!(plan.components.len(), 1);
    assert_eq!(plan.components[0].runs, vec![healthy]);
    f.cleanup(bad).await;
    f.cleanup(healthy).await;
}
