//! Narrow successor adapters. Production composition/Ready and cancel decisions belong to 4b-D.
use super::*;
use crate::ownership::{AdoptedOwnershipEvidence, OwnershipError, PlannedHold};
use std::collections::BTreeSet;
use storage::recovery::restart::{handoff::*, tools::*, *};

/// Holds the runtime's physical proof; a DB row or serialized hold cannot construct it.
pub struct RestartOwnershipEvidence {
    runtime: AgentRuntime,
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
                        F::AdoptedBeforeReady => BootFailure::BeforeReady,
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
            != hold.active_runs
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
        let queue = self
            .learning_job_queue()
            .ok_or_else(|| error("no learning queue"))?;
        let store = self
            .shared
            .run_store
            .get()
            .ok_or_else(|| error("no run store"))?;
        // A graph may already be Completed when a crash precedes learning requeue.
        // Its pending learning membership still has no authority on a cold boot.
        for tx in store
            .with_database(|db| db.restart_transactions())
            .map_err(error)?
        {
            let inventory = store
                .with_database(|db| db.restart_saved_learning_inventory(&tx.metadata.restart_id))
                .map_err(error)?;
            if let Some(inventory) = inventory
                && inventory.origin.kind == storage::learning_jobs::LearningOriginKind::Core
                && inventory.origin.config_identity == queue.origin().config_identity
                && inventory.origin.instance != queue.origin().instance
                && probe.liveness(&inventory.origin) == crate::learning_jobs::OriginLiveness::Dead
            {
                store
                    .handle
                    .mutate_restart(
                        tx.metadata.restart_id,
                        Some(tx.revision),
                        RestartMutation::QuarantineLearningCold,
                    )
                    .map_err(error)?;
            }
        }
        for job in store
            .with_database(|db| {
                db.learning_jobs(&[storage::learning_jobs::LearningJobState::Interrupted])
            })
            .map_err(error)?
        {
            if job.origin.kind == storage::learning_jobs::LearningOriginKind::Core
                && job.origin.config_identity == queue.origin().config_identity
                && job.origin.instance != queue.origin().instance
                && probe.liveness(&job.origin) == crate::learning_jobs::OriginLiveness::Dead
            {
                queue
                    .change(
                        &job.key(),
                        storage::learning_jobs::LearningAttemptChange::Abandon(
                            storage::learning_jobs::LearningEndReason::NoResumeAuthority,
                        ),
                    )
                    .map_err(error)?;
            }
        }
        Ok(())
    }
    /// Predecessor-only: fix the exact effective schemas and non-tool authority after
    /// expected capture has been persisted, before committing/arming the restart.
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

fn tool_snapshot(
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
