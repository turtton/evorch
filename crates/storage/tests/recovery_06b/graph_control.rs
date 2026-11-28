use super::*;
use storage::recovery::{graph_control::*, restart::*};

fn graph(f: &Fixture, claimed: bool) -> (GraphControlWrite, RecoveryActivationRequest) {
    let activation = f.seed(false);
    let claim = claimed.then(|| f.claim(&activation));
    let manifest = f
        .db()
        .recovery_manifests()
        .unwrap()
        .remove(0)
        .record
        .unwrap();
    let metadata = RestartMetadata {
        restart_id: "restart".into(),
        capture_id: "capture".into(),
        config_identity: "config".into(),
        old_epoch: 1,
        successor_epoch: 2,
        old_build: "old".into(),
        successor_build: "new".into(),
        target_identity: "target".into(),
        handoff_version: 1,
        snapshot_version: 1,
        storage_version: 71,
        old_builtin_tool_names: Default::default(),
        old_compiled_catalog_fingerprint: None,
        new_compiled_catalog_fingerprint: None,
        old_effective_tools_fingerprint: None,
        new_effective_tools_fingerprint: None,
    };
    f.storage
        .handle()
        .mutate_restart(
            "restart".into(),
            None,
            RestartMutation::Create(Box::new(metadata)),
        )
        .unwrap();
    let sql = f.sql();
    sql.execute("UPDATE restart_transactions SET phase='adopted',adopted_epoch=2 WHERE restart_id='restart'",[]).unwrap();
    sql.execute("INSERT INTO restart_components(restart_id,manifest_id,disposition) VALUES('restart','component','retry_pending')",[]).unwrap();
    let membership: Vec<_> = manifest
        .members
        .iter()
        .map(|m| RestartExpectedMember {
            run_id: m.run_id.clone(),
            incarnation: m.incarnation.clone(),
            root_run_id: m.run_id.clone(),
            thread_id: Some(m.run_id.clone()),
            intent_revision: m.intent_revision,
            snapshot: m.snapshot.clone(),
            lane_id: None,
        })
        .collect();
    for m in &membership {
        sql.execute("INSERT INTO restart_expected_members(restart_id,run_id,root_run_id,member_json,manifest_id) VALUES('restart',?1,?2,?3,'component')",
            rusqlite::params![m.run_id,m.root_run_id,serde_json::to_string(m).unwrap()]).unwrap();
    }
    let component = f
        .db()
        .restart_transaction("restart")
        .unwrap()
        .unwrap()
        .components
        .remove(0);
    (
        GraphControlWrite {
            expected: GraphControlCas {
                restart_id: "restart".into(),
                capture_id: "capture".into(),
                successor_epoch: 2,
                phase: RestartPhase::Adopted,
                component,
                membership,
                manifest,
                claim,
                control_revision: 0,
                control_effect: None,
            },
            effect: GraphEffectKey {
                operation_id: "release".into(),
                attempt_nonce: "nonce".into(),
            },
            action: GraphControlAction::Release,
        },
        activation,
    )
}

#[test]
fn release_atomically_revokes_and_releases_existing_prepared() {
    let f = Fixture::new();
    let (request, activation) = graph(&f, true);
    let result = f
        .storage
        .handle()
        .write_recovery_graph_control(request.clone())
        .unwrap();
    assert!(result.revoked);
    assert_eq!(result.mode, "revoked_running");
    assert!(matches!(
        f.db().recovery_claims().unwrap()[0].state,
        RecoveryClaimState::Released { .. }
    ));
    assert_eq!(
        f.db()
            .restart_transaction("restart")
            .unwrap()
            .unwrap()
            .components[0]
            .disposition,
        RestartDisposition::Released
    );
    assert!(
        f.storage
            .handle()
            .activate_recovery_claim(activation)
            .is_err()
    );
    assert!(
        f.storage
            .handle()
            .claim_recovery_manifest(RecoveryClaimRequest {
                claim_id: "new".into(),
                manifest_id: "component".into(),
                incarnations: vec![]
            })
            .is_err()
    );
    assert_eq!(
        f.storage
            .handle()
            .write_recovery_graph_control(request.clone())
            .unwrap(),
        result
    );
    let mut changed = request;
    changed.effect.attempt_nonce = "different".into();
    assert!(
        f.storage
            .handle()
            .write_recovery_graph_control(changed)
            .is_err()
    );
    assert_eq!(
        f.db().recovery_graph_effect("release").unwrap(),
        Some(result)
    );
}

#[test]
fn claim_absent_is_quarantined_without_creating_a_claim() {
    let f = Fixture::new();
    let (request, activation) = graph(&f, false);
    let result = f
        .storage
        .handle()
        .write_recovery_graph_control(request)
        .unwrap();
    assert_eq!(result.origin, "manual_release_without_claim");
    assert!(
        f.storage
            .handle()
            .claim_recovery_manifest(RecoveryClaimRequest {
                claim_id: "after-revocation".into(),
                manifest_id: "component".into(),
                incarnations: activation
                    .intents
                    .iter()
                    .map(|i| (i.run_id.clone(), i.incarnation.clone()))
                    .collect(),
            })
            .is_err(),
        "the writer itself must reject prepare after revocation even with no prior claim"
    );
    assert!(f.db().recovery_claims().unwrap().is_empty());
    let tx = f.db().restart_transaction("restart").unwrap().unwrap();
    assert_eq!(
        tx.components[0].disposition,
        RestartDisposition::Quarantined
    );
}

#[test]
fn progress_is_full_durable_monotone_and_independent_of_operation_id() {
    use storage::recovery::graph_progress::*;
    for after_commit in [false, true] {
        let f = Fixture::new();
        let (request, _) = graph(&f, true);
        assert_eq!(
            f.db().recovery_graph_cas("restart", "component").unwrap(),
            request.expected
        );
        let control = f
            .storage
            .handle()
            .write_recovery_graph_control(request)
            .unwrap();
        let key = GraphDisposalProgressKey {
            manifest_id: "component".into(),
            restart_id: "restart".into(),
            capture_id: "capture".into(),
            thread_id: "run".into(),
        };
        // Fixture run IDs are canonical strings from the actual membership.
        let key = GraphDisposalProgressKey {
            thread_id: f
                .db()
                .recovery_graph_cas("restart", "component")
                .unwrap()
                .membership[0]
                .thread_id
                .clone()
                .unwrap(),
            ..key
        };
        let write = GraphThreadProgressWrite {
            key: key.clone(),
            expected_revision: 0,
            control: control.clone(),
            scratch: GraphDisposalStep::Applied,
        };
        f.storage
            .handle()
            .inject_restart_write_fault(after_commit)
            .unwrap();
        assert!(
            f.storage
                .handle()
                .write_recovery_graph_progress(write.clone())
                .is_err()
        );
        let readback = f.db().recovery_graph_thread_progress(&key).unwrap();
        assert_eq!(readback.is_some(), after_commit);
        let applied = if after_commit {
            readback.unwrap()
        } else {
            f.storage
                .handle()
                .write_recovery_graph_progress(write.clone())
                .unwrap()
        };
        assert_eq!(applied.revision, 1);
        let mut foreign = write.clone();
        foreign.key.capture_id = "foreign".into();
        assert!(
            f.storage
                .handle()
                .write_recovery_graph_progress(foreign)
                .is_err()
        );
        let mut stale = write.clone();
        stale.expected_revision = 1;
        stale.scratch = GraphDisposalStep::Unknown;
        assert!(
            f.storage
                .handle()
                .write_recovery_graph_progress(stale)
                .is_err()
        );
        // A new administrative ID cannot make the graph/thread step runnable again.
        f.sql().execute("UPDATE recovery_graph_control SET operation_id='new-release',attempt_nonce='new-attempt',revision=revision+1",[]).unwrap();
        let mut fresh = write;
        fresh.control = f.db().recovery_graph_control("component").unwrap().unwrap();
        fresh.expected_revision = 1;
        fresh.scratch = GraphDisposalStep::NotStarted;
        assert!(
            f.storage
                .handle()
                .write_recovery_graph_progress(fresh)
                .is_err()
        );
        assert_eq!(
            f.db().recovery_graph_thread_progress(&key).unwrap(),
            Some(applied)
        );
    }
}

#[test]
fn every_main_cas_mismatch_leaves_all_effects_unapplied() {
    let mutations = graph_cas_mutations();
    for mutate in mutations {
        let f = Fixture::new();
        let (mut request, _) = graph(&f, true);
        let claim = request.expected.claim.clone().unwrap();
        let before = request.expected.clone();
        mutate(&mut request.expected);
        assert!(
            f.storage
                .handle()
                .write_recovery_graph_control(request)
                .is_err()
        );
        f.assert_prepared(&claim);
        assert_eq!(
            f.db().recovery_graph_cas("restart", "component").unwrap(),
            before
        );
        assert!(
            f.db()
                .recovery_graph_control("component")
                .unwrap()
                .is_none()
        );
        assert!(f.db().recovery_graph_effect("release").unwrap().is_none());
        assert_eq!(
            f.db()
                .restart_transaction("restart")
                .unwrap()
                .unwrap()
                .components[0]
                .revision,
            0
        );
    }
}

#[test]
fn pre_and_post_commit_ack_loss_are_distinguishable_by_same_id_readback() {
    for after in [false, true] {
        let f = Fixture::new();
        let (request, _) = graph(&f, true);
        f.storage
            .handle()
            .inject_restart_write_fault(after)
            .unwrap();
        assert!(
            f.storage
                .handle()
                .write_recovery_graph_control(request)
                .is_err()
        );
        let result = f.db().recovery_graph_effect("release").unwrap();
        assert_eq!(result.is_some(), after);
        assert_eq!(
            f.db()
                .recovery_graph_control("component")
                .unwrap()
                .is_some(),
            after
        );
        assert_eq!(
            matches!(
                f.db().recovery_claims().unwrap()[0].state,
                RecoveryClaimState::Released { .. }
            ),
            after
        );
    }
}

#[test]
fn activation_checks_reservation_and_resolves_manual_marker_in_same_transaction() {
    let f = Fixture::new();
    let (mut request, mut activation) = graph(&f, true);
    request.action = GraphControlAction::Resume;
    let result = f
        .storage
        .handle()
        .write_recovery_graph_control(request.clone())
        .unwrap();
    assert!(
        f.storage
            .handle()
            .activate_recovery_claim(activation.clone())
            .is_err()
    );
    for control in [
        ActivationControl::Manual {
            effect: request.effect.clone(),
            control_revision: result.revision + 1,
        },
        ActivationControl::Manual {
            effect: GraphEffectKey {
                operation_id: "other".into(),
                attempt_nonce: "nonce".into(),
            },
            control_revision: result.revision,
        },
    ] {
        activation.reservation = control;
        assert!(
            f.storage
                .handle()
                .activate_recovery_claim(activation.clone())
                .is_err()
        );
        f.assert_prepared(request.expected.claim.as_ref().unwrap());
    }
    activation.reservation = ActivationControl::Manual {
        effect: request.effect,
        control_revision: result.revision,
    };
    f.storage
        .handle()
        .inject_restart_write_fault(false)
        .unwrap();
    assert!(
        f.storage
            .handle()
            .activate_recovery_claim(activation.clone())
            .is_err()
    );
    assert_eq!(
        f.db()
            .recovery_graph_control("component")
            .unwrap()
            .unwrap()
            .mode,
        "manual_running"
    );
    f.storage
        .handle()
        .activate_recovery_claim(activation)
        .unwrap();
    assert_eq!(
        f.db()
            .recovery_graph_control("component")
            .unwrap()
            .unwrap()
            .mode,
        "resolved"
    );
}

// One-field changes across every scalar and nested CAS record. Each is exercised
// both on first Release and on the remaining Release after revoked_idle.
fn graph_cas_mutations() -> Vec<fn(&mut GraphControlCas)> {
    vec![
        |c| c.restart_id.push('x'),
        |c| c.capture_id.push('x'),
        |c| c.successor_epoch += 1,
        |c| c.phase = RestartPhase::Completed,
        |c| c.component.manifest_id.push('x'),
        |c| c.component.revision += 1,
        |c| c.component.disposition = RestartDisposition::Activated,
        |c| c.component.claim_id = Some("other".into()),
        |c| c.component.reason = Some("other".into()),
        |c| c.control_revision += 1,
        |c| {
            c.control_effect = Some(GraphEffectKey {
                operation_id: "other".into(),
                attempt_nonce: "other".into(),
            })
        },
        |c| {
            c.membership.pop();
        },
        |c| c.membership[0].run_id.push('x'),
        |c| c.membership[0].incarnation.push('x'),
        |c| c.membership[0].root_run_id.push('x'),
        |c| c.membership[0].thread_id = Some("other".into()),
        |c| c.membership[0].intent_revision += 1,
        |c| {
            c.membership[0].snapshot = Some(storage::RunContextReceipt {
                updated_at_ns: 1,
                messages_end: 1,
                sha256: "other".into(),
            })
        },
        |c| c.membership[0].lane_id = Some("other".into()),
        |c| c.manifest.manifest_id.push('x'),
        |c| c.manifest.schema_version += 1,
        |c| c.manifest.committed = !c.manifest.committed,
        |c| c.manifest.superseded = !c.manifest.superseded,
        |c| {
            c.manifest.edges.push(storage::recovery::RecoveryEdge {
                from: "other".into(),
                to: "other".into(),
                kind: "other".into(),
            })
        },
        |c| {
            c.manifest.members.pop();
        },
        |c| c.manifest.members[0].run_id.push('x'),
        |c| c.manifest.members[0].incarnation.push('x'),
        |c| c.manifest.members[0].intent_revision += 1,
        |c| {
            c.manifest.members[0].snapshot = Some(storage::RunContextReceipt {
                updated_at_ns: 1,
                messages_end: 1,
                sha256: "other".into(),
            })
        },
        |c| c.manifest.members[0].resume = !c.manifest.members[0].resume,
        |c| c.manifest.members[0].record_json.push(' '),
        |c| c.claim = None,
        |c| c.claim.as_mut().unwrap().claim_id.push('x'),
        |c| c.claim.as_mut().unwrap().manifest_id.push('x'),
        |c| c.claim.as_mut().unwrap().state = RecoveryClaimState::Activated,
        |c| {
            c.claim.as_mut().unwrap().members.pop();
        },
        |c| c.claim.as_mut().unwrap().members[0].run_id.push('x'),
        |c| {
            c.claim.as_mut().unwrap().members[0]
                .old_incarnation
                .push('x')
        },
        |c| c.claim.as_mut().unwrap().members[0].reserved_revision += 1,
        |c| {
            c.claim.as_mut().unwrap().members[0]
                .new_incarnation
                .push('x')
        },
    ]
}

#[test]
fn explicit_finish_is_cas_only_permanent_and_same_id_is_readback() {
    let mutations: [fn(&mut GraphControlRecord); 7] = [
        |c| c.manifest_id.push('x'),
        |c| c.mode = "resolved".into(),
        |c| c.revision += 1,
        |c| c.effect.operation_id.push('x'),
        |c| c.effect.attempt_nonce.push('x'),
        |c| c.revoked = !c.revoked,
        |c| c.origin.push('x'),
    ];
    for action in [GraphControlAction::Resume, GraphControlAction::Release] {
        let f = Fixture::new();
        let (mut request, _) = graph(&f, true);
        request.action = action;
        let running = f
            .storage
            .handle()
            .write_recovery_graph_control(request.clone())
            .unwrap();
        for mutate in mutations {
            let mut wrong = running.clone();
            mutate(&mut wrong);
            assert!(
                f.storage
                    .handle()
                    .finish_recovery_graph_control(wrong)
                    .is_err()
            );
            assert_eq!(
                f.db().recovery_graph_control("component").unwrap(),
                Some(running.clone())
            );
        }
        let before_claims = f.db().recovery_claims().unwrap();
        let before_component = f
            .db()
            .restart_transaction("restart")
            .unwrap()
            .unwrap()
            .components;
        f.storage.handle().inject_restart_write_fault(true).unwrap();
        assert!(
            f.storage
                .handle()
                .finish_recovery_graph_control(running.clone())
                .is_err()
        );
        let idle = f
            .db()
            .recovery_graph_effect(&running.effect.operation_id)
            .unwrap()
            .unwrap();
        assert_eq!(
            idle.mode,
            if action == GraphControlAction::Release {
                "revoked_idle"
            } else {
                "manual_idle"
            }
        );
        assert_eq!(idle.revoked, running.revoked);
        assert_eq!(idle.revision, running.revision + 1);
        for _ in 0..2 {
            assert_eq!(
                f.storage
                    .handle()
                    .finish_recovery_graph_control(running.clone())
                    .unwrap(),
                idle
            );
            assert_eq!(
                f.storage
                    .handle()
                    .write_recovery_graph_control(request.clone())
                    .unwrap(),
                idle
            );
        }
        assert_eq!(
            f.db().recovery_graph_control("component").unwrap(),
            Some(idle)
        );
        assert_eq!(f.db().recovery_claims().unwrap(), before_claims);
        assert_eq!(
            f.db()
                .restart_transaction("restart")
                .unwrap()
                .unwrap()
                .components,
            before_component
        );
    }
}
#[test]
fn residual_release_changes_only_control_and_journal_and_rejects_every_cas_conflict() {
    let f = Fixture::new();
    let (request, _) = graph(&f, true);
    let running = f
        .storage
        .handle()
        .write_recovery_graph_control(request)
        .unwrap();
    let idle = f
        .storage
        .handle()
        .finish_recovery_graph_control(running)
        .unwrap();
    let expected = f.db().recovery_graph_cas("restart", "component").unwrap();
    let next = GraphControlWrite {
        expected,
        effect: GraphEffectKey {
            operation_id: "residual".into(),
            attempt_nonce: "next".into(),
        },
        action: GraphControlAction::Release,
    };
    let before_claims = f.db().recovery_claims().unwrap();
    let before_component = f
        .db()
        .restart_transaction("restart")
        .unwrap()
        .unwrap()
        .components;
    for mutate in graph_cas_mutations() {
        let mut wrong = next.clone();
        mutate(&mut wrong.expected);
        assert!(
            f.storage
                .handle()
                .write_recovery_graph_control(wrong)
                .is_err()
        );
        assert_eq!(
            f.db().recovery_graph_control("component").unwrap(),
            Some(idle.clone())
        );
    }
    let result = f
        .storage
        .handle()
        .write_recovery_graph_control(next.clone())
        .unwrap();
    assert_eq!(result.mode, "revoked_running");
    assert!(result.revoked);
    assert_eq!(result.origin, idle.origin);
    assert_eq!(result.revision, idle.revision + 1);
    assert_eq!(f.db().recovery_claims().unwrap(), before_claims);
    assert_eq!(
        f.db()
            .restart_transaction("restart")
            .unwrap()
            .unwrap()
            .components,
        before_component
    );
    assert_eq!(
        f.storage
            .handle()
            .write_recovery_graph_control(next)
            .unwrap(),
        result
    );
    assert_eq!(
        f.db().recovery_graph_control("component").unwrap(),
        Some(result)
    );
}

#[test]
fn relational_membership_is_complete_in_readonly_snapshot_and_full_writer_cas() {
    for assignment in [None, Some("foreign-component")] {
        let f = Fixture::new();
        let (request, _) = graph(&f, true);
        assert_eq!(
            f.db().recovery_graph_cas("restart", "component").unwrap(),
            request.expected
        );
        f.sql()
            .execute(
                "UPDATE restart_expected_members SET manifest_id=?1 WHERE run_id=?2",
                rusqlite::params![assignment, request.expected.membership[0].run_id],
            )
            .unwrap();
        assert!(f.db().recovery_graph_cas("restart", "component").is_err());
        assert!(
            f.storage
                .handle()
                .write_recovery_graph_control(request)
                .is_err()
        );
        assert!(
            f.db()
                .recovery_graph_control("component")
                .unwrap()
                .is_none()
        );
        assert!(f.db().recovery_graph_effect("release").unwrap().is_none());
        assert!(matches!(
            f.db().recovery_claims().unwrap()[0].state,
            RecoveryClaimState::Prepared
        ));
        assert_eq!(
            f.db()
                .restart_transaction("restart")
                .unwrap()
                .unwrap()
                .components[0]
                .disposition,
            RestartDisposition::RetryPending
        );
    }
}

#[test]
fn terminal_cleanup_confirms_revocation_without_releasing_or_activating() {
    for phase in ["aborted", "invalidated", "superseded"] {
        for claimed in [false, true] {
            let f = Fixture::new();
            let (mut request, _) = graph(&f, claimed);
            if let Some(claim) = &request.expected.claim {
                f.storage
                    .handle()
                    .release_prepared_claim_if_unexecuted(
                        claim.claim_id.clone(),
                        "terminal before cleanup".into(),
                    )
                    .unwrap();
            }
            f.sql()
                .execute(
                    "UPDATE restart_transactions SET phase=?1 WHERE restart_id='restart'",
                    [phase],
                )
                .unwrap();
            request.expected = f.db().recovery_graph_cas("restart", "component").unwrap();
            request.action = GraphControlAction::TerminalCleanup;
            let before = request.expected.clone();
            let result = f
                .storage
                .handle()
                .write_recovery_graph_control(request.clone())
                .unwrap();
            assert!(result.revoked);
            assert_eq!(
                result.origin,
                if claimed {
                    "manual_release"
                } else {
                    "manual_release_without_claim"
                }
            );
            let after = f.db().recovery_graph_cas("restart", "component").unwrap();
            assert_eq!(after.claim, before.claim);
            assert_eq!(after.component, before.component);
            assert_eq!(after.manifest, before.manifest);
            assert_eq!(after.phase, before.phase);
            assert!(matches!(
                after.phase,
                RestartPhase::Aborted | RestartPhase::Invalidated | RestartPhase::Superseded
            ));
            // Origins are shared, but phase and unchanged disposition distinguish
            // terminal cleanup from an actual user manual release.
            let manual = Fixture::new();
            let (manual_request, _) = graph(&manual, claimed);
            let manual_result = manual
                .storage
                .handle()
                .write_recovery_graph_control(manual_request)
                .unwrap();
            assert_eq!(manual_result.origin, result.origin);
            let manual_cas = manual
                .db()
                .recovery_graph_cas("restart", "component")
                .unwrap();
            assert!(!matches!(
                manual_cas.phase,
                RestartPhase::Aborted | RestartPhase::Invalidated | RestartPhase::Superseded
            ));
            assert_ne!(manual_cas.phase, after.phase);
            assert_eq!(
                f.storage
                    .handle()
                    .write_recovery_graph_control(request)
                    .unwrap(),
                result
            );
            f.storage
                .handle()
                .finish_recovery_graph_control(result)
                .unwrap();
        }
    }
}

#[test]
fn terminal_cleanup_rejects_prepared_activated_and_nonterminal_state() {
    for phase in [
        "adopted",
        "completed",
        "aborted",
        "invalidated",
        "superseded",
    ] {
        for state in ["prepared", "activated"] {
            let f = Fixture::new();
            let (mut request, activation) = graph(&f, true);
            if state == "activated" {
                f.storage
                    .handle()
                    .activate_recovery_claim(activation)
                    .unwrap();
            }
            f.sql()
                .execute(
                    "UPDATE restart_transactions SET phase=?1 WHERE restart_id='restart'",
                    [phase],
                )
                .unwrap();
            request.expected = f.db().recovery_graph_cas("restart", "component").unwrap();
            request.action = GraphControlAction::TerminalCleanup;
            let before = request.expected.clone();
            assert!(
                f.storage
                    .handle()
                    .write_recovery_graph_control(request)
                    .is_err(),
                "{phase}/{state}"
            );
            assert_eq!(
                f.db().recovery_graph_cas("restart", "component").unwrap(),
                before
            );
            assert!(f.db().recovery_graph_effect("release").unwrap().is_none());
        }
    }
}

fn ended_cleanup(f: &Fixture, phase: &str, claimed: bool, released: bool) -> GraphControlWrite {
    let (mut request, _) = graph(f, claimed);
    if released {
        f.storage
            .handle()
            .release_prepared_claim_if_unexecuted(
                request.expected.claim.as_ref().unwrap().claim_id.clone(),
                "already ended".into(),
            )
            .unwrap();
    }
    f.sql()
        .execute(
            "UPDATE restart_transactions SET phase=?1 WHERE restart_id='restart'",
            [phase],
        )
        .unwrap();
    request.expected = f.db().recovery_graph_cas("restart", "component").unwrap();
    request.action = GraphControlAction::TerminalCleanup;
    assert!(
        f.db()
            .recovery_graph_control("component")
            .unwrap()
            .is_none()
    );
    request
}
fn assert_cleanup_rejected(f: &Fixture, request: GraphControlWrite) {
    let before = request.expected.clone();
    assert!(
        f.storage
            .handle()
            .write_recovery_graph_control(request)
            .is_err()
    );
    assert_eq!(
        f.db().recovery_graph_cas("restart", "component").unwrap(),
        before
    );
    assert!(f.db().recovery_graph_effect("release").unwrap().is_none());
}
#[test]
fn completed_cleanup_without_control_accepts_absent_and_released_only() {
    for claimed in [false, true] {
        let f = Fixture::new();
        let request = ended_cleanup(&f, "completed", claimed, claimed);
        let before = request.expected.clone();
        assert!(
            f.storage
                .handle()
                .write_recovery_graph_control(request)
                .unwrap()
                .revoked
        );
        let after = f.db().recovery_graph_cas("restart", "component").unwrap();
        assert_eq!(after.phase, before.phase);
        assert_eq!(after.claim, before.claim);
        assert_eq!(after.component, before.component);
        assert_eq!(after.manifest, before.manifest);
    }
}
#[test]
fn adopted_released_cleanup_without_control_preserves_ended_eligibility() {
    let f = Fixture::new();
    let request = ended_cleanup(&f, "adopted", true, true);
    let before = request.expected.clone();
    assert!(
        f.storage
            .handle()
            .write_recovery_graph_control(request)
            .unwrap()
            .revoked
    );
    let after = f.db().recovery_graph_cas("restart", "component").unwrap();
    assert_eq!(after.phase, before.phase);
    assert_eq!(after.claim, before.claim);
    assert_eq!(after.component, before.component);
}
#[test]
fn completed_cleanup_phase_guard_rejects_armed_without_claim() {
    let f = Fixture::new();
    let request = ended_cleanup(&f, "armed", false, false);
    assert_cleanup_rejected(&f, request);
}
#[test]
fn released_cleanup_phase_guard_rejects_armed_with_released_claim() {
    let f = Fixture::new();
    let request = ended_cleanup(&f, "armed", true, true);
    assert_cleanup_rejected(&f, request);
}
#[test]
fn completed_cleanup_claim_guard_rejects_prepared_and_activated() {
    cleanup_live_claim_rejected("completed");
}
#[test]
fn adopted_cleanup_claim_guard_rejects_prepared_and_activated() {
    cleanup_live_claim_rejected("adopted");
}
fn cleanup_live_claim_rejected(phase: &str) {
    for activated in [false, true] {
        let f = Fixture::new();
        let (mut request, activation) = graph(&f, true);
        if activated {
            f.storage
                .handle()
                .activate_recovery_claim(activation)
                .unwrap();
        }
        f.sql()
            .execute(
                "UPDATE restart_transactions SET phase=?1 WHERE restart_id='restart'",
                [phase],
            )
            .unwrap();
        request.expected = f.db().recovery_graph_cas("restart", "component").unwrap();
        request.action = GraphControlAction::TerminalCleanup;
        assert_cleanup_rejected(&f, request);
    }
}
#[test]
fn ended_cleanup_disposition_guard_rejects_activated_even_if_claim_is_released() {
    for phase in ["completed", "adopted"] {
        let f = Fixture::new();
        let mut request = ended_cleanup(&f, phase, true, true);
        f.sql().execute("UPDATE restart_components SET disposition='activated' WHERE manifest_id='component'", []).unwrap();
        request.expected = f.db().recovery_graph_cas("restart", "component").unwrap();
        assert_cleanup_rejected(&f, request);
    }
}
#[test]
fn graph_cas_refusal_has_typed_conflict_and_no_effect() {
    let f = Fixture::new();
    let (mut request, _) = graph(&f, false);
    request.expected.component.revision += 1;
    assert!(matches!(
        f.storage.handle().write_recovery_graph_control(request),
        Err(storage::StorageError::RecoveryGraphConflict(_))
    ));
    assert!(f.db().recovery_graph_effect("release").unwrap().is_none());
}

#[test]
fn adopted_cleanup_requires_an_existing_released_claim_not_absence() {
    let f = Fixture::new();
    let request = ended_cleanup(&f, "adopted", false, false);
    assert_cleanup_rejected(&f, request);
}
