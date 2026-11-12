//! Authority-free prepare/release. A paused claim is never an executable run.
use super::*;
use crate::quiesce::{
    RecoveryClaim, RecoveryClaimState, RecoveryComponent, ResumeIntent, RunRecoveryRecord,
};
use std::collections::BTreeSet;
use storage::recovery::RecoveryClaimRequest;

pub(super) struct PausedRecoveryIndex {
    pub(super) claim_id: String,
    pub(super) old_incarnation: String,
}

/// Owns one prepared component's process-local reservations. Dropping it does
/// not release the durable claim or undo any revision; startup sees PendingClaim.
/// No authority, task, provider admission or wake can be obtained from this type.
pub struct PausedRecoveryComponent {
    runtime: Weak<Shared>,
    claim: RecoveryClaim,
    members: Vec<ResumeIntent>,
    _reservations: Vec<SnapshotReservation>,
    _startup: storage::RecoveryStartupComplete,
}
impl PausedRecoveryComponent {
    pub fn claim(&self) -> &RecoveryClaim {
        &self.claim
    }
}

fn error(reason: impl ToString) -> RuntimeError {
    crate::quiesce::runtime::error(reason)
}

impl AgentRuntime {
    pub(super) fn fence_paused_stop(
        &self,
        run: RunId,
        old: &str,
        reason: crate::quiesce::StopReason,
    ) -> Result<(), RuntimeError> {
        self.shared
            .quiesce
            .update(run, old, self.shared.run_store.get(), |intent| {
                intent.active = false;
                intent.resume_requested = false;
                intent.stop_reason = Some(reason);
            })
            .map_err(error)
    }
    pub fn has_paused_recovery(&self) -> bool {
        !self.paused_recovery_run_ids().is_empty()
    }
    pub(crate) fn paused_recovery_run_ids(&self) -> Vec<RunId> {
        self.shared
            .spawn_intents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .filter_map(|(run, index)| index.paused_recovery.as_ref().map(|_| *run))
            .collect()
    }

    /// Prepare only the currently validated component, after the exclusive
    /// startup phase. Saved recipes never manufacture a RunConfig or permit.
    pub fn prepare_component(
        &self,
        component: &RecoveryComponent,
        startup: storage::RecoveryStartupComplete,
        ownership_root: &std::path::Path,
    ) -> Result<PausedRecoveryComponent, RuntimeError> {
        let _dispatch = self.dispatch_registration_guard()?;
        if !self.recovery_plan()?.components.contains(component) {
            return Err(error("component is no longer a recovery candidate"));
        }
        let store = self
            .shared
            .run_store
            .get()
            .ok_or_else(|| error("no run store"))?;
        let manifest = store
            .with_database(|db| {
                db.recovery_manifests()?
                    .into_iter()
                    .find(|m| m.manifest_id == component.manifest_id)
                    .ok_or_else(|| storage::StorageError::Io("missing recovery manifest".into()))?
                    .record
                    .map_err(|_| storage::StorageError::Io("corrupt recovery manifest".into()))
            })
            .map_err(error)?;
        let mut ids = manifest
            .members
            .iter()
            .map(|m| m.run_id.parse::<RunId>().map_err(error))
            .collect::<Result<Vec<_>, _>>()?;
        ids.sort_unstable();
        let gates = ids
            .iter()
            .map(|id| self.stop_restore_gate(*id))
            .collect::<Vec<_>>();
        let _stop = gates
            .iter()
            .map(|gate| {
                gate.lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
            })
            .collect::<Vec<_>>();
        let mut indexes = self
            .shared
            .spawn_intents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        {
            let admissions = self
                .shared
                .admissions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let runs = lock_runs(&self.shared.runs);
            if ids.iter().any(|id| {
                indexes.get(id).is_some_and(|i| {
                    i.execution.is_some() || i.paused_recovery.is_some() || i.cancelled
                }) || admissions.contains_key(id)
                    || runs.contains_key(id)
            }) {
                return Err(error(
                    "recovery member already registered, reserved or cancelled",
                ));
            }
        }
        let mut sources = Vec::new();
        let mut incarnations = Vec::new();
        for member in &manifest.members {
            let record: RunRecoveryRecord =
                serde_json::from_str(&member.record_json).map_err(error)?;
            let (intent, snapshot) = store
                .with_database(|db| {
                    Ok((
                        db.resume_intent(&member.run_id)?.ok_or_else(|| {
                            storage::StorageError::Io("missing recovery intent".into())
                        })?,
                        db.run_context(&member.run_id)?,
                    ))
                })
                .map_err(error)?;
            let intent = ResumeIntent::decode(intent).map_err(error)?;
            let execution = member.resume.then(|| ExecutionControl::new(false));
            if let Some(execution) = &execution {
                incarnations.push((member.run_id.clone(), execution.publication().incarnation()));
            }
            sources.push((record, intent, snapshot, member.snapshot.clone(), execution));
        }
        // Validate process-local tokens before the durable claim. No later
        // prepare step is fallible or writes a snapshot consumption marker.
        let mut reservations = Vec::new();
        for (record, _, snapshot, receipt, _) in &sources {
            if let Some(receipt) = receipt {
                let snapshot = snapshot
                    .as_ref()
                    .ok_or_else(|| error("missing recovery snapshot"))?;
                reservations.push(SnapshotReservation::prepare_recovery(
                    store,
                    record.run_id,
                    snapshot,
                    receipt,
                )?);
            }
        }
        let mut nonce = [0_u8; 16];
        getrandom::fill(&mut nonce).map_err(error)?;
        let claim = store
            .handle
            .claim_recovery_component(
                RecoveryClaimRequest {
                    claim_id: format!("recovery-{:032x}", u128::from_le_bytes(nonce)),
                    manifest_id: component.manifest_id.clone(),
                    incarnations,
                },
                &startup,
                ownership_root,
            )
            .map_err(error)?;
        let members = sources
            .iter()
            .map(|(_, intent, _, _, _)| intent.clone())
            .collect::<Vec<_>>();
        self.shared.quiesce.adopt(members.clone(), true);
        for (record, intent, _, _, execution) in sources {
            let index = indexes.entry(record.run_id).or_default();
            index.parent = record.parent;
            index.execution = execution;
            index.paused_recovery = Some(PausedRecoveryIndex {
                claim_id: claim.claim_id.clone(),
                old_incarnation: intent.incarnation,
            });
        }
        Ok(PausedRecoveryComponent {
            runtime: Arc::downgrade(&self.shared),
            claim,
            members,
            _reservations: reservations,
            _startup: startup,
        })
    }

    /// Only an unactivated claim may be released. A failed/uncertain ACK retains
    /// the index unless a read-only claim read proves that release committed.
    pub fn release_prepared_component(
        &self,
        paused: PausedRecoveryComponent,
        reason: &str,
    ) -> Result<(), RuntimeError> {
        if !Weak::ptr_eq(&paused.runtime, &Arc::downgrade(&self.shared)) {
            return Err(error("foreign paused recovery component"));
        }
        let expected = paused
            .members
            .iter()
            .map(|i| i.run_id)
            .collect::<BTreeSet<_>>();
        self.release_indexed_recovery(&paused.claim.claim_id, reason, Some(&expected))
    }

    fn release_indexed_recovery(
        &self,
        claim_id: &str,
        reason: &str,
        expected: Option<&BTreeSet<RunId>>,
    ) -> Result<(), RuntimeError> {
        let members = {
            let indexes = self
                .shared
                .spawn_intents
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut members = indexes
                .iter()
                .filter_map(|(id, index)| {
                    index
                        .paused_recovery
                        .as_ref()
                        .filter(|p| p.claim_id == claim_id)
                        .map(|p| (*id, p.old_incarnation.clone(), index.execution.clone()))
                })
                .collect::<Vec<_>>();
            members.sort_unstable_by_key(|(id, _, _)| *id);
            members
        };
        let ids = members
            .iter()
            .map(|(id, _, _)| *id)
            .collect::<BTreeSet<_>>();
        if ids.is_empty() || expected.is_some_and(|expected| *expected != ids) {
            return Err(error("paused recovery index changed"));
        }
        let gates = ids
            .iter()
            .map(|id| self.stop_restore_gate(*id))
            .collect::<Vec<_>>();
        let _stop = gates
            .iter()
            .map(|gate| {
                gate.lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
            })
            .collect::<Vec<_>>();
        let _publication = members
            .iter()
            .filter_map(|(_, _, execution)| execution.as_ref().map(|e| e.publication_gate()))
            .collect::<Vec<_>>();
        let mut indexes = self
            .shared
            .spawn_intents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if members.iter().any(|(id, old, execution)| {
            indexes.get(id).is_none_or(|index| {
                index
                    .paused_recovery
                    .as_ref()
                    .is_none_or(|p| p.claim_id != claim_id || p.old_incarnation != *old)
                    || match (&index.execution, execution) {
                        (Some(a), Some(b)) => !Arc::ptr_eq(a, b),
                        (None, None) => false,
                        _ => true,
                    }
            })
        }) {
            return Err(error("paused recovery index changed"));
        }
        let store = self
            .shared
            .run_store
            .get()
            .ok_or_else(|| error("no run store"))?;
        if let Err(write_error) = store
            .handle
            .release_recovery_claim(claim_id.into(), reason.into())
        {
            let released = store
                .with_database(|db| db.recovery_claims())
                .map_err(error)?
                .into_iter()
                .any(|c| {
                    c.claim_id == claim_id
                        && c.state
                            == RecoveryClaimState::Released {
                                reason: reason.into(),
                            }
                });
            if !released {
                return Err(error(write_error));
            }
        }
        for (id, old, execution) in &members {
            if let Some(execution) = execution {
                execution.stop_admission();
            }
            self.shared.quiesce.forget_released(*id, old);
            indexes.remove(id);
        }
        Ok(())
    }

    /// Operator shutdown only; planned restart deliberately carries Prepared to
    /// the next process, where 06c must resolve it under current authority.
    pub(crate) fn release_shutdown_recovery(&self) -> Result<(), RuntimeError> {
        let claims = self
            .shared
            .spawn_intents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .filter_map(|i| i.paused_recovery.as_ref().map(|p| p.claim_id.clone()))
            .collect::<BTreeSet<_>>();
        for claim in claims {
            self.release_indexed_recovery(&claim, "shutdown", None)?;
        }
        Ok(())
    }
}
