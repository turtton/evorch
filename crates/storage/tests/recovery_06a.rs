//! PR 06a persistence boundaries; no wall-clock waits or provider fixtures.
use serde_json::json;
use storage::{Database, Storage, StorageConfig, recovery::*, resume_intent::ResumeIntentRecord};
struct Fixture {
    dir: tempfile::TempDir,
    config: StorageConfig,
    storage: Storage,
}
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let config = StorageConfig {
            db_path: dir.path().join("recovery.db"),
            ..Default::default()
        };
        let storage = Storage::open(config.clone()).unwrap();
        Self {
            dir,
            config,
            storage,
        }
    }
    fn sql(&self) -> rusqlite::Connection {
        rusqlite::Connection::open(&self.config.db_path).unwrap()
    }
    fn intent(&self, id: &str) -> ResumeIntentRecord {
        ResumeIntentRecord{run_id:id.into(),incarnation:"old".into(),payload:json!({"run_id":id,"incarnation":"old","active":true,"stop_reason":null,"tool_intent":[],"initial_input":{"id":"initial"},"wait":{"kind":"admission_queued"}}).to_string()}
    }
    fn seed(&self, id: &str) {
        self.storage
            .handle()
            .save_resume_intent(self.intent(id), None)
            .unwrap();
    }
    fn manifest(&self, id: &str, runs: &[&str]) -> RecoveryManifestRecord {
        RecoveryManifestRecord {
            manifest_id: id.into(),
            schema_version: 1,
            committed: false,
            superseded: false,
            edges: runs
                .windows(2)
                .map(|p| RecoveryEdge {
                    from: p[0].into(),
                    to: p[1].into(),
                    kind: "parent".into(),
                })
                .collect(),
            members: runs
                .iter()
                .map(|run| RecoveryMemberRecord {
                    run_id: (*run).into(),
                    incarnation: "old".into(),
                    intent_revision: 0,
                    snapshot: None,
                    resume: true,
                    record_json: json!({"run_id":run,"recipe":"current-settings-reference"})
                        .to_string(),
                })
                .collect(),
        }
    }
    fn commit(&self, id: &str, runs: &[&str]) {
        for run in runs {
            self.seed(run);
        }
        self.storage
            .handle()
            .prepare_recovery_manifest(self.manifest(id, runs))
            .unwrap();
        self.storage
            .handle()
            .commit_recovery_manifest(id.into())
            .unwrap();
    }
    fn claim(&self, id: &str, runs: &[&str]) -> RecoveryClaimRequest {
        RecoveryClaimRequest {
            claim_id: format!("claim-{id}"),
            manifest_id: id.into(),
            incarnations: runs
                .iter()
                .map(|r| ((*r).into(), format!("new-{r}")))
                .collect(),
        }
    }
}
#[test]
fn prepared_claim_survives_reopen_without_mutating_intent_or_consuming_snapshot() {
    let f = Fixture::new();
    f.commit("component", &["run-1", "run-2"]);
    let before = Database::open(&f.config).unwrap().resume_intents().unwrap();
    let claim = f
        .storage
        .handle()
        .claim_recovery_manifest(f.claim("component", &["run-1", "run-2"]))
        .unwrap();
    assert_eq!(claim.state, RecoveryClaimState::Prepared);
    assert!(
        f.storage
            .handle()
            .claim_recovery_manifest(f.claim("component", &["run-1", "run-2"]))
            .is_err()
    );
    f.storage
        .handle()
        .validate_recovery_claim(claim.claim_id.clone())
        .unwrap();
    f.storage.close();
    let db = Database::open(&f.config).unwrap();
    assert_eq!(db.resume_intents().unwrap(), before);
    assert_eq!(db.resume_intent_revision("run-1").unwrap(), Some(1));
    assert_eq!(db.recovery_claims().unwrap(), vec![claim]);
    assert_eq!(db.recovery_manifests().unwrap().len(), 1);
}
#[test]
fn same_incarnation_stop_conflicts_with_claim_and_activation_revalidation() {
    let f = Fixture::new();
    f.commit("before", &["run-1", "run-2"]);
    let mut stopped = f.intent("run-2");
    let mut value: serde_json::Value = serde_json::from_str(&stopped.payload).unwrap();
    value["stop_reason"] = json!("operator_stopped");
    stopped.payload = value.to_string();
    f.storage
        .handle()
        .save_resume_intent(stopped, Some("old".into()))
        .unwrap();
    assert!(
        f.storage
            .handle()
            .claim_recovery_manifest(f.claim("before", &["run-1", "run-2"]))
            .is_err()
    );
    assert_eq!(
        Database::open(&f.config)
            .unwrap()
            .resume_intent_revision("run-1")
            .unwrap(),
        Some(0)
    );
    assert!(
        Database::open(&f.config)
            .unwrap()
            .recovery_claims()
            .unwrap()
            .is_empty()
    );
    f.commit("after", &["run-3"]);
    let c = f
        .storage
        .handle()
        .claim_recovery_manifest(f.claim("after", &["run-3"]))
        .unwrap();
    let mut stopped = f.intent("run-3");
    let mut v: serde_json::Value = serde_json::from_str(&stopped.payload).unwrap();
    v["stop_reason"] = json!("cancelled");
    stopped.payload = v.to_string();
    f.storage
        .handle()
        .save_resume_intent(stopped, Some("old".into()))
        .unwrap();
    assert!(
        f.storage
            .handle()
            .validate_recovery_claim(c.claim_id)
            .is_err()
    );
}
#[test]
fn failed_prepare_rolls_back_and_failed_commit_remains_partial() {
    let f = Fixture::new();
    f.seed("run-1");
    f.seed("run-2");
    f.sql().execute_batch("CREATE TRIGGER reject_graph BEFORE INSERT ON recovery_members WHEN NEW.run_id='run-2' BEGIN SELECT RAISE(ABORT,'injected save failure'); END;").unwrap();
    assert!(
        f.storage
            .handle()
            .prepare_recovery_manifest(f.manifest("failed", &["run-1", "run-2"]))
            .is_err()
    );
    assert!(
        Database::open(&f.config)
            .unwrap()
            .recovery_manifests()
            .unwrap()
            .is_empty()
    );
    f.sql().execute_batch("DROP TRIGGER reject_graph").unwrap();
    f.storage
        .handle()
        .prepare_recovery_manifest(f.manifest("partial", &["run-1"]))
        .unwrap();
    f.storage
        .handle()
        .save_resume_intent(f.intent("run-1"), Some("old".into()))
        .unwrap();
    assert!(
        f.storage
            .handle()
            .commit_recovery_manifest("partial".into())
            .is_err()
    );
    let rows = Database::open(&f.config)
        .unwrap()
        .recovery_manifests()
        .unwrap();
    assert!(!rows[0].record.as_ref().unwrap().committed);
}
#[test]
fn new_capture_replaces_partial_and_supersedes_only_overlapping_committed_component() {
    let f = Fixture::new();
    f.commit("old", &["run-1"]);
    f.commit("healthy", &["run-2"]);
    f.storage
        .handle()
        .prepare_recovery_manifest(f.manifest("abandoned", &["run-1"]))
        .unwrap();
    f.storage
        .handle()
        .prepare_recovery_manifest(f.manifest("replacement", &["run-1"]))
        .unwrap();
    f.storage
        .handle()
        .commit_recovery_manifest("replacement".into())
        .unwrap();
    let rows = Database::open(&f.config)
        .unwrap()
        .recovery_manifests()
        .unwrap();
    assert_eq!(rows.len(), 3);
    assert!(!rows.iter().any(|r| r.manifest_id == "abandoned"));
    assert!(
        rows.iter()
            .find(|r| r.manifest_id == "old")
            .unwrap()
            .record
            .as_ref()
            .unwrap()
            .superseded
    );
    assert!(
        !rows
            .iter()
            .find(|r| r.manifest_id == "healthy")
            .unwrap()
            .record
            .as_ref()
            .unwrap()
            .superseded
    );
}
#[test]
fn reopened_corruption_isolated_by_component_for_payload_attribute_and_missing_edge() {
    for damage in [
        "UPDATE recovery_members SET record_json='broken' WHERE manifest_id='bad'",
        "UPDATE recovery_members SET intent_revision='broken' WHERE manifest_id='bad'",
        "UPDATE recovery_manifests SET edges_json='[]' WHERE manifest_id='bad'",
    ] {
        let f = Fixture::new();
        f.commit("bad", &["run-1", "run-2"]);
        f.commit("healthy", &["run-3"]);
        f.storage.close();
        rusqlite::Connection::open(&f.config.db_path)
            .unwrap()
            .execute(damage, [])
            .unwrap();
        let db = Database::open(&f.config).unwrap();
        let rows = db.recovery_manifests().unwrap();
        assert_eq!(rows.len(), 2);
        assert!(
            rows.iter()
                .find(|r| r.manifest_id == "healthy")
                .unwrap()
                .record
                .is_ok()
        );
        let bad = &rows.iter().find(|r| r.manifest_id == "bad").unwrap().record;
        if damage.contains("record_json") {
            assert_eq!(bad.as_ref().unwrap().members[0].record_json, "broken");
        } else {
            assert!(bad.is_err());
        }
    }
}
#[test]
fn tombstone_fences_capture_claim_and_prepared_claim_revalidation() {
    let f = Fixture::new();
    f.commit("ready", &["run-1"]);
    let claim = f
        .storage
        .handle()
        .claim_recovery_manifest(f.claim("ready", &["run-1"]))
        .unwrap();
    f.sql().execute("INSERT INTO run_tombstones(run_id,thread_id,family_key) VALUES('run-1','thread','family')",[]).unwrap();
    assert!(
        f.storage
            .handle()
            .validate_recovery_claim(claim.claim_id)
            .is_err()
    );
    assert!(
        f.storage
            .handle()
            .prepare_recovery_manifest(f.manifest("deleted", &["run-1"]))
            .is_err()
    );
    assert!(
        f.storage
            .handle()
            .claim_recovery_manifest(f.claim("ready", &["run-1"]))
            .is_err()
    );
}

#[test]
fn snapshot_receipt_is_preserved_by_claim_and_late_missing_consumed_or_replaced_snapshot_blocks() {
    for damage in ["missing", "consumed", "replaced"] {
        let f = Fixture::new();
        let snapshot = storage::RunContextRecord {
            run_id: "run-1".into(),
            role: "Worker".into(),
            name: "snapshot".into(),
            parent_run_id: None,
            config_json: "{}".into(),
            messages_json: json!([{"role":"user","content":[]}]).to_string(),
            checkpoints_json: "[]".into(),
            terminal_phase: "running".into(),
            restorable: false,
            updated_at_ns: 1,
            host_inputs_json: "[]".into(),
        };
        let receipt = f
            .storage
            .handle()
            .upsert_run_context_receipt(&snapshot)
            .unwrap();
        let mut intent = f.intent("run-1");
        let mut value: serde_json::Value = serde_json::from_str(&intent.payload).unwrap();
        let mut reference = serde_json::to_value(&receipt).unwrap();
        reference["kind"] = json!("run_context");
        value["checkpoint"] = json!({"snapshot":reference});
        intent.payload = value.to_string();
        f.storage.handle().save_resume_intent(intent, None).unwrap();
        let mut manifest = f.manifest("snapshot", &["run-1"]);
        manifest.members[0].snapshot = Some(receipt.clone());
        f.storage
            .handle()
            .prepare_recovery_manifest(manifest)
            .unwrap();
        f.storage
            .handle()
            .commit_recovery_manifest("snapshot".into())
            .unwrap();
        let claim = f
            .storage
            .handle()
            .claim_recovery_manifest(f.claim("snapshot", &["run-1"]))
            .unwrap();
        f.storage
            .handle()
            .validate_recovery_claim(claim.claim_id.clone())
            .unwrap();
        assert_eq!(
            Database::open(&f.config)
                .unwrap()
                .run_context("run-1")
                .unwrap()
                .unwrap()
                .messages_json,
            snapshot.messages_json
        );
        assert!(
            receipt.matches(
                &Database::open(&f.config)
                    .unwrap()
                    .run_context("run-1")
                    .unwrap()
                    .unwrap()
            )
        );
        match damage {
            "missing" => {
                f.sql()
                    .execute_batch(
                        "PRAGMA foreign_keys=OFF; DELETE FROM run_contexts WHERE run_id='run-1'",
                    )
                    .unwrap();
            }
            "consumed" => {
                f.sql().execute("UPDATE run_contexts SET config_json='{\"non_restorable_reason\":\"snapshot_consumed\"}' WHERE run_id='run-1'", []).unwrap();
            }
            _ => {
                let mut replacement = snapshot.clone();
                replacement.messages_json = "[]".into();
                replacement.updated_at_ns += 1;
                f.storage.handle().upsert_run_context(&replacement).unwrap();
            }
        }
        assert!(
            f.storage
                .handle()
                .validate_recovery_claim(claim.claim_id)
                .is_err(),
            "{damage}"
        );
    }
}

#[test]
fn non_resume_members_allow_interrupted_tools_and_consumed_snapshot_but_keep_receipt_fencing() {
    for resume in [false, true] {
        for evidence in ["tool", "consumed", "replaced"] {
            let f = Fixture::new();
            let snapshot = storage::RunContextRecord {
                run_id: "run-1".into(),
                role: "Worker".into(),
                name: "snapshot".into(),
                parent_run_id: None,
                config_json: if evidence == "consumed" {
                    "{\"non_restorable_reason\":\"snapshot_consumed\"}"
                } else {
                    "{}"
                }
                .into(),
                messages_json: "[]".into(),
                checkpoints_json: "[]".into(),
                terminal_phase: "stopped".into(),
                restorable: false,
                updated_at_ns: 1,
                host_inputs_json: "[]".into(),
            };
            let receipt = f
                .storage
                .handle()
                .upsert_run_context_receipt(&snapshot)
                .unwrap();
            let mut intent = f.intent("run-1");
            let mut value: serde_json::Value = serde_json::from_str(&intent.payload).unwrap();
            value["active"] = json!(resume);
            value["stop_reason"] = if resume {
                json!(null)
            } else {
                json!("operator_stopped")
            };
            if evidence == "tool" {
                value["tool_intent"] = json!([{"id":"interrupted"}]);
            }
            let mut reference = serde_json::to_value(&receipt).unwrap();
            reference["kind"] = json!("run_context");
            value["checkpoint"] = json!({"snapshot":reference});
            intent.payload = value.to_string();
            f.storage.handle().save_resume_intent(intent, None).unwrap();
            let mut manifest = f.manifest("terminal-evidence", &["run-1"]);
            manifest.members[0].resume = resume;
            manifest.members[0].snapshot = Some(receipt);
            f.storage
                .handle()
                .prepare_recovery_manifest(manifest)
                .unwrap();
            if evidence == "replaced" {
                let mut replacement = snapshot.clone();
                replacement.messages_json = "[{\"different\":true}]".into();
                f.storage
                    .handle()
                    .upsert_run_context_receipt(&replacement)
                    .unwrap();
            }
            let result = f
                .storage
                .handle()
                .commit_recovery_manifest("terminal-evidence".into());
            assert_eq!(
                result.is_ok(),
                !resume && evidence != "replaced",
                "{resume}/{evidence}: {result:?}"
            );
        }
    }
}

#[test]
fn superseding_a_committed_component_does_not_reapply_its_old_tombstone_commit_fence() {
    let f = Fixture::new();
    f.commit("old", &["run-1", "run-2"]);
    f.sql().execute("INSERT INTO run_tombstones(run_id,thread_id,family_key) VALUES('run-2','thread','family')", []).unwrap();
    f.storage
        .handle()
        .prepare_recovery_manifest(f.manifest("new", &["run-1"]))
        .unwrap();
    f.storage
        .handle()
        .commit_recovery_manifest("new".into())
        .unwrap();
    let manifests = Database::open(&f.config)
        .unwrap()
        .recovery_manifests()
        .unwrap();
    assert!(
        manifests
            .iter()
            .any(|m| m.manifest_id == "old" && m.record.as_ref().unwrap().superseded)
    );
    assert!(
        manifests
            .iter()
            .any(|m| m.manifest_id == "new" && m.record.as_ref().unwrap().committed)
    );
}

#[test]
fn retention_of_one_member_erases_component_claim_and_cross_member_delivery_copies() {
    for count in [2, 200] {
        let f = Fixture::new();
        let runs: Vec<_> = (0..count).map(|n| format!("run-{n:03}")).collect();
        let ids: Vec<_> = runs.iter().map(String::as_str).collect();
        f.commit("deleted-component", &ids);
        f.commit("healthy", &["survivor"]);
        for run in &runs {
            let deliveries: Vec<_> = (0..20).map(|n| json!({"message_id":format!("{run}-{n}"),"body":"copied-delivery-marker","sender":runs[0]})).collect();
            let record = json!({"run_id":run,"deliveries":deliveries}).to_string();
            f.sql()
                .execute(
                    "UPDATE recovery_members SET record_json=?1 WHERE run_id=?2",
                    rusqlite::params![record, run],
                )
                .unwrap();
        }
        f.storage
            .handle()
            .claim_recovery_manifest(f.claim("deleted-component", &ids))
            .unwrap();
        f.sql()
            .execute_batch(
                "INSERT INTO thread_tombstones VALUES('deleted-thread','deleted-family',1);
             INSERT INTO run_tombstones VALUES('run-000','deleted-thread','deleted-family');
             INSERT INTO reclaim_jobs(family_key,state,created_at_ns,updated_at_ns)
             VALUES('deleted-family','db_pending',1,1);",
            )
            .unwrap();
        let mut finished = false;
        // A structural step budget catches a repeated cursor without wall-clock conditions.
        for _ in 0..1000 {
            let progress = f
                .storage
                .handle()
                .reclaim_step(storage::thread_retention::ReclaimBudget {
                    max_transactions: 1,
                })
                .unwrap();
            assert_eq!(progress.last_error, None);
            if progress.jobs_db_pending == 0 {
                finished = true;
                break;
            }
        }
        assert!(finished, "{count} members did not finish reclaim");
        let db = Database::open(&f.config).unwrap();
        assert_eq!(
            db.recovery_manifests()
                .unwrap()
                .iter()
                .map(|r| r.manifest_id.as_str())
                .collect::<Vec<_>>(),
            ["healthy"]
        );
        assert!(db.recovery_claims().unwrap().is_empty());
        let copies: i64 = f.sql().query_row("SELECT COUNT(*) FROM recovery_members WHERE record_json LIKE '%copied-delivery-marker%'", [], |r|r.get(0)).unwrap();
        assert_eq!(copies, 0);
    }
}
#[test]
fn fresh_upgrade_and_undo_v61_preserve_previous_intents() {
    let f = Fixture::new();
    f.seed("run-1");
    f.storage.close();
    let sql = rusqlite::Connection::open(&f.config.db_path).unwrap();
    assert_eq!(
        sql.pragma_query_value(None, "user_version", |r| r.get::<_, u32>(0))
            .unwrap(),
        61
    );
    let package = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let undo =
        std::fs::read_to_string(std::path::Path::new(&package).join("undo/undo_v61.sql")).unwrap();
    sql.execute_batch(&undo).unwrap();
    sql.pragma_update(None, "user_version", 60).unwrap();
    drop(sql);
    let storage = Storage::open(f.config.clone()).unwrap();
    let db = Database::open(&f.config).unwrap();
    assert_eq!(db.resume_intent_revision("run-1").unwrap(), Some(0));
    assert_eq!(db.resume_intents().unwrap().len(), 1);
    assert!(db.recovery_manifests().unwrap().is_empty());
    storage.close();
    assert!(f.dir.path().is_dir());
}
