//! Durable activation/release boundaries. Faults are synchronous SQLite triggers.
use serde_json::{Value, json};
use storage::{
    Database, RunContextRecord, Storage, StorageConfig, recovery::*,
    resume_intent::ResumeIntentRecord,
};

#[path = "recovery_06b/retention.rs"]
mod retention;

struct Fixture {
    _dir: tempfile::TempDir,
    config: StorageConfig,
    storage: Storage,
}
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let config = StorageConfig {
            db_path: dir.path().join("activation.db"),
            ..Default::default()
        };
        let storage = Storage::open(config.clone()).unwrap();
        Self {
            _dir: dir,
            config,
            storage,
        }
    }
    fn db(&self) -> Database {
        Database::open(&self.config).unwrap()
    }
    fn sql(&self) -> rusqlite::Connection {
        rusqlite::Connection::open(&self.config.db_path).unwrap()
    }
    fn seed(&self, terminal: bool) -> RecoveryActivationRequest {
        let mut members = Vec::new();
        let mut snapshots = Vec::new();
        let mut intents = Vec::new();
        for (id, resume) in [("run-a", true), ("run-b", !terminal)] {
            let record = RunContextRecord {
                run_id: id.into(),
                role: "Worker".into(),
                name: id.into(),
                parent_run_id: None,
                config_json: "{}".into(),
                messages_json: json!([{"role":"user","content":[]}]).to_string(),
                checkpoints_json: "[]".into(),
                terminal_phase: "running".into(),
                restorable: false,
                updated_at_ns: 1,
                host_inputs_json: "[]".into(),
            };
            let receipt = self
                .storage
                .handle()
                .upsert_run_context_receipt(&record)
                .unwrap();
            let mut value = json!({
                "run_id":id, "incarnation":"old", "parent_run_id":null,"role":"Worker",
                "active":resume,"stop_reason":if resume { Value::Null } else { json!("terminal") },
                "tool_intent":[], "snapshot":receipt, "provider_input":{"prefix":["sent"]},
                "continuation":{"call_order":["first","second"]}
            });
            self.storage
                .handle()
                .save_resume_intent(
                    ResumeIntentRecord {
                        run_id: id.into(),
                        incarnation: "old".into(),
                        payload: value.to_string(),
                    },
                    None,
                )
                .unwrap();
            members.push(RecoveryMemberRecord {
                run_id: id.into(),
                incarnation: "old".into(),
                intent_revision: 0,
                snapshot: Some(receipt.clone()),
                resume,
                record_json:
                    json!({"run_id":id,"old_incarnation":"old","parent":null,"role":"Worker"})
                        .to_string(),
            });
            snapshots.push(RecoverySnapshotExpectation {
                run_id: id.into(),
                receipt,
                config_json: record.config_json,
            });
            if resume {
                value["incarnation"] = json!(format!("new-{id}"));
                intents.push(ResumeIntentRecord {
                    run_id: id.into(),
                    incarnation: format!("new-{id}"),
                    payload: value.to_string(),
                });
            }
        }
        self.storage
            .handle()
            .prepare_recovery_manifest(RecoveryManifestRecord {
                manifest_id: "component".into(),
                schema_version: 1,
                committed: false,
                superseded: false,
                edges: vec![RecoveryEdge {
                    from: "run-a".into(),
                    to: "run-b".into(),
                    kind: "wait".into(),
                }],
                members,
            })
            .unwrap();
        self.storage
            .handle()
            .commit_recovery_manifest("component".into())
            .unwrap();
        RecoveryActivationRequest {
            claim_id: "claim".into(),
            intents,
            snapshots,
        }
    }
    fn claim(&self, request: &RecoveryActivationRequest) -> RecoveryClaim {
        self.storage
            .handle()
            .claim_recovery_manifest(RecoveryClaimRequest {
                claim_id: request.claim_id.clone(),
                manifest_id: "component".into(),
                incarnations: request
                    .intents
                    .iter()
                    .map(|r| (r.run_id.clone(), r.incarnation.clone()))
                    .collect(),
            })
            .unwrap()
    }
    fn assert_prepared(&self, claim: &RecoveryClaim) {
        let db = self.db();
        assert_eq!(
            db.recovery_claims().unwrap().as_slice(),
            std::slice::from_ref(claim)
        );
        assert!(
            !db.recovery_manifests().unwrap()[0]
                .record
                .as_ref()
                .unwrap()
                .superseded
        );
        for m in &claim.members {
            assert_eq!(
                db.resume_intent(&m.run_id).unwrap().unwrap().incarnation,
                m.old_incarnation
            );
            assert_eq!(
                db.resume_intent_revision(&m.run_id).unwrap(),
                Some(m.reserved_revision)
            );
        }
    }
}

#[test]
fn activation_commits_all_intents_claim_and_retirement_without_consuming_snapshots() {
    let f = Fixture::new();
    let request = f.seed(false);
    let claim = f.claim(&request);
    assert_eq!(
        f.db().observe_recovery_activation(&claim).unwrap(),
        RecoveryActivationObservation::NotActivated
    );
    let activated = f
        .storage
        .handle()
        .activate_recovery_claim(request.clone())
        .unwrap();
    assert_eq!(activated.state, RecoveryClaimState::Activated);
    assert!(
        f.storage
            .handle()
            .activate_recovery_claim(request.clone())
            .is_err()
    );
    assert!(
        f.storage
            .handle()
            .release_recovery_claim("claim".into(), "retry".into())
            .is_err()
    );
    for snapshot in &request.snapshots {
        let record = f.db().run_context(&snapshot.run_id).unwrap().unwrap();
        assert!(snapshot.receipt.matches(&record));
        assert_eq!(record.config_json, snapshot.config_json);
    }
    assert_eq!(f.db().resume_intent_revision("run-a").unwrap(), Some(2));
    f.storage.close();
    let db = Database::open(&f.config).unwrap();
    assert_eq!(db.recovery_claims().unwrap(), [activated]);
    assert!(
        db.recovery_manifests().unwrap()[0]
            .record
            .as_ref()
            .unwrap()
            .superseded
    );
    assert_eq!(
        db.observe_recovery_activation(&claim).unwrap(),
        RecoveryActivationObservation::Activated
    );
    assert_eq!(db.resume_intents().unwrap(), request.intents);
}

#[test]
fn sql_fault_after_each_write_rolls_back_every_activation_effect() {
    for fault in [
        "CREATE TRIGGER fault AFTER UPDATE OF incarnation ON resume_intents WHEN NEW.run_id='run-b' BEGIN SELECT RAISE(ABORT,'intent fault'); END",
        "CREATE TRIGGER fault AFTER UPDATE OF state ON recovery_claims WHEN NEW.state='activated' BEGIN SELECT RAISE(ABORT,'claim fault'); END",
        "CREATE TRIGGER fault BEFORE UPDATE OF superseded ON recovery_manifests WHEN NEW.superseded=1 BEGIN SELECT RAISE(ABORT,'retirement fault'); END",
    ] {
        let f = Fixture::new();
        let request = f.seed(false);
        let claim = f.claim(&request);
        f.sql().execute_batch(fault).unwrap();
        assert!(f.storage.handle().activate_recovery_claim(request).is_err());
        f.assert_prepared(&claim);
    }
}

#[test]
fn activation_rejects_every_incomplete_or_rewritten_member_set() {
    for damage in [
        "missing",
        "duplicate",
        "extra",
        "incarnation",
        "parent",
        "role",
        "prefix",
        "continuation",
        "snapshot_missing",
        "snapshot_duplicate",
    ] {
        let f = Fixture::new();
        let mut request = f.seed(false);
        let claim = f.claim(&request);
        match damage {
            "missing" => {
                request.intents.pop();
            }
            "duplicate" => {
                request.intents[1] = request.intents[0].clone();
            }
            "extra" => {
                request.intents.push(request.intents[0].clone());
            }
            "incarnation" => {
                request.intents[1].incarnation = "unclaimed".into();
            }
            "snapshot_missing" => {
                request.snapshots.pop();
            }
            "snapshot_duplicate" => {
                request.snapshots[1] = request.snapshots[0].clone();
            }
            field => {
                let mut value: Value = serde_json::from_str(&request.intents[1].payload).unwrap();
                value[match field {
                    "parent" => "parent_run_id",
                    "prefix" => "provider_input",
                    _ => field,
                }] = json!("rewritten");
                request.intents[1].payload = value.to_string();
            }
        }
        assert!(
            f.storage.handle().activate_recovery_claim(request).is_err(),
            "{damage}"
        );
        f.assert_prepared(&claim);
    }
}

#[test]
fn same_incarnation_stop_and_terminal_endpoint_changes_fence_the_whole_component() {
    for terminal in [false, true] {
        let f = Fixture::new();
        let request = f.seed(terminal);
        let claim = f.claim(&request);
        let mut row = f.db().resume_intent("run-b").unwrap().unwrap();
        let mut value: Value = serde_json::from_str(&row.payload).unwrap();
        value["active"] = json!(false);
        value["stop_reason"] = json!("operator_stopped");
        row.payload = value.to_string();
        f.storage
            .handle()
            .save_resume_intent(row, Some("old".into()))
            .unwrap();
        assert!(f.storage.handle().activate_recovery_claim(request).is_err());
        assert_eq!(
            f.db().resume_intent("run-a").unwrap().unwrap().incarnation,
            "old"
        );
        assert_eq!(f.db().recovery_claims().unwrap(), [claim]);
    }
}

#[test]
fn descriptor_and_receipt_are_independent_fences_including_other_restore_reservations() {
    for damage in [
        "consumed",
        "reservation",
        "descriptor",
        "receipt",
        "tombstone",
    ] {
        let f = Fixture::new();
        let request = f.seed(false);
        let claim = f.claim(&request);
        let sql = match damage {
            "consumed" => {
                r#"UPDATE run_contexts SET config_json='{"non_restorable_reason":"snapshot_consumed"}' WHERE run_id='run-b'"#
            }
            "reservation" => {
                r#"UPDATE run_contexts SET config_json='{"admission_reservation_id":"other"}' WHERE run_id='run-b'"#
            }
            "descriptor" => {
                r#"UPDATE run_contexts SET config_json='{"changed":true}' WHERE run_id='run-b'"#
            }
            "receipt" => "UPDATE run_contexts SET updated_at_ns=2 WHERE run_id='run-b'",
            _ => {
                "INSERT INTO run_tombstones(run_id,thread_id,family_key) VALUES('run-b','thread','family')"
            }
        };
        f.sql().execute_batch(sql).unwrap();
        assert!(
            f.storage.handle().activate_recovery_claim(request).is_err(),
            "{damage}"
        );
        assert_eq!(
            f.db().resume_intent("run-a").unwrap().unwrap().incarnation,
            "old"
        );
        assert_eq!(f.db().recovery_claims().unwrap(), [claim]);
    }
}

#[test]
fn terminal_observation_endpoint_is_not_admitted() {
    let f = Fixture::new();
    let request = f.seed(true);
    let before = f.db().resume_intent("run-b").unwrap();
    f.claim(&request);
    f.storage.handle().activate_recovery_claim(request).unwrap();
    assert_eq!(f.db().resume_intent("run-b").unwrap(), before);
    assert_eq!(f.db().resume_intent_revision("run-b").unwrap(), Some(0));
}

#[test]
fn release_preserves_revision_and_prevents_reclaiming_the_same_manifest() {
    let f = Fixture::new();
    let request = f.seed(false);
    f.claim(&request);
    let released = f
        .storage
        .handle()
        .release_recovery_claim("claim".into(), "operator".into())
        .unwrap();
    assert_eq!(
        released.state,
        RecoveryClaimState::Released {
            reason: "operator".into()
        }
    );
    assert_eq!(f.db().resume_intent_revision("run-a").unwrap(), Some(1));
    assert!(
        f.storage
            .handle()
            .activate_recovery_claim(request.clone())
            .is_err()
    );
    assert!(
        f.storage
            .handle()
            .claim_recovery_manifest(RecoveryClaimRequest {
                claim_id: "another".into(),
                manifest_id: "component".into(),
                incarnations: request
                    .intents
                    .iter()
                    .map(|r| (r.run_id.clone(), r.incarnation.clone()))
                    .collect(),
            })
            .is_err()
    );
    f.storage.close();
    assert_eq!(
        Database::open(&f.config)
            .unwrap()
            .recovery_claims()
            .unwrap(),
        [released]
    );
}

#[test]
fn ack_observation_uses_intents_even_without_claim_and_never_writes() {
    let f = Fixture::new();
    let request = f.seed(false);
    let claim = f.claim(&request);
    f.storage.handle().activate_recovery_claim(request).unwrap();
    f.sql()
        .execute_batch("PRAGMA foreign_keys=ON; DELETE FROM recovery_claims")
        .unwrap();
    let db = f.db();
    let before = db.resume_intents().unwrap();
    assert_eq!(
        db.observe_recovery_activation(&claim).unwrap(),
        RecoveryActivationObservation::Activated
    );
    assert_eq!(db.resume_intents().unwrap(), before);
    f.sql()
        .execute_batch("UPDATE resume_intents SET incarnation='unrelated' WHERE run_id='run-b'")
        .unwrap();
    assert_eq!(
        db.observe_recovery_activation(&claim).unwrap(),
        RecoveryActivationObservation::Indeterminate
    );
}

#[test]
fn manual_continue_after_release_fences_revision_and_snapshot_in_one_transaction() {
    use storage::thread_index::*;
    let f = Fixture::new();
    let request = f.seed(false);
    f.claim(&request);
    f.storage
        .handle()
        .release_recovery_claim("claim".into(), "operator".into())
        .unwrap();
    let before = f.db().run_context("run-a").unwrap().unwrap();
    let mut record = before.clone();
    record.messages_json = "[]".into();
    record.updated_at_ns += 1;
    let write = SnapshotWrite {
        record,
        expected_snapshot_revision: None,
        binding: None,
        status: ProvenanceStatus::Verified {
            evidence: ProvenanceEvidence::CompletePositionMap,
        },
        map: ProvenanceMap {
            version: PROVENANCE_FORMAT_VERSION,
            raw_message_count: 0,
            checkpoint_count: 0,
            messages: vec![],
        },
    };
    let row = request.intents[0].clone();
    assert!(
        f.storage
            .handle()
            .save_snapshot_bundle_with_fenced_resume_intent(&write, row.clone(), "old".into(), 0)
            .is_err()
    );
    assert_eq!(f.db().run_context("run-a").unwrap(), Some(before));
    assert_eq!(
        f.db().resume_intent("run-a").unwrap().unwrap().incarnation,
        "old"
    );
    f.storage
        .handle()
        .save_snapshot_bundle_with_fenced_resume_intent(&write, row, "old".into(), 1)
        .unwrap();
    assert_eq!(f.db().resume_intent_revision("run-a").unwrap(), Some(2));
    assert_eq!(
        f.db().run_context("run-a").unwrap().unwrap().messages_json,
        "[]"
    );
}

#[test]
fn human_continue_evidence_bundle_rolls_back_when_the_release_revision_changed() {
    use storage::thread_index::*;
    let f = Fixture::new();
    let request = f.seed(false);
    f.claim(&request);
    f.storage
        .handle()
        .release_recovery_claim("claim".into(), "human".into())
        .unwrap();
    let before = f.db().run_context("run-a").unwrap().unwrap();
    let mut record = before.clone();
    record.messages_json = "[]".into();
    record.updated_at_ns += 1;
    let write = SnapshotWrite {
        record,
        expected_snapshot_revision: None,
        binding: None,
        status: ProvenanceStatus::Verified {
            evidence: ProvenanceEvidence::CompletePositionMap,
        },
        map: ProvenanceMap {
            version: PROVENANCE_FORMAT_VERSION,
            raw_message_count: 0,
            checkpoint_count: 0,
            messages: vec![],
        },
    };
    let fence = storage::resume_intent::RecoveryContinueFence {
        record: request.intents[0].clone(),
        expected_incarnation: "old".into(),
        expected_revision: 0,
    };
    let mut wrong_identity = fence.clone();
    wrong_identity.record.run_id = "run-b".into();
    assert!(
        f.storage
            .handle()
            .save_recovery_continue(&write, &[], None, &Default::default(), wrong_identity,)
            .is_err()
    );
    assert!(
        f.storage
            .handle()
            .save_recovery_continue(&write, &[], None, &Default::default(), fence.clone(),)
            .is_err()
    );
    assert_eq!(f.db().run_context("run-a").unwrap(), Some(before));
    assert_eq!(f.db().resume_intent_revision("run-a").unwrap(), Some(1));
    f.storage
        .handle()
        .save_recovery_continue(
            &write,
            &[],
            None,
            &Default::default(),
            storage::resume_intent::RecoveryContinueFence {
                expected_revision: 1,
                ..fence
            },
        )
        .unwrap();
    assert_eq!(f.db().resume_intent_revision("run-a").unwrap(), Some(2));
    assert_eq!(
        f.db().run_context("run-a").unwrap().unwrap().messages_json,
        "[]"
    );
}

#[cfg(unix)]
#[test]
fn startup_evidence_is_bound_to_both_leases_and_keeps_them_shared() {
    use std::time::Duration;
    let f = Fixture::new();
    let root = f._dir.path().join("ownership");
    let leases = storage::shared_with_wait(&f.config.db_path, &root, Duration::ZERO).unwrap();
    let proof = leases
        .recovery_startup_complete(&f.config.db_path, &root)
        .unwrap();
    let request = f.seed(false);
    let other = Fixture::new();
    let other_root = other._dir.path().join("ownership");
    let other_leases =
        storage::shared_with_wait(&other.config.db_path, &other_root, Duration::ZERO).unwrap();
    assert!(
        leases
            .recovery_startup_complete(&other.config.db_path, &root)
            .is_err()
    );
    assert!(
        leases
            .recovery_startup_complete(&f.config.db_path, &other_root)
            .is_err()
    );
    let wrong = other_leases
        .recovery_startup_complete(&other.config.db_path, &other_root)
        .unwrap();
    assert!(
        f.storage
            .handle()
            .claim_recovery_component(
                RecoveryClaimRequest {
                    claim_id: "claim".into(),
                    manifest_id: "component".into(),
                    incarnations: request
                        .intents
                        .iter()
                        .map(|r| (r.run_id.clone(), r.incarnation.clone()))
                        .collect(),
                },
                &wrong,
                &root
            )
            .is_err()
    );
    f.storage
        .handle()
        .claim_recovery_component(
            RecoveryClaimRequest {
                claim_id: "claim".into(),
                manifest_id: "component".into(),
                incarnations: request
                    .intents
                    .iter()
                    .map(|r| (r.run_id.clone(), r.incarnation.clone()))
                    .collect(),
            },
            &proof,
            &root,
        )
        .unwrap();
    drop(leases);
    f.storage.close();
    assert!(
        storage::try_exclusive(&f.config.db_path, &root)
            .unwrap()
            .is_none()
    );
    drop(proof);
    assert!(
        storage::try_exclusive(&f.config.db_path, &root)
            .unwrap()
            .is_some()
    );
}
