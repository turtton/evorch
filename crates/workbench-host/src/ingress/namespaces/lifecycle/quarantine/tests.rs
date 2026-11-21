//! Q1 over the real admin service, real audit journal and real storage. Faults
//! are injected only at the store seam, and every synchronization is an event
//! (a reply, the admission slots joining), never a sleep.
use super::*;
use crate::{
    admin::*, areas::tasks::AreaTasks, bootstrap::restart::HostRestartGate,
    bootstrap::services::BackgroundServices,
};
use serde_json::{Value, json};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use storage::{
    Database, RunContextRecord, Storage, StorageConfig, StorageError,
    admin_audit::AuditOutcome,
    recovery::{
        RecoveryClaimRequest, RecoveryEdge, RecoveryManifestRecord, RecoveryMemberRecord,
        quarantine::{DiscardReport, DiscardRequest, QuarantineSnapshot},
        restart::{RestartExpectedMember, RestartMetadata, RestartMutation},
    },
    resume_intent::ResumeIntentRecord,
};
use workbench_authority::{IngressContext, test_support};
use workbench_protocol::{
    envelope::{Provenance, Surface},
    errors::ErrorCode,
    ids::ClientId,
    namespaces::lifecycle::{
        AdminId, ConfirmedGraph, GraphCas, LifecycleCommand, LifecycleQuery, LifecycleReply,
        quarantine::*,
    },
    wire::{InstanceEpoch, Revision},
};

const EPOCH: InstanceEpoch = InstanceEpoch(7);
const TX: &str = "restart";
const CAPTURE: &str = "capture";
const MANIFEST: &str = "component";

fn id(s: &str) -> AdminId {
    AdminId::new(s).unwrap()
}

/// A committed two-member manifest, a transaction, and the production cold-boot
/// transition that quarantines it.
fn seed(storage: &Storage, config: &StorageConfig, claimed: bool, threads: bool) {
    let handle = storage.handle();
    let mut members = Vec::new();
    let mut incarnations = Vec::new();
    for run in ["run-a", "run-b"] {
        let record = RunContextRecord {
            run_id: run.into(),
            role: "Worker".into(),
            name: run.into(),
            parent_run_id: None,
            config_json: "{}".into(),
            messages_json: json!([{"role":"user","content":[]}]).to_string(),
            checkpoints_json: "[]".into(),
            terminal_phase: "running".into(),
            restorable: false,
            updated_at_ns: 1,
            host_inputs_json: "[]".into(),
        };
        let receipt = handle.upsert_run_context_receipt(&record).unwrap();
        let payload = json!({
            "run_id":run,"incarnation":"old","parent_run_id":null,"role":"Worker",
            "active":true,"stop_reason":Value::Null,"tool_intent":[],"snapshot":receipt,
            "provider_input":{"prefix":["sent"]},"continuation":{"call_order":["first"]}
        });
        handle
            .save_resume_intent(
                ResumeIntentRecord {
                    run_id: run.into(),
                    incarnation: "old".into(),
                    payload: payload.to_string(),
                },
                None,
            )
            .unwrap();
        members.push(RecoveryMemberRecord {
            run_id: run.into(),
            incarnation: "old".into(),
            intent_revision: 0,
            snapshot: Some(receipt),
            resume: true,
            record_json:
                json!({"run_id":run,"old_incarnation":"old","parent":null,"role":"Worker"})
                    .to_string(),
        });
        incarnations.push((run.to_owned(), format!("new-{run}")));
    }
    handle
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
            members,
        })
        .unwrap();
    handle.commit_recovery_manifest(MANIFEST.into()).unwrap();
    if claimed {
        handle
            .claim_recovery_manifest(RecoveryClaimRequest {
                claim_id: "claim".into(),
                manifest_id: MANIFEST.into(),
                incarnations,
            })
            .unwrap();
    }
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
    handle
        .mutate_restart(TX.into(), None, RestartMutation::Create(Box::new(metadata)))
        .unwrap();
    let sql = rusqlite::Connection::open(&config.db_path).unwrap();
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
    let (revision, _) = Database::open(config)
        .unwrap()
        .restart_revision_phase(TX)
        .unwrap()
        .unwrap();
    handle.quarantine_cold_restart(TX.into(), revision).unwrap();
}

/// Delegates to the real store and counts or perturbs each call.
#[derive(Default)]
struct Hooks {
    snapshots: AtomicUsize,
    discards: AtomicUsize,
    /// Runs before the real discard: a concurrent change after inspection.
    before_discard: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    /// The real discard commits, then its acknowledgement is lost.
    lose_ack_after_commit: Mutex<bool>,
    /// The write fails before it is applied.
    fail_before_commit: Mutex<bool>,
}
struct Store {
    inner: StorageQuarantineStore,
    hooks: Arc<Hooks>,
}
impl QuarantineStore for Store {
    fn snapshot(&self, limit: usize) -> Result<QuarantineSnapshot, StorageError> {
        self.hooks.snapshots.fetch_add(1, Ordering::SeqCst);
        self.inner.snapshot(limit)
    }
    fn discard(&self, request: DiscardRequest) -> Result<DiscardReport, StorageError> {
        self.hooks.discards.fetch_add(1, Ordering::SeqCst);
        if let Some(before) = self.hooks.before_discard.lock().unwrap().take() {
            before();
        }
        if std::mem::take(&mut *self.hooks.fail_before_commit.lock().unwrap()) {
            return Err(StorageError::WriterClosed);
        }
        let report = self.inner.discard(request)?;
        if std::mem::take(&mut *self.hooks.lose_ack_after_commit.lock().unwrap()) {
            return Err(StorageError::WriterClosed);
        }
        Ok(report)
    }
}

struct Harness {
    config: StorageConfig,
    storage: Storage,
    service: Arc<AdminService>,
    tasks: AreaTasks,
    services: BackgroundServices,
    hooks: Arc<Hooks>,
    context: IngressContext,
    _lease: test_support::Lease,
}
impl Harness {
    fn open(config: StorageConfig) -> Self {
        let storage = Storage::open(config.clone()).unwrap();
        let tasks = AreaTasks::new(tokio::runtime::Handle::current());
        let service = AdminService::new(
            AdminAudit::new(Arc::new(storage.handle()), HostRestartGate::default()),
            tasks.clone(),
            EPOCH,
        );
        service.enable_management();
        let hooks = Arc::new(Hooks::default());
        service.install_quarantine(QuarantineFacade::new(
            Arc::new(Store {
                inner: StorageQuarantineStore::new(config.clone(), storage.handle()),
                hooks: hooks.clone(),
            }),
            service.reader().clone(),
            EPOCH,
        ));
        service.install_operation_reader(Arc::new(config.clone()));
        let mut services = BackgroundServices::default();
        service.register_worker(&mut services);
        let (proof, lease) = test_support::local();
        let context = IngressContext::new(
            Provenance::human(Surface::Desktop, ClientId::new("operator").unwrap()),
            proof,
        );
        Self {
            config,
            storage,
            service,
            tasks,
            services,
            hooks,
            context,
            _lease: lease,
        }
    }
    /// Members that belong to no thread: the claim is the only cleanup there is.
    fn new(claimed: bool) -> (tempfile::TempDir, Self) {
        Self::build(claimed, false)
    }
    /// Members on threads whose owner and scratch this leaf can neither see nor clean.
    fn with_threads(claimed: bool) -> (tempfile::TempDir, Self) {
        Self::build(claimed, true)
    }
    fn build(claimed: bool, threads: bool) -> (tempfile::TempDir, Self) {
        let dir = tempfile::tempdir().unwrap();
        let config = StorageConfig {
            db_path: dir.path().join("q1.db"),
            ..Default::default()
        };
        {
            let storage = Storage::open(config.clone()).unwrap();
            seed(&storage, &config, claimed, threads);
        }
        (dir, Self::open(config))
    }
    /// Joins every task and worker, then closes the writer: a process stop.
    async fn close(mut self) -> StorageConfig {
        self.tasks.request_stop();
        self.tasks.shutdown().await.unwrap();
        self.services.shutdown().await.unwrap();
        drop(self.service);
        let config = self.config;
        drop(self.storage);
        config
    }
    async fn query(&self, query: QuarantineQuery) -> Result<QuarantineReply, AdminFailure> {
        self.query_as(self.context.clone(), query).await
    }
    async fn query_as(
        &self,
        context: IngressContext,
        query: QuarantineQuery,
    ) -> Result<QuarantineReply, AdminFailure> {
        let authorized = self
            .service
            .authorize_query(context, LifecycleQuery::Quarantine(query))?;
        match self.service.query(authorized)?.await.unwrap()? {
            LifecycleReply::Quarantine(reply) => Ok(reply),
            other => panic!("unexpected reply {other:?}"),
        }
    }
    async fn item(&self) -> QuarantineItem {
        let QuarantineReply::List { mut items, .. } =
            self.query(QuarantineQuery::List).await.unwrap()
        else {
            panic!("list");
        };
        assert_eq!(items.len(), 1);
        items.remove(0)
    }
    async fn detail(&self, item: &QuarantineItem) -> QuarantineDetail {
        match self
            .query(QuarantineQuery::Detail {
                graph_id: item.graph_id.clone(),
            })
            .await
            .unwrap()
        {
            QuarantineReply::Detail(d) => d,
            other => panic!("unexpected {other:?}"),
        }
    }
    fn binding(item: &QuarantineItem, operation: &str) -> GraphCas {
        GraphCas {
            graph_id: item.graph_id.clone(),
            restart_tx_id: item.restart_tx_id.clone(),
            capture_id: item.capture_id.clone(),
            expected_revision: item.revision,
            expected_core_epoch: EPOCH,
            owner_identity: None,
            owner_generation: None,
            operation_id: id(operation),
        }
    }
    fn discard_intent(binding: GraphCas) -> AdminIntent {
        LifecycleCommand::Quarantine(QuarantineCommand::Discard(ConfirmedGraph {
            binding,
            confirmed: true,
        }))
    }
    /// The admission reply, then every admitted continuation joined.
    async fn discard(&self, binding: GraphCas) -> Result<AdminAccepted, AdminFailure> {
        let intent = Self::discard_intent(binding);
        let request = self
            .service
            .authorize_request(self.context.clone(), intent)?;
        let reply = self.service.submit(request)?.await.unwrap();
        self.service.wait_for_request_slots().await;
        reply
    }
    fn audit_outcome(&self, operation: &str) -> Option<AuditOutcome> {
        Database::read_admin_audit_record(&self.config, operation)
            .unwrap()
            .and_then(|r| r.outcome)
    }
    fn rows(&self, table: &str) -> i64 {
        rusqlite::Connection::open(&self.config.db_path)
            .unwrap()
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
            .unwrap()
    }
    fn sql(&self, statement: &str) {
        rusqlite::Connection::open(&self.config.db_path)
            .unwrap()
            .execute(statement, [])
            .unwrap();
    }
}

#[tokio::test]
async fn list_and_detail_are_separate_bounded_views_of_the_same_cold_graph() {
    let (_dir, h) = Harness::new(false);
    let item = h.item().await;
    assert!(item.cold);
    assert_eq!(item.reason, QuarantineReason::ColdBoot);
    assert_eq!(item.eligibility, QuarantineEligibility::Open);
    assert_eq!(item.completion, QuarantineCompletion::NotStarted);
    assert_eq!(item.members, 2);
    let detail = h.detail(&item).await;
    assert_eq!(detail.item, item);
    assert_eq!(detail.core_epoch, EPOCH);
    // Nothing about the raw identities leaks into the opaque references.
    for reference in [&item.graph_id, &item.restart_tx_id, &item.capture_id] {
        assert!(!reference.as_str().contains(MANIFEST) && !reference.as_str().contains(TX));
    }
    let missing = h
        .query(QuarantineQuery::Detail {
            graph_id: id("graph.unknown"),
        })
        .await
        .unwrap_err();
    assert_eq!(missing.code, ErrorCode::NotFound);
    assert_eq!(h.rows("recovery_graph_control"), 0);
    h.close().await;
}

#[tokio::test]
async fn an_unverified_or_foreign_caller_reaches_neither_a_view_nor_the_store() {
    let (_dir, h) = Harness::new(false);
    let item = h.item().await;
    let calls = h.hooks.snapshots.load(Ordering::SeqCst);
    let forged = IngressContext::unverified(Provenance::human(
        Surface::Web,
        ClientId::new("claimed-human").unwrap(),
    ));
    for query in [
        QuarantineQuery::List,
        QuarantineQuery::Detail {
            graph_id: item.graph_id.clone(),
        },
    ] {
        let error = h.query_as(forged.clone(), query).await.unwrap_err();
        assert_eq!(error.code, ErrorCode::PermissionDenied);
    }
    let intent = Harness::discard_intent(Harness::binding(&item, "op-forged"));
    let error = h.service.authorize_request(forged, intent).err().unwrap();
    assert_eq!(error.code, ErrorCode::PermissionDenied);
    assert_eq!(h.hooks.snapshots.load(Ordering::SeqCst), calls);
    assert_eq!(h.hooks.discards.load(Ordering::SeqCst), 0);
    assert_eq!(h.rows("recovery_graph_control"), 0);
    h.close().await;
}

#[tokio::test]
async fn a_confirmed_discard_succeeds_and_the_revocation_survives_a_reload() {
    let (_dir, h) = Harness::new(true);
    let item = h.item().await;
    assert_eq!(item.claim, QuarantineClaim::Prepared);
    let accepted = h.discard(Harness::binding(&item, "op-1")).await.unwrap();
    assert_eq!(accepted.receipt().operation_id, id("op-1"));
    assert_eq!(h.audit_outcome("op-1"), Some(AuditOutcome::Succeeded));
    let after = h.item().await;
    assert!(after.discard_complete());
    assert_eq!(after.claim, QuarantineClaim::Released);
    assert!(after.revision > item.revision);

    // A fresh process over the same database: nothing is held in memory.
    let config = h.close().await;
    let h = Harness::open(config);
    let reloaded = h.item().await;
    assert_eq!(reloaded, after);
    assert_eq!(h.detail(&reloaded).await.item, after);
    h.close().await;
}

#[tokio::test]
async fn a_stale_revision_is_refused_before_any_effect() {
    let (_dir, h) = Harness::new(false);
    let item = h.item().await;
    let mut stale = Harness::binding(&item, "op-stale");
    stale.expected_revision = Revision(item.revision.get() - 1);
    assert_eq!(
        h.discard(stale).await.unwrap_err().code,
        ErrorCode::Conflict
    );
    assert_eq!(h.hooks.discards.load(Ordering::SeqCst), 0);
    assert_eq!(
        (
            h.rows("recovery_graph_control"),
            h.rows("recovery_graph_effects")
        ),
        (0, 0)
    );
    assert!(!h.item().await.discard_complete());
    h.close().await;
}

#[tokio::test]
async fn a_change_between_inspection_and_the_write_ends_as_a_conflict_without_a_revocation() {
    let (_dir, h) = Harness::new(false);
    let item = h.item().await;
    let config = h.config.clone();
    *h.hooks.before_discard.lock().unwrap() = Some(Box::new(move || {
        rusqlite::Connection::open(&config.db_path)
            .unwrap()
            .execute("UPDATE restart_components SET revision=revision+1", [])
            .unwrap();
    }));
    h.discard(Harness::binding(&item, "op-race")).await.unwrap();
    assert_eq!(h.audit_outcome("op-race"), Some(AuditOutcome::Conflict));
    assert_eq!(h.rows("recovery_graph_control"), 0);
    assert!(!h.item().await.discard_complete());
    h.close().await;
}

#[tokio::test]
async fn repeating_the_same_operation_does_not_apply_it_again() {
    let (_dir, h) = Harness::new(true);
    let item = h.item().await;
    let binding = Harness::binding(&item, "op-1");
    h.discard(binding.clone()).await.unwrap();
    let settled = (
        h.rows("recovery_graph_control"),
        h.rows("recovery_graph_effects"),
        h.item().await,
    );
    let again = h.discard(binding).await.unwrap();
    assert_eq!(again.receipt().operation_id, id("op-1"));
    assert_eq!(h.hooks.discards.load(Ordering::SeqCst), 1);
    assert_eq!(
        settled,
        (
            h.rows("recovery_graph_control"),
            h.rows("recovery_graph_effects"),
            h.item().await
        )
    );
    // The same ID for a different request is a conflict, not a second discard.
    let mut other = Harness::binding(&item, "op-1");
    other.expected_revision = Revision(item.revision.get() + 1);
    assert!(h.discard(other).await.is_err());
    assert_eq!(h.hooks.discards.load(Ordering::SeqCst), 1);
    h.close().await;
}

#[tokio::test]
async fn a_revoked_graph_with_cleanup_left_is_never_reported_as_success() {
    let (_dir, h) = Harness::new(true);
    let item = h.item().await;
    // The claim cannot be released while a member intent has moved on.
    h.sql("UPDATE resume_intents SET revision=revision+1 WHERE run_id='run-a'");
    let item = QuarantineItem {
        revision: h.item().await.revision,
        ..item
    };
    h.discard(Harness::binding(&item, "op-1")).await.unwrap();
    assert_ne!(h.audit_outcome("op-1"), Some(AuditOutcome::Succeeded));
    assert_eq!(h.audit_outcome("op-1"), Some(AuditOutcome::Failed));
    let partial = h.item().await;
    assert_eq!(partial.eligibility, QuarantineEligibility::Revoked);
    assert_eq!(partial.completion, QuarantineCompletion::Partial);
    assert_eq!(partial.claim, QuarantineClaim::Prepared);
    assert!(!partial.discard_complete());

    // Finishing the cleanup is a new operation; it ends in success only then.
    h.sql("UPDATE resume_intents SET revision=revision-1 WHERE run_id='run-a'");
    h.discard(Harness::binding(&partial, "op-2")).await.unwrap();
    assert_eq!(h.audit_outcome("op-2"), Some(AuditOutcome::Succeeded));
    assert!(h.item().await.discard_complete());
    h.close().await;
}

#[tokio::test]
async fn a_lost_acknowledgement_is_settled_from_storage_without_a_second_write() {
    let (_dir, h) = Harness::new(false);
    let item = h.item().await;
    *h.hooks.lose_ack_after_commit.lock().unwrap() = true;
    h.discard(Harness::binding(&item, "op-1")).await.unwrap();
    assert_eq!(h.audit_outcome("op-1"), Some(AuditOutcome::Succeeded));
    assert_eq!(h.hooks.discards.load(Ordering::SeqCst), 1);
    assert_eq!(h.rows("recovery_graph_effects"), 1);

    let (_dir, h) = Harness::new(false);
    let item = h.item().await;
    *h.hooks.fail_before_commit.lock().unwrap() = true;
    h.discard(Harness::binding(&item, "op-2")).await.unwrap();
    assert_eq!(h.audit_outcome("op-2"), Some(AuditOutcome::Failed));
    assert_eq!(h.rows("recovery_graph_control"), 0);
    h.close().await;
}

#[tokio::test]
async fn an_owner_expectation_or_an_adopted_graph_is_not_this_leafs_to_discard() {
    let (_dir, h) = Harness::new(false);
    let item = h.item().await;
    let mut owned = Harness::binding(&item, "op-owner");
    owned.owner_identity = Some(id("owner"));
    owned.owner_generation = Some(Revision(1));
    assert_eq!(
        h.discard(owned).await.unwrap_err().code,
        ErrorCode::Conflict
    );

    h.sql("UPDATE restart_transactions SET adopted_epoch=2");
    let adopted = h.item().await;
    assert!(!adopted.cold);
    assert_eq!(
        h.discard(Harness::binding(&adopted, "op-adopted"))
            .await
            .unwrap_err()
            .code,
        ErrorCode::Conflict
    );
    assert_eq!(h.hooks.discards.load(Ordering::SeqCst), 0);
    assert_eq!(h.rows("recovery_graph_control"), 0);
    h.close().await;
}

#[tokio::test]
async fn a_discard_needs_explicit_confirmation_and_the_current_epoch() {
    let (_dir, h) = Harness::new(false);
    let item = h.item().await;
    let unconfirmed = LifecycleCommand::Quarantine(QuarantineCommand::Discard(ConfirmedGraph {
        binding: Harness::binding(&item, "op-1"),
        confirmed: false,
    }));
    assert!(
        h.service
            .authorize_request(h.context.clone(), unconfirmed)
            .is_err()
    );
    let mut old_epoch = Harness::binding(&item, "op-2");
    old_epoch.expected_core_epoch = InstanceEpoch(6);
    assert!(
        h.service
            .authorize_request(h.context.clone(), Harness::discard_intent(old_epoch))
            .is_err()
    );
    assert_eq!(h.hooks.discards.load(Ordering::SeqCst), 0);
    h.close().await;
}

#[tokio::test]
async fn threads_with_no_disposal_record_are_never_reported_as_a_successful_discard() {
    use storage::recovery::graph_progress::{
        GraphDisposalProgressKey, GraphDisposalStep, GraphThreadProgressWrite,
    };
    let (_dir, h) = Harness::with_threads(false);
    let item = h.item().await;
    h.discard(Harness::binding(&item, "op-1")).await.unwrap();
    // The eligibility is revoked, yet an owner or scratch may remain and nothing
    // recorded that they were handled: that is not success.
    assert_ne!(h.audit_outcome("op-1"), Some(AuditOutcome::Succeeded));
    assert_eq!(h.audit_outcome("op-1"), Some(AuditOutcome::Failed));
    let revoked = h.item().await;
    assert_eq!(revoked.eligibility, QuarantineEligibility::Revoked);
    assert_eq!(revoked.completion, QuarantineCompletion::Partial);
    assert!(!revoked.discard_complete());

    // Only the production progress write for every thread lets it finish.
    let control = Database::open(&h.config)
        .unwrap()
        .recovery_graph_control(MANIFEST)
        .unwrap()
        .unwrap();
    for thread in ["run-a", "run-b"] {
        h.storage
            .handle()
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
                owners: GraphDisposalStep::NotApplicable,
                scratch: GraphDisposalStep::Applied,
            })
            .unwrap();
    }
    let recorded = h.item().await;
    assert!(recorded.discard_complete());
    h.discard(Harness::binding(&recorded, "op-2"))
        .await
        .unwrap();
    assert_eq!(h.audit_outcome("op-2"), Some(AuditOutcome::Succeeded));
    h.close().await;
}
