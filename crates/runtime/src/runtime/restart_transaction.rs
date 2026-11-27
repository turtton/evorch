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
/// Copied classification of one member of the current capture inventory.
/// Contains no execution settings, owner capability, provider input or live handle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RestartRecipeMember {
    pub run: RunId,
    pub root: RunId,
    pub parent: Option<RunId>,
    pub incarnation: String,
    pub thread: Option<String>,
    pub role: String,
    pub conversation: bool,
    pub permit_matches: bool,
    pub lane: bool,
    pub team: bool,
    pub goal: bool,
    pub internal: bool,
    /// Project/model selector and interaction flags, without exposing settings.
    pub context_fingerprint: String,
    pub snapshots: Vec<crate::checkpoint::SnapshotReference>,
}

/// Memory-only recipe-collection observation; never admission or capture authority.
pub struct RestartRecipeObservation {
    runtime: Weak<Shared>,
    commits: SnapshotCommits,
    intents: BTreeMap<RunId, ResumeIntent>,
    metadata: BTreeMap<RunId, String>,
}
/// One-use read-only DB preflight, bound to this runtime and the inspected live set.
/// Consuming it performs memory checks only; it is not capture/exec authority.
pub struct RestartPreflight {
    runtime: Weak<Shared>,
    intents: BTreeMap<RunId, ResumeIntent>,
    metadata: BTreeMap<RunId, String>,
    recipes: String,
}
/// Failed adoption retains the physical proof, including on readback failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartAdoptionFailure {
    Rejected,
    Unconfirmed,
}
#[derive(Debug)]
pub struct RestartAdoptionError {
    pub failure: RestartAdoptionFailure,
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
        if let Some(run_id) = self
            .shared
            .admissions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .find_map(|(run, a)| a.queued.is_some().then_some(*run))
        {
            return Err(RuntimeError::RestartRunUnavailable {
                run_id,
                reason: "RAM-only queued launch".into(),
            });
        }
        if let Some(run_id) = self.accepted_chat_resumes().keys().next().copied() {
            return Err(RuntimeError::RestartRunUnavailable {
                run_id,
                reason: "accepted resume authority is a seal blocker".into(),
            });
        }
        let intents = self.shared.quiesce.live_capture_intents();
        for intent in intents.values().filter(|i| i.active) {
            let refused = |reason: &str| RuntimeError::RestartRunUnavailable {
                run_id: intent.run_id,
                reason: reason.into(),
            };
            let root = super::recovery_graph::root_of(intent.run_id, &intents)
                .map_err(|_| refused("invalid recovery parent graph"))?;
            let recipe = recipes
                .get(&root)
                .ok_or_else(|| refused("missing root recipe"))?;
            if recipe.policy != ResumePolicy::CoreOwned {
                return Err(refused("recovery policy"));
            }
            let snapshot = store
                .with_database(|db| db.run_context(&intent.run_id.to_string()))
                .map_err(|_| refused("snapshot unavailable"))?;
            let descriptor = snapshot
                .as_ref()
                .map(|s| {
                    serde_json::from_str::<crate::restore::RunRestoreDescriptor>(&s.config_json)
                })
                .transpose()
                .map_err(|_| refused("corrupt descriptor"))?;
            let mut has_receipt = false;
            // An adopted checkpoint describes the previous capture boundary.
            // Ordinary turns may since have committed a newer snapshot. The
            // latest receipt is preflight evidence; capture still parks and
            // commits its own safe-point checkpoint before graph serialization.
            if let Some(reference) = intent
                .snapshot
                .as_ref()
                .or_else(|| intent.checkpoint.as_ref().map(|c| &c.snapshot))
                && matches!(
                    reference,
                    crate::checkpoint::SnapshotReference::RunContext { .. }
                )
            {
                has_receipt = true;
                if !reference.available(snapshot.as_ref()) {
                    return Err(refused("missing or changed referenced snapshot"));
                }
            }
            if !has_receipt {
                let durable_queued = matches!(intent.wait, ResumeWait::AdmissionQueued)
                    && intent.initial_input.is_some()
                    && store
                        .with_database(|db| db.resume_intent(&intent.run_id.to_string()))
                        .map_err(|_| refused("snapshot unavailable"))?
                        .and_then(|row| ResumeIntent::decode(row).ok())
                        .is_some_and(|saved| {
                            saved.incarnation == intent.incarnation
                                && saved.active
                                && saved.stop_reason.is_none()
                                && matches!(saved.wait, ResumeWait::AdmissionQueued)
                                && saved.initial_input == intent.initial_input
                        });
                if !durable_queued {
                    return Err(refused("missing safe snapshot"));
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
                return Err(refused("root is not bound to a thread"));
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
        let preflight = self.prepare_restart_quiesce(recipes)?;
        self.begin_preflighted_restart_quiesce(preflight)
    }
    /// Blocking DB work. Call outside the lifecycle mutex, before the final
    /// generation/cancellation/deadline check.
    pub fn prepare_restart_quiesce(
        &self,
        recipes: &BTreeMap<RunId, RecoveryRecipe>,
    ) -> Result<RestartPreflight, RuntimeError> {
        let commits = self.shared.quiesce.snapshot_commits();
        let intents = self.shared.quiesce.live_capture_intents();
        let metadata = self.restart_recipe_metadata();
        let checked = self.preflight_restart_capture(recipes).and_then(|()| {
            let pending = match self.shared.run_store.get() {
                Some(store) => store
                    .with_database(|db| db.has_pending_adopted_restart())
                    .map_err(error)?,
                None => false,
            };
            if pending {
                return Err(error(
                    "previous adopted restart has RetryPending components",
                ));
            }
            Ok(())
        });
        // A run that changed while it was inspected (a snapshot committed but
        // not yet in its intent, for one) is observed again, whatever the
        // inspection of the older state concluded.
        if intents != self.shared.quiesce.live_capture_intents()
            || metadata != self.restart_recipe_metadata()
            || !self
                .shared
                .quiesce
                .snapshot_commits()
                .settled_since(commits)
        {
            return Err(RuntimeError::RestartObservationChanged);
        }
        checked?;
        Ok(RestartPreflight {
            runtime: Arc::downgrade(&self.shared),
            intents,
            metadata,
            recipes: serde_json::to_string(recipes).map_err(error)?,
        })
    }
    /// Read-only classification against permits already retained by the host.
    /// Matching a supplied permit is evidence only; this method cannot issue one.
    /// Call outside the host mutex. The ordinary preflight still binds and checks
    /// the runtime observation before any planned admission transition.
    pub fn restart_recipe_inventory(
        &self,
        held: &BTreeMap<RunId, crate::ownership::OwnerPermit>,
    ) -> Result<Vec<RestartRecipeMember>, RuntimeError> {
        let observed = self.observe_restart_recipes();
        let mut members = Vec::with_capacity(observed.intents.len());
        {
            let runs = self
                .shared
                .runs
                .lock()
                .map_err(|_| error("recipe inventory unavailable"))?;
            for intent in observed.intents.values() {
                let root = super::recovery_graph::root_of(intent.run_id, &observed.intents)
                    .map_err(|_| RuntimeError::RestartRunUnavailable {
                        run_id: intent.run_id,
                        reason: "invalid recovery parent graph".into(),
                    })?;
                // A missing live member is never silently filtered out. Queued
                // launches without a registered config remain unsupported.
                let entry = runs.get(&intent.run_id).ok_or_else(|| {
                    RuntimeError::RestartRunUnavailable {
                        run_id: intent.run_id,
                        reason: "recipe member unavailable".into(),
                    }
                })?;
                if entry.parent != intent.parent_run_id || entry.role.name() != intent.role {
                    return Err(RuntimeError::RestartRunUnavailable {
                        run_id: intent.run_id,
                        reason: "runtime metadata mismatch".into(),
                    });
                }
                let config = &entry.config;
                let permit_matches =
                    config
                        .ownership
                        .as_ref()
                        .zip(held.get(&root))
                        .is_some_and(|(live, held)| {
                            live.registry_path == held.registry_path
                                && live.thread_id == held.thread_id
                                && live.lease.owner_id == held.lease.owner_id
                                && live.lease.generation == held.lease.generation
                        });
                members.push(RestartRecipeMember {
                    run: intent.run_id,
                    root,
                    parent: entry.parent,
                    incarnation: intent.incarnation.clone(),
                    thread: None,
                    role: intent.role.clone(),
                    conversation: config.conversation,
                    permit_matches,
                    lane: config.lane_scope.is_some(),
                    team: config.team.is_some()
                        || config.team_task.is_some()
                        || config.team_store.is_some()
                        || config.topology.worker_limit().is_some(),
                    goal: false,
                    internal: config.learning_internal
                        || config.purpose != crate::RunPurpose::General,
                    context_fingerprint: restart_digest(&(
                        &config.project_root,
                        &config.model_preference,
                        config.interactive,
                        config.keep_alive,
                    ))
                    .map_err(error)?,
                    snapshots: intent
                        .snapshot
                        .clone()
                        .or_else(|| intent.checkpoint.as_ref().map(|c| c.snapshot.clone()))
                        .into_iter()
                        .collect(),
                });
            }
        }
        for member in &mut members {
            member.thread = self.goal_thread(member.root);
            member.goal = self.goal_for_root(member.root).is_some()
                || self.goal_for_root(member.run).is_some();
        }
        self.validate_restart_recipe_observation(&observed)?;
        Ok(members)
    }

    /// Observe before collecting host recipes. No I/O and no authority is minted.
    pub fn observe_restart_recipes(&self) -> RestartRecipeObservation {
        RestartRecipeObservation {
            runtime: Arc::downgrade(&self.shared),
            commits: self.shared.quiesce.snapshot_commits(),
            intents: self.shared.quiesce.live_capture_intents(),
            metadata: self.restart_recipe_metadata(),
        }
    }
    fn validate_restart_recipe_observation(
        &self,
        observed: &RestartRecipeObservation,
    ) -> Result<(), RuntimeError> {
        if !Weak::ptr_eq(&observed.runtime, &Arc::downgrade(&self.shared))
            || observed.intents != self.shared.quiesce.live_capture_intents()
            || observed.metadata != self.restart_recipe_metadata()
            || !self
                .shared
                .quiesce
                .snapshot_commits()
                .settled_since(observed.commits)
        {
            return Err(RuntimeError::RestartObservationChanged);
        }
        Ok(())
    }
    /// DB preflight is bound to the observation taken before host recipe collection.
    pub fn prepare_restart_quiesce_observed(
        &self,
        recipes: &BTreeMap<RunId, RecoveryRecipe>,
        observed: RestartRecipeObservation,
    ) -> Result<RestartPreflight, RuntimeError> {
        self.validate_restart_recipe_observation(&observed)?;
        let proof = self.prepare_restart_quiesce(recipes)?;
        self.validate_restart_recipe_observation(&observed)?;
        Ok(proof)
    }
    fn restart_recipe_metadata(&self) -> BTreeMap<RunId, String> {
        let runs = self
            .shared
            .runs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut metadata: BTreeMap<_, _> = runs
            .iter()
            .map(|(id, entry)| {
                let c = &entry.config;
                (
                    *id,
                    format!(
                        "{:?}",
                        (
                            &entry.parent,
                            c.conversation,
                            &c.ownership,
                            &c.lane_scope,
                            &c.team,
                            &c.team_task,
                            &c.topology,
                            &c.task_id
                        )
                    ),
                )
            })
            .collect();
        drop(runs);
        for (id, value) in &mut metadata {
            value.push_str(&format!(
                "{:?}",
                (
                    self.goal_thread(*id),
                    self.goal_for_root(*id).map(|goal| goal.goal_id)
                )
            ));
        }
        metadata
    }
    /// All checks here are memory-only. The inspected DB proof is never refreshed
    /// under public/collector/coordinator locks. Capture still revalidates storage.
    pub fn validate_restart_preflight(
        &self,
        proof: &RestartPreflight,
        recipes: &BTreeMap<RunId, RecoveryRecipe>,
    ) -> Result<(), RuntimeError> {
        if !Weak::ptr_eq(&proof.runtime, &Arc::downgrade(&self.shared))
            || proof.intents != self.shared.quiesce.live_capture_intents()
            || proof.metadata != self.restart_recipe_metadata()
            || proof.recipes != serde_json::to_string(recipes).map_err(error)?
            || self
                .shared
                .admissions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .values()
                .any(|a| a.queued.is_some())
            || !self.accepted_chat_resumes().is_empty()
        {
            return Err(error("stale or foreign restart preflight"));
        }
        self.check_restart_preparation()
    }
    /// Synchronous memory-only boundary. DB preflight is never run under the
    /// host coordinator mutex. Capture independently repeats durable validation.
    pub fn begin_preflighted_restart_quiesce(
        &self,
        preflight: RestartPreflight,
    ) -> Result<RestartAttempt, RuntimeError> {
        // Same external registration exclusion as ordinary planned admission.
        let _registration = self.external_admission_guard()?;
        if !Weak::ptr_eq(&preflight.runtime, &Arc::downgrade(&self.shared))
            || preflight.intents != self.shared.quiesce.live_capture_intents()
            || preflight.metadata != self.restart_recipe_metadata()
        {
            return Err(
                if Weak::ptr_eq(&preflight.runtime, &Arc::downgrade(&self.shared)) {
                    RuntimeError::RestartObservationChanged
                } else {
                    error("stale or foreign restart preflight")
                },
            );
        }
        self.check_restart_preparation()?;
        self.begin_restart_quiesce_guarded(true, &_registration)
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
        if let Err((error, failure)) = self.validate_restart_adoption(&proof) {
            return Err(Box::new(RestartAdoptionError {
                error,
                proof,
                failure,
            }));
        }
        self.shared.planned_adoption.set(proof).map_err(|proof| {
            Box::new(RestartAdoptionError {
                failure: RestartAdoptionFailure::Rejected,
                error: error("handoff proof already installed"),
                proof,
            })
        })
    }
    fn validate_restart_adoption(
        &self,
        proof: &core_instance::InheritedHandoffProof,
    ) -> Result<(), (RuntimeError, RestartAdoptionFailure)> {
        use RestartAdoptionFailure::{Rejected, Unconfirmed};
        let store = self
            .shared
            .run_store
            .get()
            .ok_or_else(|| (error("no run store"), Rejected))?;
        let read = || {
            store
                .with_database(|db| db.restart_transaction(proof.restart_id()))
                .map_err(|e| (error(e), Unconfirmed))?
                .ok_or_else(|| (error("missing handoff transaction"), Rejected))
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
            return Err((error("handoff proof mismatch"), Rejected));
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
                return Err((
                    write
                        .err()
                        .map(error)
                        .unwrap_or_else(|| error("adoption readback mismatch")),
                    Unconfirmed,
                ));
            }
            after
        } else {
            before
        };
        if !matches_proof(&tx)
            || tx.phase != RestartPhase::Adopted
            || tx.adopted_epoch != Some(proof.successor_epoch())
        {
            return Err((error("handoff is not adoptable"), Rejected));
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

#[cfg(test)]
mod admission_observation_tests {
    use super::*;
    struct Never;
    #[async_trait::async_trait]
    impl AgentModel for Never {
        fn selected_model(&self, _: Role, _: Option<&str>) -> String {
            "never".into()
        }
        async fn complete(
            &self,
            _: &crate::AgentInvocationContext,
            _: Role,
            _: &[providers::Message],
            _: &[providers::ToolSpec],
        ) -> Result<providers::ChatResponse, RuntimeError> {
            std::future::pending().await
        }
    }
    fn runtime() -> AgentRuntime {
        let bus = Arc::new(EventBus::new(16));
        AgentRuntime::new(
            bus.clone(),
            Arc::new(ToolExecutor::new(bus)),
            Arc::new(Never),
        )
    }
    #[tokio::test]
    async fn commit_memory_proof_does_not_reopen_db_and_rejects_foreign_or_changed_recipes() {
        let root = tempfile::tempdir().unwrap();
        let config = storage::StorageConfig {
            db_path: root.path().join("proof.db"),
            ..Default::default()
        };
        let storage = storage::Storage::open(config.clone()).unwrap();
        let rt =
            runtime().with_run_store(crate::RunStore::open(&config, storage.handle()).unwrap());
        let recipes = BTreeMap::new();
        let proof = rt.prepare_restart_quiesce(&recipes).unwrap();
        // Reader spy: any DB preflight here fails. A poisoned reader is never
        // touched by memory validation (nor are owner/filesystem/provider ports).
        let store = rt.shared.run_store.get().unwrap();
        let poison = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = store.with_database::<()>(|_| panic!("reader disabled after inspect"));
        }));
        assert!(poison.is_err());
        rt.validate_restart_preflight(&proof, &recipes).unwrap();
        assert!(
            runtime()
                .validate_restart_preflight(&proof, &recipes)
                .is_err()
        );
        let changed = BTreeMap::from([(
            RunId::new(1),
            RecoveryRecipe {
                settings_reference: CORE_RECOVERY_SETTINGS_REFERENCE.into(),
                policy: ResumePolicy::CoreOwned,
                goal_id: None,
                team: None,
                lane: None,
                descriptor: None,
            },
        )]);
        assert!(rt.validate_restart_preflight(&proof, &changed).is_err());
    }
    #[tokio::test]
    async fn live_recipe_metadata_change_invalidates_proof_without_membership_change() {
        let rt = runtime();
        let run = rt
            .delegate_chat(
                "thread",
                Role::Orchestrator,
                "wait".into(),
                RunConfig {
                    interactive: true,
                    keep_alive: true,
                    ..Default::default()
                },
            )
            .unwrap();
        let recipes = BTreeMap::new();
        let proof = RestartPreflight {
            runtime: Arc::downgrade(&rt.shared),
            intents: rt.shared.quiesce.live_capture_intents(),
            metadata: rt.restart_recipe_metadata(),
            recipes: serde_json::to_string(&recipes).unwrap(),
        };
        rt.validate_restart_preflight(&proof, &recipes).unwrap();
        {
            let mut runs = rt.shared.runs.lock().unwrap();
            let config = &mut runs.get_mut(&run).unwrap().config;
            config.conversation = !config.conversation;
        }
        assert_eq!(proof.intents, rt.shared.quiesce.live_capture_intents());
        assert!(rt.validate_restart_preflight(&proof, &recipes).is_err());
        rt.cancel(run).unwrap();
        rt.wait(run).await.unwrap();
    }
}
