//! Narrow successor adapters. Production composition/Ready and cancel decisions belong to 4b-D.
use super::*;
use crate::ownership::{AdoptedOwnershipEvidence, OwnershipError, PlannedHold};
use std::collections::BTreeSet;
use storage::recovery::restart::{handoff::*, tools::*, *};

/// Holds the runtime's physical proof; a DB row or serialized hold cannot construct it.
pub struct RestartOwnershipEvidence {
    pub(super) runtime: AgentRuntime,
}
impl AdoptedOwnershipEvidence for RestartOwnershipEvidence {
    fn validate_hold(&self, hold: &PlannedHold) -> Result<(), OwnershipError> {
        self.runtime
            .validate_adopted_hold(hold)
            .map_err(|_| OwnershipError::PlannedMismatch)
    }
}
/// One-use tool replacement only. No history replacement, model/settings/permission grant.
pub struct PlannedToolSchemaBoundary {
    old: crate::quiesce::ProviderInput,
    new: crate::quiesce::ProviderInput,
}
impl PlannedToolSchemaBoundary {
    pub fn apply(
        self,
        old: &crate::quiesce::ProviderInput,
    ) -> Result<crate::quiesce::ProviderInput, RuntimeError> {
        if *old != self.old {
            return Err(error("tool boundary input changed"));
        }
        Ok(self.new)
    }
}
impl AgentRuntime {
    /// Mirror bounded file diagnostics through the existing writer when it is
    /// available. The receipt never supplies a phase, deadline or recovery grant.
    pub fn record_restart_boot_receipt(
        &self,
        receipt: &core_instance::handoff::receipt::RestartBootReceipt,
        restarting_at_ms: u64,
    ) -> Result<RestartBootProgress, RuntimeError> {
        receipt.validate().map_err(error)?;
        let store = self
            .shared
            .run_store
            .get()
            .ok_or_else(|| error("no run store"))?;
        let tx = store
            .with_database(|db| db.restart_transaction(&receipt.transaction_id))
            .map_err(error)?
            .ok_or_else(|| error("missing restart diagnostic"))?;
        if tx.metadata.config_identity != receipt.config_identity
            || tx.metadata.old_epoch != receipt.predecessor_epoch
            || tx.metadata.successor_epoch != receipt.successor_epoch
        {
            return Err(error("stale boot diagnostic"));
        }
        let (failure, exit_code) = match receipt.status {
            core_instance::handoff::receipt::BootStatus::Restarting => (None, None),
            core_instance::handoff::receipt::BootStatus::Failed { failure, exit_code } => {
                use core_instance::handoff::boot::BootFailure as F;
                (
                    Some(match failure {
                        F::BeforeAdopt => BootFailure::Bootstrap,
                        F::AdoptRejected => BootFailure::AdoptRejected,
                        F::AdoptAckUnknown => BootFailure::AckUnknown,
                        F::AdoptedBeforeReady | F::SandboxProbe => BootFailure::BeforeReady,
                        F::Interrupt => BootFailure::Interrupted,
                        F::Terminate => BootFailure::Terminated,
                    }),
                    Some(exit_code),
                )
            }
        };
        let value = RestartBootProgress {
            restarting_at_ms,
            failure,
            exit_code,
            diagnostic_revision: receipt.revision,
        };
        let result = store.handle.mutate_restart(
            receipt.transaction_id.clone(),
            Some(tx.revision),
            RestartMutation::BootProgress(value.clone()),
        );
        if store
            .with_database(|db| db.restart_boot_progress(&receipt.transaction_id))
            .map_err(error)?
            .as_ref()
            != Some(&value)
        {
            return Err(result
                .err()
                .map(error)
                .unwrap_or_else(|| error("boot diagnostic unconfirmed")));
        }
        Ok(value)
    }
    pub(super) fn read_adopted_restart(
        &self,
        completed: bool,
    ) -> Result<RestartTransaction, RuntimeError> {
        let proof = self
            .shared
            .planned_adoption
            .get()
            .ok_or_else(|| error("inherited proof required"))?;
        let store = self
            .shared
            .run_store
            .get()
            .ok_or_else(|| error("no run store"))?;
        let tx = store
            .with_database(|db| db.restart_transaction(proof.restart_id()))
            .map_err(error)?
            .ok_or_else(|| error("missing adopted restart"))?;
        if tx.metadata.restart_id != proof.restart_id()
            || tx.metadata.config_identity != proof.config_identity()
            || tx.metadata.successor_epoch != proof.successor_epoch()
            || tx.adopted_epoch != Some(proof.successor_epoch())
            || tx.metadata.successor_build != proof.successor_build()
            || tx.ticket_nonce_hash.as_deref() != Some(proof.nonce_hash())
            || !(tx.phase == RestartPhase::Adopted
                || completed && tx.phase == RestartPhase::Completed)
        {
            return Err(error("adopted restart proof mismatch"));
        }
        store
            .with_database(|db| db.validate_restart_capture_digest(proof.restart_id()))
            .map_err(error)?;
        Ok(tx)
    }
    /// The physical proof and canonical DB readback identify the same adopted transaction.
    pub fn adopted_restart_transaction(&self) -> Result<RestartTransaction, RuntimeError> {
        self.read_adopted_restart(true)
    }
    pub fn adopted_restart_ownership_evidence(
        &self,
    ) -> Result<RestartOwnershipEvidence, RuntimeError> {
        self.read_adopted_restart(false)?;
        Ok(RestartOwnershipEvidence {
            runtime: self.clone(),
        })
    }
    fn validate_adopted_hold(&self, hold: &PlannedHold) -> Result<(), RuntimeError> {
        let tx = self.read_adopted_restart(false)?;
        if hold.orphaned
            || hold.transaction_id != tx.metadata.restart_id
            || hold.capture_id != tx.metadata.capture_id
        {
            return Err(error("hold transaction mismatch"));
        }
        let progress = tx
            .owner_progress
            .iter()
            .find(|p| p.thread_id == hold.thread_id)
            .ok_or_else(|| error("hold absent from full owner inventory"))?;
        if progress.old_owner_id != hold.owner_id
            || progress.old_generation != hold.generation
            || progress.active_runs != hold.active_runs
            || progress.parked_runs != hold.parked_runs
            || !hold.active_runs.is_disjoint(&hold.parked_runs)
            || !progress.hold_saved
            || !progress.hold_read_back
            || progress.hold_removed
        {
            return Err(error("hold inventory mismatch"));
        }
        let members = tx
            .expected
            .iter()
            .filter(|m| m.thread_id.as_deref() == Some(&hold.thread_id))
            .collect::<Vec<_>>();
        if members
            .iter()
            .map(|m| m.run_id.clone())
            .collect::<BTreeSet<_>>()
            != hold.active_runs.union(&hold.parked_runs).cloned().collect()
            || members.iter().any(|m| {
                m.owner_id.as_deref() != Some(&hold.owner_id)
                    || m.owner_generation != Some(hold.generation)
            })
        {
            return Err(error("hold capture membership mismatch"));
        }
        let store = self
            .shared
            .run_store
            .get()
            .ok_or_else(|| error("no run store"))?;
        for member in members {
            let current = store
                .with_database(|db| db.resume_intent(&member.run_id))
                .map_err(error)?
                .ok_or_else(|| error("hold run missing"))?;
            if current.incarnation != member.incarnation
                || store
                    .with_database(|db| db.resume_intent_revision(&member.run_id))
                    .map_err(error)?
                    != Some(member.intent_revision)
            {
                return Err(error("hold run incarnation changed"));
            }
        }
        Ok(())
    }
    /// Call once the final learning cohort has normally returned and capture expected
    /// membership is persisted. Includes all eligible Pending, without fake attempts.
    pub async fn persist_restart_learning_inventory(
        &self,
        session: &mut RestartCaptureSession,
        pause: &crate::learning_suspend::LearningPause,
        suspension: &crate::learning_suspend::LearningSuspension,
    ) -> Result<LearningInventory, RuntimeError> {
        self.validate_learning_handoff(pause, suspension)
            .map_err(error)?;
        suspension.wait().await.map_err(error)?;
        self.validate_capture_session(session)?;
        let id = session.restart_id.as_str();
        if self.shared.active_restart_capture.get().map(String::as_str) != Some(id) {
            return Err(error("learning inventory requires current capture"));
        }
        let store = self
            .shared
            .run_store
            .get()
            .ok_or_else(|| error("no run store"))?;
        let inventory = store
            .with_database(|db| {
                db.restart_learning_inventory(self.learning_origin(), suspension.id().into())
            })
            .map_err(error)?;
        let tx = store
            .with_database(|db| db.restart_transaction(id))
            .map_err(error)?
            .ok_or_else(|| error("capture missing"))?;
        let write = store.handle.mutate_restart(
            id.into(),
            Some(tx.revision),
            RestartMutation::LearningInventory(inventory.clone()),
        );
        // Same identity readback after any ACK result. Never invent a replacement cohort.
        let saved = store
            .with_database(|db| db.restart_saved_learning_inventory(id))
            .map_err(error)?;
        if saved.as_ref() != Some(&inventory) {
            return Err(write
                .err()
                .map(error)
                .unwrap_or_else(|| error("learning inventory unconfirmed")));
        }
        session.revision = store
            .with_database(|db| db.restart_transaction(id))
            .map_err(error)?
            .ok_or_else(|| error("capture missing"))?
            .revision;
        Ok(inventory)
    }
    /// Ready gates requeue, not learning completion. Same-member receipts survive
    /// retry and subsequent Start; PID liveness is intentionally not consulted.
    pub fn resume_planned_learning(&self) -> Result<Vec<LearningHandoffStatus>, RuntimeError> {
        let tx = self.read_adopted_restart(true)?;
        if tx.ready_at_ms.is_none() {
            return Err(error("learning requeue before Ready"));
        }
        let store = self
            .shared
            .run_store
            .get()
            .ok_or_else(|| error("no run store"))?;
        let id = tx.metadata.restart_id;
        let members = store
            .with_database(|db| db.restart_learning_members(&id))
            .map_err(error)?;
        for status in members.into_iter().filter(|s| s.disposition == "pending") {
            let tx = self.read_adopted_restart(true)?;
            let result = store.handle.mutate_restart(
                id.clone(),
                Some(tx.revision),
                RestartMutation::ResumeLearning {
                    task_id: status.member.key.task_id.clone(),
                    to: self.learning_origin(),
                },
            );
            let after = store
                .with_database(|db| db.restart_learning_members(&id))
                .map_err(error)?;
            if after
                .iter()
                .find(|s| s.member.key.task_id == status.member.key.task_id)
                .is_none_or(|s| s.disposition == "pending")
            {
                return Err(result
                    .err()
                    .map(error)
                    .unwrap_or_else(|| error("learning requeue unconfirmed")));
            }
        }
        let statuses = store
            .with_database(|db| db.restart_learning_members(&id))
            .map_err(error)?;
        // Only after Ready and durable requeue. The coordinator may retain its
        // learning pause; opening that gate wakes the owned scheduler later.
        self.ensure_learning_scheduler();
        self.shared
            .learning_jobs_changed
            .send_modify(|n| *n = n.wrapping_add(1));
        Ok(statuses)
    }
    /// Cold disposition of Interrupted jobs that predate a complete inventory.
    /// The existing origin janitor still handles process-restart omissions.
    pub fn quarantine_cold_learning(
        &self,
        probe: &dyn crate::learning_jobs::OriginProbe,
    ) -> Result<(), RuntimeError> {
        if self.has_planned_restart_handoff() {
            return Err(error("planned successor is not cold"));
        }
        let store = self
            .shared
            .run_store
            .get()
            .ok_or_else(|| error("no run store"))?;
        // Cleanup must also work after the quick learning profile is removed.
        let queue = self.learning_job_queue().unwrap_or_else(|| {
            crate::learning_jobs::LearningJobQueue::new(
                store.handle.clone(),
                storage::StorageConfig {
                    db_path: store.db_path.clone(),
                    ..Default::default()
                },
                self.learning_origin(),
            )
        });
        if queue.origin().kind != storage::learning_jobs::LearningOriginKind::Core
            || queue.origin().config_identity.is_none()
        {
            return Err(error("cold learning requires an identified core origin"));
        }
        // A graph may already be Completed when a crash precedes learning requeue.
        // Its pending learning membership still has no authority on a cold boot.
        for id in store
            .with_database(|db| db.restart_learning_candidates())
            .map_err(error)?
        {
            let cohort = store.with_database(|db| {
                Ok((
                    db.restart_transaction(&id)?,
                    db.restart_saved_learning_inventory(&id)?,
                    db.restart_learning_members(&id)?,
                ))
            });
            let (Some(tx), Some(inventory), members) = (match cohort {
                Ok(cohort) => cohort,
                Err(e) if e.is_data_decode_error() => {
                    tracing::warn!(restart_id = %id, "corrupt cold learning cohort isolated");
                    continue;
                }
                Err(e) => return Err(error(e)),
            }) else {
                continue;
            };
            if !matches!(
                tx.phase,
                RestartPhase::Invalidated
                    | RestartPhase::Aborted
                    | RestartPhase::Superseded
                    | RestartPhase::Completed
            ) {
                return Err(error("cold learning requires graph quarantine first"));
            }
            if tx.metadata.config_identity
                != inventory.origin.config_identity.as_deref().unwrap_or("")
                || members
                    .iter()
                    .filter(|m| m.disposition == "pending")
                    .any(|m| !inventory.members.contains(&m.member))
            {
                tracing::warn!(restart_id = %id, "cold learning origin mismatch isolated");
                continue;
            }
            if inventory.origin.kind == storage::learning_jobs::LearningOriginKind::Core
                && inventory.origin.config_identity == queue.origin().config_identity
                && inventory.origin.instance != queue.origin().instance
                && probe.liveness(&inventory.origin) == crate::learning_jobs::OriginLiveness::Dead
            {
                match store.handle.mutate_restart(
                    tx.metadata.restart_id,
                    Some(tx.revision),
                    RestartMutation::QuarantineLearningCold,
                ) {
                    Ok(_) => {}
                    Err(e) if e.is_data_decode_error() => {
                        tracing::warn!(restart_id = %id, "corrupt cold learning member isolated")
                    }
                    Err(e) => return Err(error(e)),
                }
            }
        }
        let bound = store
            .with_database(|db| db.pending_restart_learning_task_ids())
            .map_err(error)?;
        for job in store
            .with_database(|db| {
                db.learning_jobs_isolated(&[storage::learning_jobs::LearningJobState::Interrupted])
            })
            .map_err(error)?
        {
            if !bound.contains(&job.task_id)
                && job.origin.kind == storage::learning_jobs::LearningOriginKind::Core
                && job.origin.config_identity == queue.origin().config_identity
                && job.origin.instance != queue.origin().instance
                && probe.liveness(&job.origin) == crate::learning_jobs::OriginLiveness::Dead
            {
                match queue.change(
                    &job.key(),
                    storage::learning_jobs::LearningAttemptChange::Abandon(
                        storage::learning_jobs::LearningEndReason::NoResumeAuthority,
                    ),
                ) {
                    Ok(_) => {}
                    Err(e) if e.is_data_decode_error() => {
                        tracing::warn!(task_id = %job.task_id, "corrupt cold learning job isolated");
                    }
                    Err(e) => return Err(error(e)),
                }
            }
        }
        Ok(())
    }
    /// Cold startup only; graph quarantine must have run first. This never dispatches.
    pub fn quarantine_cold_learning_from_proc(&self) -> Result<(), RuntimeError> {
        let probe = crate::learning_jobs::ProcOriginProbe::new(&self.learning_origin());
        self.quarantine_cold_learning(&probe)
    }
    /// Metadata-only build-owned names for a caller constructing RestartMetadata.
    /// This is deliberately broader than --describe-handoff's standard-tools-v1.
    pub fn restart_builtin_tool_names(&self) -> BTreeSet<String> {
        self.restart_builtin_tool_catalog(&[])
            .into_iter()
            .map(|tool| tool.name)
            .collect()
    }
    /// Metadata-only full build registry with exact visible projections overlaid.
    /// Handoff capture and fixture builders must not use the role-filtered list
    /// as the build-owned name set. This does not register or execute any tool.
    pub fn restart_builtin_tool_catalog(
        &self,
        effective: &[providers::ToolSpec],
    ) -> Vec<providers::ToolSpec> {
        let executor = self
            .shared
            .executor
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.restart_builtin_catalog_from_executor(&executor, effective)
    }
    fn restart_builtin_catalog_from_executor(
        &self,
        executor: &::tools::ToolExecutor,
        effective: &[providers::ToolSpec],
    ) -> Vec<providers::ToolSpec> {
        #[cfg(any(test, feature = "recovery-test-support"))]
        if let Some(raw) = self.shared.restart_builtin_catalog_fixture.get() {
            return crate::agent_loop::tool_catalog::overlay_builtin_catalog(
                raw.clone(),
                effective,
            );
        }
        crate::agent_loop::tool_catalog::builtin_catalog(executor, effective)
    }
    /// Explicit predecessor-build fixture only; scoped to this runtime instance.
    /// Raw build metadata may be replaced once, never provider-visible projections.
    #[cfg(any(test, feature = "recovery-test-support"))]
    pub fn set_restart_builtin_catalog_for_fixture(
        &self,
        raw: Vec<providers::ToolSpec>,
    ) -> Result<(), RuntimeError> {
        self.shared
            .restart_builtin_catalog_fixture
            .set(raw)
            .map_err(|_| error("fixture builtin catalog already fixed"))
    }
    fn validated_restart_builtin_catalog(
        &self,
        executor: &::tools::ToolExecutor,
        effective: &[providers::ToolSpec],
    ) -> Result<Vec<providers::ToolSpec>, RuntimeError> {
        let catalog = self.restart_builtin_catalog_from_executor(executor, effective);
        #[cfg(any(test, feature = "recovery-test-support"))]
        if self.shared.restart_builtin_catalog_fixture.get().is_some() {
            let compiled = crate::agent_loop::tool_catalog::builtin_catalog(executor, &[]);
            if effective.iter().any(|tool| {
                compiled.iter().any(|s| s.name == tool.name)
                    && !catalog.iter().any(|s| s.name == tool.name)
            }) {
                return Err(error(
                    "effective builtin missing from fixture build catalog",
                ));
            }
        }
        Ok(catalog)
    }
    /// Predecessor-only: fix the exact effective schemas and non-tool authority after
    /// expected capture has been persisted, before committing/arming the restart.
    /// M2 adapter: snapshot the actual sent input before Commit. Metadata names
    /// select compiled builtins, never rewrite schemas or invent a fresh request.
    pub fn persist_restart_tool_snapshots(
        &self,
        session: &mut RestartCaptureSession,
    ) -> Result<(), RuntimeError> {
        self.validate_capture_session(session)?;
        let store = self
            .shared
            .run_store
            .get()
            .ok_or_else(|| error("no run store"))?;
        let tx = store
            .with_database(|db| db.restart_transaction(&session.restart_id))
            .map_err(error)?
            .ok_or_else(|| error("missing capture"))?;
        for member in &tx.expected {
            let row = store
                .with_database(|db| db.resume_intent(&member.run_id))
                .map_err(error)?
                .ok_or_else(|| error("capture intent missing"))?;
            let intent = crate::quiesce::ResumeIntent::decode(row).map_err(error)?;
            let Some(input) = intent.provider_input else {
                continue;
            };
            let catalog = {
                let executor = self
                    .shared
                    .executor
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                self.validated_restart_builtin_catalog(&executor, &input.request.tools)?
            };
            let policy = self.execution_policy(Role::from_name(&intent.role).map_err(error)?);
            self.persist_restart_tool_snapshot(
                session,
                intent.run_id,
                &catalog,
                &policy_fingerprint(&policy)?,
            )?;
        }
        Ok(())
    }
    /// Preview only the compiled part. External schemas are preserved exactly;
    /// the real loop performs its ordinary external/config compatibility checks.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn prepare_planned_run_tools(
        &self,
        run: RunId,
        incarnation: &str,
        input: &crate::quiesce::ProviderInput,
        config: &RunConfig,
        role: Role,
        parent: Option<RunId>,
        model: &Arc<dyn AgentModel>,
        mcp: Option<&::tools::mcp::types::FrozenMcpSnapshot>,
    ) -> Result<(), RuntimeError> {
        self.read_adopted_restart(false)?;
        let project = self.shared.run_project(config);
        let shared = loop_shared(&Arc::downgrade(&self.shared), project.as_deref())
            .ok_or_else(|| error("no runtime"))?;
        // The actual restored loop registers this same frozen catalog before
        // composing tools. No discovery, credentials or mutable live catalog.
        let raw = crate::agent_loop::tool_catalog::restored_specs(&shared.executor, mcp)
            .map_err(|kind| error(format!("saved MCP metadata invalid: {kind:?}")))?;

        let mut policy = self
            .execution_policy(role)
            .for_run_config(config, parent.is_none())
            .with_image_generation(model.image_generation_enabled());
        if self.goal_thread(run).is_none() {
            policy.capabilities.allowed_tools.remove("plan_read");
            policy.capabilities.allowed_tools.remove("plan_update");
        }
        if let Some(snapshot) = mcp {
            crate::agent_loop::tool_catalog::allow_saved_mcp_tools(&mut policy, snapshot);
        }
        let settings = self.run_js_settings();
        let family =
            crate::prompt::classify(&model.selected_model(role, config.category.as_deref()));
        let effective = crate::agent_loop::tool_catalog::compose(
            raw,
            &policy,
            crate::agent_loop::tool_catalog::CatalogOptions {
                skills: shared.skills.is_some(),
                child: parent.is_some(),
                web: self.web_tools_enabled(),
                artifacts: self.artifact_pages().is_some(),
                run_js: config.team.is_none()
                    && self.run_js_offered_to(role, config.category.as_deref()),
                run_js_settings: &settings,
                family,
            },
        );
        // Compare provider-visible tools, not the wider authorized catalog.
        let effective = crate::edit_tools::preview(
            model.as_ref(),
            &run.to_string(),
            role,
            config.category.as_deref(),
            &effective,
        )?;
        let catalog = self.validated_restart_builtin_catalog(&shared.executor, &effective)?;
        let mut candidate = input.clone();
        candidate.request.tools = effective;
        let evidence = self.prepare_restart_tool_evidence(
            run,
            incarnation,
            &candidate,
            &catalog,
            &policy_fingerprint(&policy)?,
        )?;
        if let Some(boundary) = self.consume_restart_tool_boundary(evidence, candidate.clone())?
            && boundary.apply(input)? != candidate
        {
            return Err(error("tool boundary candidate changed"));
        }
        Ok(())
    }
    pub fn persist_restart_tool_snapshot(
        &self,
        session: &mut RestartCaptureSession,
        run: RunId,
        catalog: &[providers::ToolSpec],
        permissions_fingerprint: &str,
    ) -> Result<(), RuntimeError> {
        self.deny_deleted_run(&run.to_string()).map_err(error)?;
        self.validate_capture_session(session)?;
        let id = session.restart_id.as_str();
        if self.shared.active_restart_capture.get().map(String::as_str) != Some(id) {
            return Err(error("tool snapshot requires current capture"));
        }
        let store = self
            .shared
            .run_store
            .get()
            .ok_or_else(|| error("no run store"))?;
        let tx = store
            .with_database(|db| db.restart_transaction(id))
            .map_err(error)?
            .ok_or_else(|| error("missing capture"))?;
        let member = tx
            .expected
            .iter()
            .find(|m| m.run_id == run.to_string())
            .ok_or_else(|| error("capture member missing"))?;
        let row = store
            .with_database(|db| db.resume_intent(&run.to_string()))
            .map_err(error)?
            .ok_or_else(|| error("capture intent missing"))?;
        let intent = crate::quiesce::ResumeIntent::decode(row).map_err(error)?;
        if intent.incarnation != member.incarnation {
            return Err(error("capture tool incarnation mismatch"));
        }
        let input = intent
            .provider_input
            .ok_or_else(|| error("capture has no provider input"))?;
        let snapshot = tool_snapshot(&input, catalog, permissions_fingerprint)?;
        let result = store.handle.mutate_restart(
            id.into(),
            Some(tx.revision),
            RestartMutation::CaptureTools {
                run_id: run.to_string(),
                incarnation: member.incarnation.clone(),
                snapshot: snapshot.clone(),
            },
        );
        if store
            .with_database(|db| db.restart_tool_snapshot(id, &run.to_string(), &member.incarnation))
            .map_err(error)?
            .as_ref()
            != Some(&snapshot)
        {
            return Err(result
                .err()
                .map(error)
                .unwrap_or_else(|| error("tool capture readback mismatch")));
        }
        session.revision = store
            .with_database(|db| db.restart_transaction(id))
            .map_err(error)?
            .ok_or_else(|| error("capture missing"))?
            .revision;
        Ok(())
    }
    /// Compare exact saved provider input and the candidate input, never reconstructed
    /// history. Current authority supplies the non-wire permissions fingerprint.
    pub fn prepare_restart_tool_evidence(
        &self,
        run: RunId,
        incarnation: &str,
        new: &crate::quiesce::ProviderInput,
        new_catalog: &[providers::ToolSpec],
        permissions_fingerprint: &str,
    ) -> Result<RestartToolsEvidence, RuntimeError> {
        let tx = self.read_adopted_restart(false)?;
        self.captured_provider_input(&tx, run, incarnation)?;
        let store = self
            .shared
            .run_store
            .get()
            .ok_or_else(|| error("no run store"))?;
        let old = store
            .with_database(|db| {
                db.restart_tool_snapshot(&tx.metadata.restart_id, &run.to_string(), incarnation)
            })
            .map_err(error)?
            .ok_or_else(|| error("no predecessor tool snapshot"))?;
        RestartToolsEvidence::compare(
            run.to_string(),
            incarnation.into(),
            tx.metadata.old_build,
            tx.metadata.successor_build,
            old,
            tool_snapshot(new, new_catalog, permissions_fingerprint)?,
        )
        .map_err(error)
    }
    fn captured_provider_input(
        &self,
        tx: &RestartTransaction,
        run: RunId,
        incarnation: &str,
    ) -> Result<crate::quiesce::ProviderInput, RuntimeError> {
        if !tx
            .expected
            .iter()
            .any(|m| m.run_id == run.to_string() && m.incarnation == incarnation)
        {
            return Err(error("tool capture incarnation mismatch"));
        }
        let store = self
            .shared
            .run_store
            .get()
            .ok_or_else(|| error("no run store"))?;
        if !store
            .with_database(|db| {
                db.restart_tool_member_eligible(
                    &tx.metadata.restart_id,
                    &run.to_string(),
                    incarnation,
                )
            })
            .map_err(error)?
        {
            return Err(error("tool capture not eligible"));
        }
        let intent = store
            .with_database(|db| db.resume_intent(&run.to_string()))
            .map_err(error)?
            .ok_or_else(|| error("tool intent missing"))?;
        if intent.incarnation != incarnation {
            return Err(error("tool intent incarnation changed"));
        }
        let intent = crate::quiesce::ResumeIntent::decode(intent).map_err(error)?;
        if !intent.active || intent.stop_reason.is_some() {
            return Err(error("tool intent stopped"));
        }
        intent
            .provider_input
            .ok_or_else(|| error("no captured provider input"))
    }
    /// Returns no new capability after consumption, including unknown-ACK retries.
    /// A failed/unknown consumption never grants a speculative boundary.
    pub fn consume_restart_tool_boundary(
        &self,
        evidence: RestartToolsEvidence,
        new: crate::quiesce::ProviderInput,
    ) -> Result<Option<PlannedToolSchemaBoundary>, RuntimeError> {
        let tx = self.read_adopted_restart(false)?;
        let run = evidence.run_id.parse::<RunId>().map_err(error)?;
        let old = self.captured_provider_input(&tx, run, &evidence.incarnation)?;
        let mut expected = old.clone();
        expected.request.tools = new.request.tools.clone();
        if expected != new
            || serde_json::to_value(&old.request.tools).map_err(error)?
                != serde_json::Value::Array(evidence.old.effective.clone())
            || serde_json::to_value(&new.request.tools).map_err(error)?
                != serde_json::Value::Array(evidence.new.effective.clone())
        {
            return Err(error("tool boundary changes more than tools"));
        }
        let store = self
            .shared
            .run_store
            .get()
            .ok_or_else(|| error("no run store"))?;
        let id = tx.metadata.restart_id.clone();
        if let Some((saved, consumed)) = store
            .with_database(|db| {
                db.restart_tools_evidence(&id, &evidence.run_id, &evidence.incarnation)
            })
            .map_err(error)?
        {
            if saved != evidence {
                return Err(error("tool comparison changed"));
            }
            if consumed {
                return Ok(None);
            }
        } else {
            store
                .handle
                .mutate_restart(
                    id.clone(),
                    Some(tx.revision),
                    RestartMutation::Tools(Box::new(evidence.clone())),
                )
                .map_err(error)?;
        }
        if !evidence.boundary {
            return Ok(None);
        }
        let tx = self.read_adopted_restart(false)?;
        store
            .handle
            .mutate_restart(
                id,
                Some(tx.revision),
                RestartMutation::ConsumeTools {
                    run_id: evidence.run_id,
                    incarnation: evidence.incarnation,
                },
            )
            .map_err(error)?;
        Ok(Some(PlannedToolSchemaBoundary { old, new }))
    }
}
fn error(e: impl ToString) -> RuntimeError {
    crate::quiesce::runtime::error(e)
}

pub(super) fn tool_snapshot(
    input: &crate::quiesce::ProviderInput,
    catalog: &[providers::ToolSpec],
    permissions: &str,
) -> Result<ToolSnapshot, RuntimeError> {
    let mut stable = input.clone();
    stable.request.tools.clear();
    Ok(ToolSnapshot {
        effective: input
            .request
            .tools
            .iter()
            .map(serde_json::to_value)
            .collect::<Result<_, _>>()
            .map_err(error)?,
        builtins: catalog
            .iter()
            .map(serde_json::to_value)
            .collect::<Result<_, _>>()
            .map_err(error)?,
        stable_context: restart_digest(&(stable, permissions)).map_err(error)?,
    })
}

// Role/build-derived tool grants are compared as ordered catalog evidence. The
// non-tool execution authority must remain identical across the handoff.
pub(super) fn policy_fingerprint(p: &crate::ExecutionPolicy) -> Result<String, RuntimeError> {
    restart_digest(&(
        &p.role_name,
        p.capabilities.can_delegate,
        p.escalation_approval,
        p.escalate_to_user_on_deny,
    ))
    .map_err(error)
}
