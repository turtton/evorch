//! A quarantined cold graph built only through public storage entry points.
use serde_json::{Value, json};
use storage::{
    Database, RunContextRecord, Storage, StorageConfig,
    recovery::{graph_progress::*, restart::*, *},
    resume_intent::ResumeIntentRecord,
};

pub const TX: &str = "restart";
pub const CAPTURE: &str = "capture";
pub const MANIFEST: &str = "component";

pub struct Fixture {
    pub _dir: tempfile::TempDir,
    pub config: StorageConfig,
    pub storage: Option<Storage>,
}
impl Fixture {
    pub fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let config = StorageConfig {
            db_path: dir.path().join("quarantine.db"),
            ..Default::default()
        };
        let storage = Some(Storage::open(config.clone()).unwrap());
        Self {
            _dir: dir,
            config,
            storage,
        }
    }
    pub fn handle(&self) -> storage::StorageHandle {
        self.storage.as_ref().unwrap().handle()
    }
    pub fn db(&self) -> Database {
        Database::open(&self.config).unwrap()
    }
    pub fn sql(&self) -> rusqlite::Connection {
        rusqlite::Connection::open(&self.config.db_path).unwrap()
    }
    /// Closes the writer and opens everything again, as a process restart does.
    pub fn reload(&mut self) {
        drop(self.storage.take());
        self.storage = Some(Storage::open(self.config.clone()).unwrap());
    }
    /// A committed two-member manifest, optionally with a prepared claim.
    pub fn seed(&self, claimed: bool) -> RecoveryActivationRequest {
        let mut members = Vec::new();
        let mut snapshots = Vec::new();
        let mut intents = Vec::new();
        for id in ["run-a", "run-b"] {
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
            let receipt = self.handle().upsert_run_context_receipt(&record).unwrap();
            let mut value = json!({
                "run_id":id, "incarnation":"old", "parent_run_id":null,"role":"Worker",
                "active":true,"stop_reason":Value::Null,
                "tool_intent":[], "snapshot":receipt, "provider_input":{"prefix":["sent"]},
                "continuation":{"call_order":["first","second"]}
            });
            self.handle()
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
                resume: true,
                record_json:
                    json!({"run_id":id,"old_incarnation":"old","parent":null,"role":"Worker"})
                        .to_string(),
            });
            snapshots.push(RecoverySnapshotExpectation {
                run_id: id.into(),
                receipt,
                config_json: record.config_json,
            });
            value["incarnation"] = json!(format!("new-{id}"));
            intents.push(ResumeIntentRecord {
                run_id: id.into(),
                incarnation: format!("new-{id}"),
                payload: value.to_string(),
            });
        }
        self.handle()
            .prepare_recovery_manifest(RecoveryManifestRecord {
                manifest_id: MANIFEST.into(),
                schema_version: 1,
                committed: false,
                superseded: false,
                edges: vec![RecoveryEdge {
                    from: "run-a".into(),
                    to: "run-b".into(),
                    kind: "wait".into(),
                }],
                members: members.clone(),
            })
            .unwrap();
        self.handle()
            .commit_recovery_manifest(MANIFEST.into())
            .unwrap();
        let request = RecoveryActivationRequest {
            reservation: storage::recovery::graph_control::ActivationControl::Automatic,
            claim_id: "claim".into(),
            intents,
            snapshots,
        };
        if claimed {
            self.handle()
                .claim_recovery_manifest(RecoveryClaimRequest {
                    claim_id: "claim".into(),
                    manifest_id: MANIFEST.into(),
                    incarnations: request
                        .intents
                        .iter()
                        .map(|r| (r.run_id.clone(), r.incarnation.clone()))
                        .collect(),
                })
                .unwrap();
        }
        request
    }
    /// A transaction with the one manifest as a retry-pending component.
    pub fn transaction(&self, threads: bool) {
        let metadata = RestartMetadata {
            restart_id: TX.into(),
            capture_id: CAPTURE.into(),
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
        self.handle()
            .mutate_restart(TX.into(), None, RestartMutation::Create(Box::new(metadata)))
            .unwrap();
        let sql = self.sql();
        sql.execute(
            "INSERT INTO restart_components(restart_id,manifest_id,disposition) VALUES(?1,?2,'retry_pending')",
            [TX, MANIFEST],
        )
        .unwrap();
        for run in ["run-a", "run-b"] {
            let member = RestartExpectedMember {
                run_id: run.into(),
                incarnation: "old".into(),
                root_run_id: run.into(),
                thread_id: threads.then(|| run.into()),
                intent_revision: 0,
                snapshot: None,
                owner_id: None,
                owner_generation: None,
                lane_id: None,
            };
            sql.execute(
                "INSERT INTO restart_expected_members(restart_id,run_id,root_run_id,member_json,manifest_id) VALUES(?1,?2,?2,?3,?4)",
                rusqlite::params![TX, run, serde_json::to_string(&member).unwrap(), MANIFEST],
            )
            .unwrap();
        }
    }
    /// The production cold-boot transition: the tx ends and the component is quarantined.
    pub fn cold_quarantine(claimed: bool) -> (Self, RecoveryActivationRequest) {
        Self::quarantine(claimed, true)
    }
    /// Members that belong to no thread: no owner or scratch can remain.
    pub fn cold_quarantine_threadless(claimed: bool) -> (Self, RecoveryActivationRequest) {
        Self::quarantine(claimed, false)
    }
    fn quarantine(claimed: bool, threads: bool) -> (Self, RecoveryActivationRequest) {
        let f = Self::new();
        let activation = f.seed(claimed);
        f.transaction(threads);
        let (revision, _) = f.db().restart_revision_phase(TX).unwrap().unwrap();
        f.handle()
            .quarantine_cold_restart(TX.into(), revision)
            .unwrap();
        (f, activation)
    }
    /// What a disposal records through the production progress write, per thread.
    pub fn record_cleanup(&self, owners: GraphDisposalStep, scratch: GraphDisposalStep) {
        let control = self.db().recovery_graph_control(MANIFEST).unwrap().unwrap();
        for thread in ["run-a", "run-b"] {
            self.handle()
                .write_recovery_graph_progress(GraphThreadProgressWrite {
                    key: GraphDisposalProgressKey {
                        manifest_id: MANIFEST.into(),
                        restart_id: TX.into(),
                        capture_id: CAPTURE.into(),
                        thread_id: thread.into(),
                        owner_generation: 1,
                    },
                    expected_revision: 0,
                    control: control.clone(),
                    owners,
                    scratch,
                })
                .unwrap();
        }
    }
    pub fn graph(&self) -> quarantine::QuarantinedGraph {
        self.db()
            .quarantined_graph(TX, MANIFEST)
            .unwrap()
            .expect("quarantined graph")
    }
    pub fn request(&self, operation: &str) -> quarantine::DiscardRequest {
        quarantine::DiscardRequest {
            restart_id: TX.into(),
            capture_id: CAPTURE.into(),
            manifest_id: MANIFEST.into(),
            expected_revision: self.graph().revision,
            operation_id: operation.into(),
            attempt_nonce: format!("nonce-{operation}"),
        }
    }
    pub fn control_rows(&self) -> i64 {
        self.sql()
            .query_row("SELECT COUNT(*) FROM recovery_graph_control", [], |r| {
                r.get(0)
            })
            .unwrap()
    }
    pub fn effect_rows(&self) -> i64 {
        self.sql()
            .query_row("SELECT COUNT(*) FROM recovery_graph_effects", [], |r| {
                r.get(0)
            })
            .unwrap()
    }
}
