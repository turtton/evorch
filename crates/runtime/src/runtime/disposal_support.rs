//! Integration seam for the Q2 leaf. No execution authority or owner claim path.
use super::*;
use std::collections::BTreeSet;
use storage::recovery::graph_control::{
    GraphControlAction, GraphControlRecord, GraphControlWrite, GraphEffectKey,
};

impl AgentRuntime {
    /// Logical revocation precedes every physical mutation.
    /// Lock order: Stop -> publication -> index -> main writer.
    pub fn release_owned_graph_eligibility(
        &self,
        authority: &DisposalAuthority,
        effect: GraphEffectKey,
    ) -> Result<GraphControlRecord, RuntimeError> {
        #[cfg(test)]
        self.record_graph_effect(1);
        self.validate_disposal_runtime(authority)
            .map_err(|_| RuntimeError::GraphControlConflict)?;
        let ids = authority
            .expected()
            .manifest
            .members
            .iter()
            .map(|m| {
                m.run_id
                    .parse::<RunId>()
                    .map_err(|_| RuntimeError::GraphControlConflict)
            })
            .collect::<Result<BTreeSet<_>, _>>()?;
        let claim_id = authority
            .expected()
            .claim
            .as_ref()
            .map(|c| c.claim_id.as_str());
        let gates = ids
            .iter()
            .map(|id| self.stop_restore_gate(*id))
            .collect::<Vec<_>>();
        let _stop = gates
            .iter()
            .map(|g| g.lock().unwrap_or_else(std::sync::PoisonError::into_inner))
            .collect::<Vec<_>>();
        let executions = {
            let indexes = self
                .shared
                .spawn_intents
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            ids.iter()
                .filter_map(|id| {
                    indexes
                        .get(id)
                        .and_then(|i| i.execution.as_ref())
                        .map(|e| (*id, e.clone()))
                })
                .collect::<Vec<_>>()
        };
        let _publication = executions
            .iter()
            .map(|(_, e)| e.publication_gate())
            .collect::<Vec<_>>();
        let mut indexes = self
            .shared
            .spawn_intents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for id in &ids {
            if let Some(index) = indexes.get(id) {
                let captured = authority
                    .expected()
                    .manifest
                    .members
                    .iter()
                    .find(|m| m.run_id == id.to_string())
                    .ok_or(RuntimeError::GraphControlConflict)?;
                if index.paused_recovery.as_ref().is_none_or(|p| {
                    Some(p.claim_id.as_str()) != claim_id
                        || p.old_incarnation != captured.incarnation
                }) || match (
                    index.execution.as_ref(),
                    executions.iter().find(|(run, _)| run == id),
                ) {
                    (Some(a), Some((_, b))) => !Arc::ptr_eq(a, b),
                    (None, None) => false,
                    _ => true,
                } {
                    return Err(RuntimeError::GraphControlConflict);
                }
            }
        }
        let store = self
            .shared
            .run_store
            .get()
            .ok_or(RuntimeError::GraphControlNotApplied)?;
        let request = GraphControlWrite {
            expected: authority.expected().clone(),
            effect: effect.clone(),
            action: if matches!(
                authority.expected().phase,
                storage::recovery::restart::RestartPhase::Aborted
                    | storage::recovery::restart::RestartPhase::Invalidated
                    | storage::recovery::restart::RestartPhase::Superseded
                    | storage::recovery::restart::RestartPhase::Completed
            ) || authority.expected().claim.as_ref().is_some_and(|claim| {
                matches!(
                    claim.state,
                    storage::recovery::RecoveryClaimState::Released { .. }
                )
            }) {
                GraphControlAction::TerminalCleanup
            } else {
                GraphControlAction::Release
            },
        };
        let result = graph_control_write_result(
            store.handle.write_recovery_graph_control(request),
            || store.with_database(|db| db.recovery_graph_effect(&effect.operation_id)),
            |r| {
                r.revoked
                    && r.effect == effect
                    && r.manifest_id == authority.expected().manifest.manifest_id
            },
        )?;
        for id in &ids {
            if let Some(index) = indexes.remove(id) {
                if let Some(execution) = index.execution {
                    execution.stop_admission();
                }
                if let Some(paused) = index.paused_recovery {
                    self.shared
                        .quiesce
                        .forget_released(*id, &paused.old_incarnation);
                }
            }
        }
        Ok(result)
    }
    pub(super) fn validate_disposal_runtime(
        &self,
        authority: &DisposalAuthority,
    ) -> Result<(), RuntimeError> {
        if !Weak::ptr_eq(authority.runtime(), &Arc::downgrade(&self.shared)) {
            return Err(error("foreign disposal authority"));
        }
        Ok(())
    }
}
fn error(reason: impl ToString) -> RuntimeError {
    crate::quiesce::runtime::error(reason)
}

// One operation-independent tuple in both durable progress and scratch memory.
impl TryFrom<&storage::recovery::graph_progress::GraphDisposalProgressKey>
    for crate::scratch::GraphDisposalKey
{
    type Error = std::io::Error;
    fn try_from(
        key: &storage::recovery::graph_progress::GraphDisposalProgressKey,
    ) -> Result<Self, Self::Error> {
        Self::new(
            key.manifest_id.clone(),
            key.restart_id.clone(),
            key.capture_id.clone(),
            key.thread_id.clone(),
        )
    }
}

/// Only an ACKed refusal plus a readable absent/nonmatching receipt proves
/// non-application. Channel closure or unreadable readback remains unknown.
pub(super) fn graph_control_write_result(
    result: Result<GraphControlRecord, storage::StorageError>,
    read: impl FnOnce() -> Result<Option<GraphControlRecord>, storage::StorageError>,
    matches: impl FnOnce(&GraphControlRecord) -> bool,
) -> Result<GraphControlRecord, RuntimeError> {
    match result {
        Ok(record) => Ok(record),
        Err(write_error) => {
            let observed = read().map_err(|_| RuntimeError::GraphControlUnknown)?;
            if let Some(record) = observed
                && matches(&record)
            {
                return Ok(record);
            }
            Err(match write_error {
                storage::StorageError::WriterClosed => RuntimeError::GraphControlUnknown,
                storage::StorageError::RecoveryGraphConflict(_) => {
                    RuntimeError::GraphControlConflict
                }
                _ => RuntimeError::GraphControlNotApplied,
            })
        }
    }
}

#[cfg(test)]
mod control_result_tests {
    use super::*;
    #[test]
    fn writer_closed_and_absent_receipt_is_unknown_not_a_rejection() {
        assert_eq!(
            graph_control_write_result(
                Err(storage::StorageError::WriterClosed),
                || Ok(None),
                |_| false
            ),
            Err(RuntimeError::GraphControlUnknown)
        );
    }
    #[test]
    fn unreadable_receipt_is_unknown_even_after_a_reply() {
        assert_eq!(
            graph_control_write_result(
                Err(storage::StorageError::RecoveryGraphConflict(
                    "changed".into()
                )),
                || Err(storage::StorageError::Io("unreadable".into())),
                |_| false
            ),
            Err(RuntimeError::GraphControlUnknown)
        );
    }
    #[test]
    fn acknowledged_refusal_is_classified_without_retrying() {
        assert_eq!(
            graph_control_write_result(
                Err(storage::StorageError::RecoveryGraphConflict(
                    "changed".into()
                )),
                || Ok(None),
                |_| false
            ),
            Err(RuntimeError::GraphControlConflict)
        );
        assert_eq!(
            graph_control_write_result(
                Err(storage::StorageError::Serialization("refused".into())),
                || Ok(None),
                |_| false
            ),
            Err(RuntimeError::GraphControlNotApplied)
        );
    }
}
