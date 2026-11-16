//! Internal durable handoff API. No wire request or production constructor.
use super::*;
use crate::quiesce::*;
use std::collections::BTreeMap;
use storage::recovery::restart::*;

/// An irreversible, process-local capture boundary. Not serializable and not
/// constructible from storage. The host starts it only from SealedRestart while
/// holding its coordinator mutex, then publishes Capturing before releasing it.
pub struct RestartCaptureSession {
    pub(super) runtime: Weak<Shared>,
    pub(super) fence: DispatchDrainFence,
    pub(super) intents: BTreeMap<RunId, ResumeIntent>,
    pub(super) recipes: BTreeMap<RunId, RecoveryRecipe>,
    pub(super) restart_id: String,
    pub(super) capture_id: String,
    pub(super) revision: u64,
    expected_persisted: bool,
}
/// Failed adoption retains the physical proof, including on readback failure.
#[derive(Debug)]
pub struct RestartAdoptionError {
    pub error: RuntimeError,
    pub proof: core_instance::InheritedHandoffProof,
}
impl std::fmt::Display for RestartAdoptionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(f)
    }
}
impl std::error::Error for RestartAdoptionError {}
impl AgentRuntime {
    pub(crate) fn external_admission_guard(
        &self,
    ) -> Result<std::sync::MutexGuard<'_, ()>, RuntimeError> {
        let g = self
            .shared
            .quiesce
            .manual_launch_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.shared.quiesce.planned_admission_closed() {
            return Err(RuntimeError::RestartBusy);
        }
        Ok(g)
    }
    pub(crate) fn invalidate_active_restart_capture(&self, reason: &str) {
        let Some(id) = self.shared.active_restart_capture.get() else {
            return;
        };
        self.shared.dispatch_tails.invalidate_capture();
        self.shared.quiesce.fail(reason);
        let result = (|| {
            let store = self
                .shared
                .run_store
                .get()
                .ok_or_else(|| error("no run store"))?;
            let (revision, phase) = store
                .with_database(|db| db.restart_revision_phase(id))
                .map_err(error)?
                .ok_or_else(|| error("missing restart invalidation target"))?;
            if !phase.terminal() {
                store
                    .handle
                    .invalidate_restart(id.clone(), revision, reason.into())
                    .map_err(error)?;
            }
            Ok::<_, RuntimeError>(())
        })();
        if let Err(error) = result {
            tracing::error!(restart_id=%id,%error,"durable restart invalidation failed; exec remains forbidden");
        }
    }
    pub fn has_planned_restart_handoff(&self) -> bool {
        self.shared.planned_adoption.get().is_some()
    }

    /// Request-time and capture-time preflight share this inspection. Read-only:
    /// Unsupported must be reported before parking, never silently dropped.
    pub fn preflight_restart_capture(
        &self,
        recipes: &BTreeMap<RunId, RecoveryRecipe>,
    ) -> Result<(), RuntimeError> {
        let store = self
            .shared
            .run_store
            .get()
            .ok_or_else(|| error("restart capture requires storage"))?;
        if self
            .shared
            .admissions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .any(|a| a.queued.is_some())
        {
            return Err(error("Unsupported: RAM-only queued launch"));
        }
        if !self.accepted_chat_resumes().is_empty() {
            return Err(error(
                "Unsupported: accepted resume authority is a seal blocker",
            ));
        }
        let intents = self.shared.quiesce.live_capture_intents();
        for intent in intents.values().filter(|i| i.active) {
            let root = super::recovery_graph::root_of(intent.run_id, &intents)?;
            let recipe = recipes
                .get(&root)
                .ok_or_else(|| error("Unsupported: missing root recipe"))?;
            if recipe.policy != ResumePolicy::CoreOwned {
                return Err(error("Unsupported: recovery policy"));
            }
            let snapshot = store
                .with_database(|db| db.run_context(&intent.run_id.to_string()))
                .map_err(error)?;
            let descriptor = snapshot
                .as_ref()
                .map(|s| {
                    serde_json::from_str::<crate::restore::RunRestoreDescriptor>(&s.config_json)
                })
                .transpose()
                .map_err(error)?;
            let mut has_receipt = false;
            for reference in intent
                .checkpoint
                .as_ref()
                .map(|c| &c.snapshot)
                .into_iter()
                .chain(intent.snapshot.as_ref())
            {
                if matches!(
                    reference,
                    crate::checkpoint::SnapshotReference::RunContext { .. }
                ) {
                    has_receipt = true;
                    if !reference.available(snapshot.as_ref()) {
                        return Err(error("Unsupported: missing or changed referenced snapshot"));
                    }
                }
            }
            if !has_receipt {
                let durable_queued = matches!(intent.wait, ResumeWait::AdmissionQueued)
                    && intent.initial_input.is_some()
                    && store
                        .with_database(|db| db.resume_intent(&intent.run_id.to_string()))
                        .map_err(error)?
                        .and_then(|row| ResumeIntent::decode(row).ok())
                        .is_some_and(|saved| {
                            saved.incarnation == intent.incarnation
                                && saved.active
                                && saved.stop_reason.is_none()
                                && matches!(saved.wait, ResumeWait::AdmissionQueued)
                                && saved.initial_input == intent.initial_input
                        });
                if !durable_queued {
                    return Err(error("Unsupported: missing safe snapshot"));
                }
            }
            if intent.run_id == root
                && descriptor
                    .as_ref()
                    .and_then(|d| d.thread_id.as_ref())
                    .or(recipe
                        .descriptor
                        .as_ref()
                        .and_then(|d| d.thread_id.as_ref()))
                    .is_none()
                && self.goal_thread(root).is_none()
            {
                return Err(error("Unsupported: root is not bound to a thread"));
            }
        }
        Ok(())
    }
    /// Planned mode rejects only fallible external admission; automatic internal
    /// launches retain 07a's queue and force a reversible Unsupported cancellation.
    pub fn begin_planned_restart_quiesce(
        &self,
        recipes: &BTreeMap<RunId, RecoveryRecipe>,
    ) -> Result<RestartAttempt, RuntimeError> {
        self.preflight_restart_capture(recipes)?;
        if let Some(store) = self.shared.run_store.get()
            && store
                .with_database(|db| db.has_pending_adopted_restart())
                .map_err(error)?
        {
            return Err(error(
                "previous adopted restart has RetryPending components",
            ));
        }
        self.begin_restart_quiesce_mode(true)
    }
    /// Production capture consumes the capability minted by the host drain gate.
    /// A raw dispatch fence cannot manufacture host admission evidence:
    /// ```compile_fail
    /// fn bypass(runtime: &runtime::AgentRuntime, fence: runtime::quiesce::DispatchDrainFence,
    ///     tx: &storage::recovery::restart::RestartTransaction) {
    ///     runtime.begin_restart_capture_session(fence, tx, Default::default());
    /// }
    /// ```
    pub fn begin_restart_capture_session(
        &self,
        sealed: SealedRestart,
        tx: &RestartTransaction,
        recipes: BTreeMap<RunId, RecoveryRecipe>,
    ) -> Result<RestartCaptureSession, RuntimeError> {
        self.begin_restart_capture_from_fence(sealed.into_fence(self)?, tx, recipes)
    }
    /// Logical fixture only: production callers must consume SealedRestart.
    #[cfg(any(test, feature = "recovery-test-support"))]
    pub fn begin_restart_capture_session_fixture(
        &self,
        fence: DispatchDrainFence,
        tx: &RestartTransaction,
        recipes: BTreeMap<RunId, RecoveryRecipe>,
    ) -> Result<RestartCaptureSession, RuntimeError> {
        self.begin_restart_capture_from_fence(fence, tx, recipes)
    }
    /// Synchronous memory boundary, no database I/O.
    fn begin_restart_capture_from_fence(
        &self,
        fence: DispatchDrainFence,
        tx: &RestartTransaction,
        recipes: BTreeMap<RunId, RecoveryRecipe>,
    ) -> Result<RestartCaptureSession, RuntimeError> {
        if self
            .shared
            .admissions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .any(|a| a.queued.is_some())
        {
            return Err(error("Unsupported: RAM-only queued launch before capture"));
        }
        if !self.shared.quiesce.planned_admission_closed() {
            return Err(error("capture requires a planned attempt"));
        }
        if tx.phase != RestartPhase::Preparing {
            return Err(error("restart is not Preparing"));
        }
        let _checkpoint = self
            .shared
            .quiesce
            .checkpoint_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.begin_recovery_capture(&fence)?;
        self.shared
            .active_restart_capture
            .set(tx.metadata.restart_id.clone())
            .map_err(|_| error("capture session already started"))?;
        Ok(RestartCaptureSession {
            runtime: Arc::downgrade(&self.shared),
            fence,
            intents: self.shared.quiesce.live_capture_intents(),
            recipes,
            restart_id: tx.metadata.restart_id.clone(),
            capture_id: tx.metadata.capture_id.clone(),
            revision: tx.revision,
            expected_persisted: false,
        })
    }
    pub(super) fn validate_capture_session(
        &self,
        s: &RestartCaptureSession,
    ) -> Result<(), RuntimeError> {
        if !Weak::ptr_eq(&s.runtime, &Arc::downgrade(&self.shared))
            || !self.shared.dispatch_tails.capture_valid()
        {
            return Err(error("foreign or invalidated capture session"));
        }
        self.validate_restart_dispatch_fence(&s.fence)
    }
    /// Persist the live expected set BEFORE reading graph payloads. Any subsequent
    /// failure is FailedClosed; even components with no manifest remain excluded.
    pub fn persist_restart_capture_expected(
        &self,
        s: &mut RestartCaptureSession,
    ) -> Result<(), RuntimeError> {
        let result = (|| {
            self.validate_capture_session(s)?;
            let store = self
                .shared
                .run_store
                .get()
                .ok_or_else(|| error("no run store"))?;
            let mut members = Vec::new();
            for intent in s.intents.values() {
                let root = super::recovery_graph::root_of(intent.run_id, &s.intents)?;
                let recipe = s
                    .recipes
                    .get(&intent.run_id)
                    .or_else(|| s.recipes.get(&root));
                let descriptor = recipe.and_then(|r| r.descriptor.as_ref());
                let (owner_id, owner_generation) = {
                    let runs = lock_runs(&self.shared.runs);
                    let owner = runs
                        .get(&intent.run_id)
                        .and_then(|e| e.config.ownership.as_ref());
                    (
                        owner.map(|p| p.lease.owner_id.clone()),
                        owner.map(|p| p.lease.generation),
                    )
                };
                members.push(RestartExpectedMember {
                    run_id: intent.run_id.to_string(),
                    incarnation: intent.incarnation.clone(),
                    root_run_id: root.to_string(),
                    thread_id: descriptor
                        .and_then(|d| d.thread_id.clone())
                        .or_else(|| self.goal_thread(root)),
                    intent_revision: store
                        .with_database(|db| db.resume_intent_revision(&intent.run_id.to_string()))
                        .map_err(error)?
                        .unwrap_or(0),
                    snapshot: super::recovery_graph::snapshot_receipt(intent),
                    owner_id,
                    owner_generation,
                    lane_id: recipe
                        .and_then(|r| r.lane.as_ref())
                        .map(|_| root.to_string()),
                });
            }
            let tx = store
                .handle
                .mutate_restart(
                    s.restart_id.clone(),
                    Some(s.revision),
                    RestartMutation::BeginCapture(members),
                )
                .map_err(error)?;
            s.revision = tx.revision;
            s.expected_persisted = true;
            Ok(())
        })();
        if result.is_err()
            && let Err(error) =
                self.invalidate_restart_capture(s, "capture persistence or readback failed")
        {
            tracing::error!(%error,"capture invalidation save failed; execution forbidden");
        }
        result
    }
    pub fn read_restart_capture_records(
        &self,
        s: &RestartCaptureSession,
    ) -> Result<Vec<RunRecoveryRecord>, RuntimeError> {
        let result = (|| {
            self.validate_capture_session(s)?;
            if !s.expected_persisted {
                return Err(error("expected set must be durable before graph read"));
            }
            let records = self.capture_records_impl(&s.fence, &s.recipes, Some(&s.intents))?;
            validate_complete_graph(&records)?;
            Ok(records)
        })();
        if result.is_err()
            && let Err(error) =
                self.invalidate_restart_capture(s, "capture persistence or readback failed")
        {
            tracing::error!(%error,"capture invalidation save failed; execution forbidden");
        }
        result
    }
    pub fn commit_restart_capture(
        &self,
        s: &mut RestartCaptureSession,
    ) -> Result<RestartTransaction, RuntimeError> {
        let result = (|| {
            self.validate_capture_session(s)?;
            if !s.expected_persisted {
                return Err(error("expected set must be durable before capture commit"));
            }
            let store = self
                .shared
                .run_store
                .get()
                .ok_or_else(|| error("no run store"))?;
            // Re-read the frozen graph; arbitrary caller-authored records cannot
            // omit a continuation/delivery while retaining the expected IDs.
            let records = self.read_restart_capture_records(s)?;
            self.save_recovery_graph_inner(
                &s.capture_id,
                &records,
                Some((&s.restart_id, &mut s.revision)),
            )?;
            let tx = store
                .handle
                .mutate_restart(
                    s.restart_id.clone(),
                    Some(s.revision),
                    RestartMutation::Commit,
                )
                .map_err(error)?;
            s.revision = tx.revision;
            let readback = store
                .with_database(|db| db.restart_transaction(&s.restart_id))
                .map_err(error)?
                .ok_or_else(|| error("committed restart readback missing"))?;
            if readback != tx {
                return Err(error("committed restart readback changed"));
            }
            Ok(readback)
        })();
        if result.is_err()
            && let Err(error) =
                self.invalidate_restart_capture(s, "capture persistence or readback failed")
        {
            tracing::error!(%error,"capture invalidation save failed; execution forbidden");
        }
        result
    }
    /// The memory fence wins even if invalidation storage fails. Its result is
    /// independent of terminal intent persistence (no owner-guard bypass journal).
    pub fn invalidate_restart_capture(
        &self,
        s: &RestartCaptureSession,
        reason: &str,
    ) -> Result<(), RuntimeError> {
        if !Weak::ptr_eq(&s.runtime, &Arc::downgrade(&self.shared)) {
            return Err(error("foreign capture session"));
        }
        self.shared.dispatch_tails.invalidate_capture();
        self.shared.quiesce.fail(reason);
        let store = self
            .shared
            .run_store
            .get()
            .ok_or_else(|| error("no run store"))?;
        let (revision, phase) = store
            .with_database(|db| db.restart_revision_phase(&s.restart_id))
            .map_err(error)?
            .ok_or_else(|| error("missing restart invalidation target"))?;
        if !phase.terminal() {
            store
                .handle
                .invalidate_restart(s.restart_id.clone(), revision, reason.into())
                .map_err(error)?;
        }
        Ok(())
    }
    /// The only recovery eligibility installation takes opaque physical evidence.
    /// An uncertain write is resolved by same-ID readback; any unresolved error
    /// returns ownership of the proof so the caller can retry without minting one.
    pub fn adopt_restart_handoff(
        &self,
        proof: core_instance::InheritedHandoffProof,
    ) -> Result<(), Box<RestartAdoptionError>> {
        if let Err(error) = self.validate_restart_adoption(&proof) {
            return Err(Box::new(RestartAdoptionError { error, proof }));
        }
        self.shared.planned_adoption.set(proof).map_err(|proof| {
            Box::new(RestartAdoptionError {
                error: error("handoff proof already installed"),
                proof,
            })
        })
    }
    fn validate_restart_adoption(
        &self,
        proof: &core_instance::InheritedHandoffProof,
    ) -> Result<(), RuntimeError> {
        let store = self
            .shared
            .run_store
            .get()
            .ok_or_else(|| error("no run store"))?;
        let read = || {
            store
                .with_database(|db| db.restart_transaction(proof.restart_id()))
                .map_err(error)?
                .ok_or_else(|| error("missing handoff transaction"))
        };
        let matches_proof = |tx: &RestartTransaction| {
            tx.metadata.restart_id == proof.restart_id()
                && tx.metadata.config_identity == proof.config_identity()
                && tx.metadata.successor_epoch == proof.successor_epoch()
                && tx.metadata.successor_build == proof.successor_build()
                && tx.ticket_nonce_hash.as_deref() == Some(proof.nonce_hash())
        };
        let before = read()?;
        if !matches_proof(&before) {
            return Err(error("handoff proof mismatch"));
        }
        let tx = if before.phase == RestartPhase::Armed {
            let write = store.handle.mutate_restart(
                proof.restart_id().into(),
                Some(before.revision),
                RestartMutation::Adopt {
                    nonce_hash: proof.nonce_hash().into(),
                    successor_epoch: proof.successor_epoch(),
                    config_identity: proof.config_identity().into(),
                    successor_build: proof.successor_build().into(),
                },
            );
            // Never infer failure or success from an ambiguous ACK. The opaque
            // proof remains owned by this call while canonical state is read.
            let after = read()?;
            if after.revision <= before.revision
                || after.metadata != before.metadata
                || after.expected_digest != before.expected_digest
                || after.manifest_digest != before.manifest_digest
            {
                return Err(write
                    .err()
                    .map(error)
                    .unwrap_or_else(|| error("adoption readback mismatch")));
            }
            after
        } else {
            before
        };
        if !matches_proof(&tx)
            || tx.phase != RestartPhase::Adopted
            || tx.adopted_epoch != Some(proof.successor_epoch())
        {
            return Err(error("handoff is not adoptable"));
        }
        // Adopt validates the capture atomically in storage. An already-adopted
        // successor installs only physical eligibility here: per-component graph
        // validation still belongs to require_planned_manifest/activation, so one
        // damaged component cannot prevent component-level diagnosis.
        Ok(())
    }
    pub fn require_planned_manifest(&self, manifest: &str) -> Result<(), RuntimeError> {
        let proof = self
            .shared
            .planned_adoption
            .get()
            .ok_or_else(|| error("cold recovery requires inherited handoff proof"))?;
        let store = self
            .shared
            .run_store
            .get()
            .ok_or_else(|| error("no run store"))?;
        let tx = store
            .with_database(|db| db.restart_for_manifest(manifest))
            .map_err(error)?
            .ok_or_else(|| error("manifest is not in adopted restart"))?;
        if tx.metadata.restart_id != proof.restart_id()
            || tx.adopted_epoch != Some(proof.successor_epoch())
            || tx.phase != RestartPhase::Adopted
            || tx
                .components
                .iter()
                .find(|c| c.manifest_id == manifest)
                .is_none_or(|c| c.disposition != RestartDisposition::RetryPending)
        {
            return Err(error("restart manifest is not eligible"));
        }
        store
            .with_database(|db| db.validate_restart_capture_digest(&tx.metadata.restart_id))
            .map_err(error)?;
        Ok(())
    }
    /// Called before ordinary startup scans. Completed handoffs are left alone;
    /// pending components are quarantined, never granted a replacement authority.
    pub fn quarantine_cold_restarts(&self) -> Result<(), RuntimeError> {
        if self.shared.planned_adoption.get().is_some() {
            return Err(error("planned successor is not a cold boot"));
        }
        let store = self
            .shared
            .run_store
            .get()
            .ok_or_else(|| error("no run store"))?;
        for (id, revision) in store
            .with_database(|db| db.cold_restart_candidates())
            .map_err(error)?
        {
            store
                .handle
                .quarantine_cold_restart(id, revision)
                .map_err(error)?;
        }
        Ok(())
    }
}
fn error(reason: impl ToString) -> RuntimeError {
    crate::quiesce::runtime::error(reason)
}

fn validate_complete_graph(records: &[RunRecoveryRecord]) -> Result<(), RuntimeError> {
    let ids: std::collections::BTreeSet<_> = records.iter().map(|r| r.run_id).collect();
    if ids.len() != records.len() {
        return Err(error("duplicate capture member"));
    }
    for record in records {
        if record.parent.is_some_and(|parent| !ids.contains(&parent))
            || record.waits.iter().any(|wait| {
                !ids.contains(&wait.requester)
                    || wait.targets.iter().any(|target| !ids.contains(target))
            })
            || record.deliveries.iter().any(|d| {
                d.state != DurableDeliveryState::Consumed
                    && (!ids.contains(&d.sender) || !ids.contains(&d.recipient))
            })
        {
            return Err(error(
                "incomplete capture continuation or delivery endpoint",
            ));
        }
    }
    Ok(())
}
