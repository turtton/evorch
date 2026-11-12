use super::fixture::Fixture;
use super::*;
use crate::{Role, RunConfig};
use std::{collections::BTreeMap, sync::atomic::Ordering};

fn intent_for_retirement(f: &Fixture) -> (RunId, String) {
    let run = f.runtime.reserve_run_id();
    let incarnation = f.runtime.reserve_run_id().to_string();
    f.runtime
        .admit_intent(
            run,
            incarnation.clone(),
            None,
            Role::Worker,
            "retire",
            &RunConfig::default(),
            false,
            false,
        )
        .unwrap();
    (run, incarnation)
}

fn replace_intent(f: &Fixture, run: RunId, incarnation: &str) -> ResumeIntent {
    let mut successor = f.intent(run);
    successor.incarnation = f.runtime.reserve_run_id().to_string();
    f._storage
        .handle()
        .save_resume_intent(
            storage::resume_intent::ResumeIntentRecord {
                run_id: run.to_string(),
                incarnation: successor.incarnation.clone(),
                payload: serde_json::to_string(&successor).unwrap(),
            },
            Some(incarnation.into()),
        )
        .unwrap();
    successor
}

#[tokio::test]
async fn superseded_retirement_is_local_but_generic_cas_failure_still_blocks_shutdown() {
    for retirement in [false, true] {
        let f = Fixture::new(0);
        let (run, incarnation) = intent_for_retirement(&f);
        let successor = replace_intent(&f, run, &incarnation);
        if retirement {
            f.runtime
                .end_intent(run, &incarnation, StopReason::Terminal);
            assert_eq!(f.runtime.quiesce_state(), QuiesceState::Running);
        } else {
            assert!(
                f.runtime
                    .shared
                    .quiesce
                    .update(
                        run,
                        &incarnation,
                        f.runtime.shared.run_store.get(),
                        |intent| intent.active = false
                    )
                    .is_err()
            );
            assert!(matches!(
                f.runtime.quiesce_state(),
                QuiesceState::Failed { .. }
            ));
        }
        assert_eq!(f.intent(run), successor);
        let fence = f
            .runtime
            .fence_dispatch_shutdown(|| async { Ok::<_, crate::RuntimeError>(()) })
            .await
            .unwrap();
        assert_eq!(
            f.runtime.drain_dispatch_tails(&fence).await.is_ok(),
            retirement
        );
        assert_eq!(f.intent(run), successor);
    }
}

#[tokio::test]
async fn only_unregistered_stops_discard_replaced_local_intents() {
    for reason in [StopReason::Cancelled, StopReason::OperatorStopped] {
        for registered in [false, true] {
            let f = Fixture::new(0);
            let (run, incarnation) = intent_for_retirement(&f);
            let successor = replace_intent(&f, run, &incarnation);
            f.runtime
                .note_fenced_intent_stop(run, &incarnation, reason, registered);
            assert_eq!(
                matches!(f.runtime.quiesce_state(), QuiesceState::Failed { .. }),
                registered
            );
            assert_eq!(
                f.runtime.shared.quiesce.lock().intents.contains_key(&run),
                registered
            );
            assert_eq!(f.intent(run), successor);
            let fence = f
                .runtime
                .fence_dispatch_shutdown(|| async { Ok::<_, crate::RuntimeError>(()) })
                .await
                .unwrap();
            assert_eq!(
                f.runtime.drain_dispatch_tails(&fence).await.is_ok(),
                !registered
            );
            assert_eq!(f.intent(run), successor);
        }
        let f = Fixture::new(0);
        let (run, incarnation) = intent_for_retirement(&f);
        let mut expected = f.intent(run);
        expected.stop_reason = Some(reason);
        expected.resume_requested = false;
        f.runtime
            .note_fenced_intent_stop(run, &incarnation, reason, false);
        assert_eq!(
            f.intent(run),
            expected,
            "a current pending admission stays active until joined"
        );
        assert!(expected.active);
        let revision = || {
            f.runtime
                .shared
                .run_store
                .get()
                .unwrap()
                .with_database(|db| db.resume_intent_revision(&run.to_string()))
                .unwrap()
        };
        let saved_revision = revision();
        f.runtime
            .note_fenced_intent_stop(run, &incarnation, reason, false);
        assert_eq!(
            revision(),
            saved_revision,
            "a repeated Stop must not advance revision"
        );
    }
}

#[tokio::test]
async fn obsolete_intent_writes_still_fail_shutdown_on_save_or_flush_error() {
    for completed in [false, true] {
        for step in [StorageStep::Save, StorageStep::Flush] {
            let f = Fixture::new(0);
            let (run, incarnation) = intent_for_retirement(&f);
            *f.runtime.shared.quiesce.fault.lock().unwrap() = Some(step);
            if completed {
                f.runtime
                    .end_intent(run, &incarnation, StopReason::Terminal);
            } else {
                f.runtime
                    .note_fenced_intent_stop(run, &incarnation, StopReason::Cancelled, false);
            }
            assert!(matches!(
                f.runtime.quiesce_state(),
                QuiesceState::Failed { .. }
            ));
            let fence = f
                .runtime
                .fence_dispatch_shutdown(|| async { Ok::<_, crate::RuntimeError>(()) })
                .await
                .unwrap();
            assert!(f.runtime.drain_dispatch_tails(&fence).await.is_err());
        }
    }
}

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

#[tokio::test]
async fn stopped_child_with_interrupted_tool_does_not_block_its_report_recipient() {
    let mut f = Fixture::new(1);
    let parent = f.runtime.delegate_background(
        Role::Orchestrator,
        "parent".into(),
        RunConfig {
            keep_alive: true,
            ..Default::default()
        },
    );
    let child = f
        .runtime
        .delegate_background_as_child(
            parent,
            Role::Worker,
            "child",
            RunConfig {
                keep_alive: true,
                ..Default::default()
            },
        )
        .unwrap();
    // Both loops are inside a tool; stopping the child must leave evidence of
    // its interrupted call, while the parent's call completes before capture.
    f.model.release.add_permits(2);
    f.entered.recv().await.unwrap();
    f.entered.recv().await.unwrap();
    f.runtime.stop(child, crate::StopScope::SelfOnly).unwrap();
    assert_eq!(
        f.runtime.wait(child).await.unwrap(),
        event_bus::AgentRunPhase::Stopped
    );
    assert!(!f.intent(child).tool_intent.is_empty());
    f.runtime.request_quiesce();
    f.release[0].add_permits(1);
    f.completed.recv().await.unwrap();
    f.runtime.wait_quiesced().await.unwrap();
    let records = capture(
        &f,
        &BTreeMap::from([(parent, recipe(ResumePolicy::CoreOwned))]),
    )
    .await;
    let recipient = records.iter().find(|r| r.run_id == parent).unwrap();
    assert!(recipient.deliveries.iter().any(
        |d| matches!(d.notice, DurableNotice::Completion { child: sender, .. } if sender == child)
    ));
    assert_eq!(
        f.runtime
            .save_recovery_graph("stopped-report", &records)
            .unwrap()
            .len(),
        1
    );
    let plan = f.runtime.recovery_plan().unwrap();
    assert_eq!(plan.components.len(), 1, "{plan:?}");
    assert_eq!(plan.components[0].runs, vec![parent]);
    assert!(
        plan.report
            .skipped
            .iter()
            .any(|s| s.runs == [child] && s.reason == RecoveryReason::OperatorStopped)
    );
    f.cleanup(parent).await;
}

#[tokio::test]
async fn historical_deliveries_keep_receipts_without_requiring_missing_or_tombstoned_senders() {
    for damage in ["missing", "tombstoned"] {
        let f = Fixture::new(0);
        let recipient = f.start();
        let sender = f
            .runtime
            .delegate_background_as_child(
                recipient,
                Role::Worker,
                "historical sender",
                RunConfig {
                    keep_alive: true,
                    ..Default::default()
                },
            )
            .unwrap();
        let consumed = f
            .runtime
            .send_agent_message(
                sender,
                recipient,
                event_bus::AgentMessageKind::Send,
                "consumed",
                None,
            )
            .unwrap();
        f.runtime.take_inbox(recipient).unwrap();
        let held_id = f
            .runtime
            .send_agent_message(
                sender,
                recipient,
                event_bus::AgentMessageKind::Send,
                "held",
                None,
            )
            .unwrap();
        let held = f.runtime.take_inbox(recipient).unwrap().pop().unwrap();
        let mailbox = f
            .runtime
            .send_agent_message(
                sender,
                recipient,
                event_bus::AgentMessageKind::Send,
                "queued",
                None,
            )
            .unwrap();
        let fence = f.runtime.fence_dispatch_restart().await.unwrap();
        f.runtime.drain_dispatch_tails(&fence).await.unwrap();
        f.runtime.shared.held_messages.lock().unwrap().insert(
            recipient,
            vec![crate::mailbox::QueuedMessage::explicit(held)],
        );
        let sql = rusqlite::Connection::open(&f.config.db_path).unwrap();
        if damage == "missing" {
            sql.execute(
                "DELETE FROM resume_intents WHERE run_id=?1",
                [sender.to_string()],
            )
            .unwrap();
        } else {
            sql.execute("INSERT INTO run_tombstones(run_id,thread_id,family_key) VALUES(?1,'thread','family')", [sender.to_string()]).unwrap();
        }
        drop(sql);
        let records = f
            .runtime
            .capture_recovery_records(&BTreeMap::from([(
                recipient,
                recipe(ResumePolicy::CoreOwned),
            )]))
            .unwrap();
        assert_eq!(records.len(), 1);
        let deliveries = &records[0].deliveries;
        assert!(deliveries.iter().any(|d| d.message_id == consumed
            && d.state == DurableDeliveryState::Consumed
            && d.consumed_receipt.is_some()));
        assert!(
            deliveries
                .iter()
                .any(|d| d.message_id == held_id && d.state == DurableDeliveryState::Held)
        );
        assert!(
            deliveries
                .iter()
                .any(|d| d.message_id == mailbox && d.state == DurableDeliveryState::Mailbox)
        );
        let ids = f
            .runtime
            .save_recovery_graph("historical", &records)
            .unwrap();
        assert_eq!(ids.len(), 1);
        let db = storage::Database::open(&f.config).unwrap();
        assert!(
            db.recovery_manifests().unwrap()[0]
                .record
                .as_ref()
                .unwrap()
                .edges
                .is_empty()
        );
        let plan = f.runtime.recovery_plan().unwrap();
        assert_eq!(plan.components.len(), 1, "{damage}: {plan:?}");
        assert_eq!(plan.components[0].runs, vec![recipient]);
        f.cleanup(recipient).await;
        // Sender evidence was deliberately removed; do not expect a durable
        // stopped intent for it, but still join its in-memory loop.
        let _ = f.runtime.cancel(sender);
        let _ = f.runtime.wait(sender).await;
    }
}

#[tokio::test]
async fn a_consumed_receipt_does_not_merge_independent_live_components() {
    let f = Fixture::new(0);
    let sender = f.start();
    let recipient = f.start();
    let mut records = capture(
        &f,
        &BTreeMap::from([
            (sender, recipe(ResumePolicy::CoreOwned)),
            (recipient, recipe(ResumePolicy::CoreOwned)),
        ]),
    )
    .await;
    // Historical sent receipts can outlive the relationship that originally
    // authorized the delivery; they are evidence, not a resumption dependency.
    records
        .iter_mut()
        .find(|r| r.run_id == recipient)
        .unwrap()
        .deliveries
        .push(DurableDelivery {
            message_id: "historical-receipt".into(),
            sender,
            recipient,
            sequence: 0,
            state: DurableDeliveryState::Consumed,
            consumed_receipt: Some("historical-receipt".into()),
            notice: DurableNotice::Explicit,
            message: None,
        });
    assert_eq!(
        f.runtime
            .save_recovery_graph("independent-receipt", &records)
            .unwrap()
            .len(),
        2
    );
    assert_eq!(f.runtime.recovery_plan().unwrap().components.len(), 2);
    f.cleanup(sender).await;
    f.cleanup(recipient).await;
}

#[tokio::test]
async fn blocked_component_reports_all_members_without_orphan_double_accounting() {
    for corrupt_payload in [false, true] {
        let f = Fixture::new(0);
        let root = f.runtime.delegate_background(
            Role::Orchestrator,
            "parent".into(),
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
        let records = capture(
            &f,
            &BTreeMap::from([(root, recipe(ResumePolicy::Unsupported))]),
        )
        .await;
        f.runtime
            .save_recovery_graph("blocked-members", &records)
            .unwrap();
        if corrupt_payload {
            let sql = rusqlite::Connection::open(&f.config.db_path).unwrap();
            sql.execute(
                "UPDATE recovery_members SET record_json='broken' WHERE run_id=?1",
                [child.to_string()],
            )
            .unwrap();
        }
        let plan = f.runtime.recovery_plan().unwrap();
        assert!(plan.components.is_empty());
        let mut expected = vec![root, child];
        expected.sort();
        assert_eq!(plan.report.blocked.len(), 1, "{plan:?}");
        assert_eq!(plan.report.blocked[0].runs, expected);
        assert!(
            !plan
                .report
                .skipped
                .iter()
                .any(|s| s.reason == RecoveryReason::NoCommittedManifest),
            "{plan:?}"
        );
        f.cleanup(root).await;
        f.cleanup(child).await;
    }
}
