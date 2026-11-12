//! ACK uncertainty never yields an executable handle merely from claim.state.
use super::*;
use storage::recovery::RecoveryClaimSnapshot;
#[path = "cleanup_stop.rs"]
mod stop;

pub enum RecoveryClaimHandle {
    Retryable(Box<PausedRecoveryComponent>),
    Quarantined(Box<RecoveryCleanupHandle>),
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryCleanupState {
    ActivatedUnpublished,
    Released,
    Unresolved,
}
/// Process-local identity fence, not serializable and not an activation token.
pub struct RecoveryCleanupHandle {
    runtime: Weak<Shared>,
    claim_id: String,
    state: RecoveryCleanupState,
    observation: Option<RecoveryClaimSnapshot>,
    members: Vec<(RunId, String, Option<Arc<ExecutionControl>>)>,
}
impl RecoveryCleanupHandle {
    pub fn claim_id(&self) -> &str {
        &self.claim_id
    }
    pub fn state(&self) -> RecoveryCleanupState {
        self.state
    }
}

impl AgentRuntime {
    /// Recover a consumed handle after a pre-ACK failure. Only a fully validated
    /// Prepared/all-old claim can be retried. Other states have cleanup only.
    pub fn recover_recovery_claim(
        &self,
        claim_id: &str,
        startup: storage::RecoveryStartupComplete,
        ownership_root: &std::path::Path,
        authority: &CurrentRecoveryAuthority,
    ) -> Result<RecoveryClaimHandle, RuntimeError> {
        let store = self
            .shared
            .run_store
            .get()
            .ok_or_else(|| error("no run store"))?;
        let observation = store.with_database(|db| db.recovery_claim_snapshot(claim_id));
        if observation.as_ref().is_ok_and(prepared_unexecuted) {
            let proof =
                store
                    .handle
                    .validate_prepared_claim(claim_id.into(), &startup, ownership_root);
            if let Ok(proof) = proof
                && let Ok(paused) =
                    self.adopt_prepared_claim(proof, startup, ownership_root, authority)
            {
                return Ok(RecoveryClaimHandle::Retryable(Box::new(paused)));
            }
        }
        // Re-read after a failed validation rather than joining observations
        // from before and after the activation transaction.
        Ok(RecoveryClaimHandle::Quarantined(Box::new(
            self.recovery_cleanup_handle(claim_id)?,
        )))
    }

    pub(super) fn recovery_cleanup_handle(
        &self,
        claim_id: &str,
    ) -> Result<RecoveryCleanupHandle, RuntimeError> {
        let store = self
            .shared
            .run_store
            .get()
            .ok_or_else(|| error("no run store"))?;
        let observation = store
            .with_database(|db| db.recovery_claim_snapshot(claim_id))
            .ok();
        let state = observation
            .as_ref()
            .map_or(RecoveryCleanupState::Unresolved, cleanup_state);
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
        if members.is_empty() {
            return Err(error("no local paused recovery claim"));
        }
        Ok(RecoveryCleanupHandle {
            runtime: Arc::downgrade(&self.shared),
            claim_id: claim_id.into(),
            state,
            observation,
            members,
        })
    }

    /// Cleanup changes no durable state, never opens a gate, and never treats
    /// Activated as Prepared. Unresolved retains its quarantine index and errors.
    pub fn cleanup_recovery_claim(
        &self,
        handle: &RecoveryCleanupHandle,
    ) -> Result<(), RuntimeError> {
        if !Weak::ptr_eq(&handle.runtime, &Arc::downgrade(&self.shared)) {
            return Err(error("foreign recovery cleanup handle"));
        }
        let gates = handle
            .members
            .iter()
            .map(|(id, _, _)| self.stop_restore_gate(*id))
            .collect::<Vec<_>>();
        let _stop = gates
            .iter()
            .map(|g| g.lock().unwrap_or_else(std::sync::PoisonError::into_inner))
            .collect::<Vec<_>>();
        let _publication = handle
            .members
            .iter()
            .filter_map(|(_, _, e)| e.as_ref().map(|e| e.publication_gate()))
            .collect::<Vec<_>>();
        let mut indexes = self
            .shared
            .spawn_intents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let admissions = self
            .shared
            .admissions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let runs = lock_runs(&self.shared.runs);
        for (id, old, execution) in &handle.members {
            let index = indexes
                .get(id)
                .ok_or_else(|| error("cleanup index missing"))?;
            if index
                .paused_recovery
                .as_ref()
                .is_none_or(|p| p.claim_id != handle.claim_id || p.old_incarnation != *old)
                || match (&index.execution, execution) {
                    (Some(a), Some(b)) => !Arc::ptr_eq(a, b),
                    (None, None) => false,
                    _ => true,
                }
                || admissions.contains_key(id)
                || runs.contains_key(id)
            {
                return Err(error("cleanup member was replaced or published"));
            }
        }
        let store = self
            .shared
            .run_store
            .get()
            .ok_or_else(|| error("no run store"))?;
        let current = store
            .with_database(|db| db.recovery_claim_snapshot(&handle.claim_id))
            .ok();
        if handle.state == RecoveryCleanupState::Unresolved
            || current != handle.observation
            || current
                .as_ref()
                .is_none_or(|s| cleanup_state(s) != handle.state)
        {
            // No authority is inferred from this read. In particular no Stop/CAS
            // is issued against a successor incarnation or an old OwnerPermit.
            for (_, _, execution) in &handle.members {
                if let Some(execution) = execution {
                    execution.stop_admission();
                }
            }
            return Err(error("recovery cleanup unresolved; quarantine retained"));
        }
        let snapshot = current.as_ref().expect("validated observation");
        let expected = snapshot
            .manifest
            .members
            .iter()
            .map(|m| m.run_id.as_str())
            .collect::<BTreeSet<_>>();
        if handle
            .members
            .iter()
            .map(|(id, _, _)| id.to_string())
            .collect::<BTreeSet<_>>()
            != expected
                .iter()
                .map(|id| (*id).to_string())
                .collect::<BTreeSet<_>>()
        {
            return Err(error("cleanup membership changed"));
        }
        let mut intents = Vec::new();
        for (id, _, execution) in &handle.members {
            let row = snapshot
                .intents
                .iter()
                .find(|o| o.run_id == id.to_string())
                .and_then(|o| o.intent.clone())
                .ok_or_else(|| error("cleanup intent missing"))?;
            let intent = ResumeIntent::decode(row).map_err(error)?;
            if let Some(execution) = execution
                && handle.state == RecoveryCleanupState::ActivatedUnpublished
                && execution.publication().incarnation() != intent.incarnation
            {
                return Err(error("cleanup execution incarnation changed"));
            }
            intents.push(intent);
        }
        // Coordinator adopts the durable new identities as already persisted;
        // do not let shutdown save the old in-memory intent over the new row.
        self.shared.quiesce.adopt(intents.clone(), true);
        for intent in &intents {
            let id = intent.run_id;
            if let Some(index) = indexes.get_mut(&id)
                && let Some(paused) = &mut index.paused_recovery
            {
                paused.old_incarnation.clone_from(&intent.incarnation);
            }
            if let Some(execution) = &indexes[&id].execution {
                execution.stop_admission();
            }
            self.shared.quiesce.forget_released(id, &intent.incarnation);
            indexes.remove(&id);
        }
        Ok(())
    }
}

pub(super) fn prepared_stoppable(s: &RecoveryClaimSnapshot) -> bool {
    matches!(s.claim.state, RecoveryClaimState::Prepared) && coherent(s, false)
}
fn prepared_unexecuted(s: &RecoveryClaimSnapshot) -> bool {
    prepared_stoppable(s)
        && s.claim.members.iter().all(|m| {
            s.intents
                .iter()
                .find(|o| o.run_id == m.run_id)
                .is_some_and(|o| {
                    o.revision == Some(m.reserved_revision)
                        && o.intent
                            .as_ref()
                            .and_then(|r| ResumeIntent::decode(r.clone()).ok())
                            .is_some_and(|i| i.active && i.stop_reason.is_none())
                })
        })
}
fn cleanup_state(s: &RecoveryClaimSnapshot) -> RecoveryCleanupState {
    match s.claim.state {
        RecoveryClaimState::Activated if coherent(s, true) => {
            RecoveryCleanupState::ActivatedUnpublished
        }
        RecoveryClaimState::Released { .. } if coherent(s, false) => RecoveryCleanupState::Released,
        _ => RecoveryCleanupState::Unresolved,
    }
}
// Readback is diagnostic: reject missing, duplicate, mixed, stopped-new and
// third-incarnation rows. No capability is constructed from this predicate.
fn coherent(s: &RecoveryClaimSnapshot, activated: bool) -> bool {
    let manifest = &s.manifest;
    let claimed = s
        .claim
        .members
        .iter()
        .map(|m| m.run_id.as_str())
        .collect::<BTreeSet<_>>();
    let resume = manifest
        .members
        .iter()
        .filter(|m| m.resume)
        .map(|m| m.run_id.as_str())
        .collect::<BTreeSet<_>>();
    if !manifest.committed
        || manifest.superseded != activated
        || manifest.schema_version != 1
        || claimed.is_empty()
        || claimed != resume
        || claimed.len() != s.claim.members.len()
        || manifest.members.len() != s.intents.len()
        || manifest
            .members
            .iter()
            .map(|m| &m.run_id)
            .collect::<BTreeSet<_>>()
            .len()
            != manifest.members.len()
    {
        return false;
    }
    manifest.members.iter().all(|m| {
        let Some(o) = s.intents.iter().find(|o| o.run_id == m.run_id) else {
            return false;
        };
        let Some(row) = o.intent.as_ref() else {
            return false;
        };
        let Ok(intent) = ResumeIntent::decode(row.clone()) else {
            return false;
        };
        if let Some(c) = s.claim.members.iter().find(|c| c.run_id == m.run_id) {
            if c.old_incarnation != m.incarnation
                || c.new_incarnation.is_empty()
                || c.new_incarnation == c.old_incarnation
                || m.intent_revision.checked_add(1) != Some(c.reserved_revision)
            {
                return false;
            }
            if activated {
                intent.incarnation == c.new_incarnation
                    && o.revision == c.reserved_revision.checked_add(1)
                    && intent.active
                    && intent.stop_reason.is_none()
            } else {
                intent.incarnation == c.old_incarnation
                    && (o.revision == Some(c.reserved_revision)
                        || (matches!(
                            s.claim.state,
                            RecoveryClaimState::Prepared | RecoveryClaimState::Released { .. }
                        ) && !intent.active
                            && matches!(
                                intent.stop_reason,
                                Some(
                                    crate::quiesce::StopReason::OperatorStopped
                                        | crate::quiesce::StopReason::Cancelled
                                )
                            )
                            && o.revision.is_some_and(|r| r > c.reserved_revision)))
            }
        } else {
            intent.incarnation == m.incarnation && o.revision == Some(m.intent_revision)
        }
    })
}
