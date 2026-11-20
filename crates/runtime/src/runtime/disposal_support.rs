//! Integration seam for the Q2 leaf. No execution authority or owner claim path.
use super::*;
use std::collections::BTreeSet;
use storage::recovery::graph_control::{
    GraphControlAction, GraphControlRecord, GraphControlWrite, GraphEffectKey,
};

impl AgentRuntime {
    /// The owner snapshot remains reserved until FULL commit/readback finishes.
    /// Lock order: Stop -> publication -> index -> owners -> main writer.
    pub fn release_owned_graph_eligibility(
        &self,
        authority: &DisposalAuthority,
        effect: GraphEffectKey,
    ) -> Result<GraphControlRecord, RuntimeError> {
        #[cfg(test)]
        self.record_graph_effect(1);
        self.validate_disposal_runtime(authority)?;
        let ids = authority
            .expected()
            .manifest
            .members
            .iter()
            .map(|m| m.run_id.parse::<RunId>().map_err(error))
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
                    .ok_or_else(|| error("missing disposal member"))?;
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
                    return Err(error("disposal index is not the captured paused execution"));
                }
            }
        }
        let _owners = crate::ownership::disposal_support::guard(authority).map_err(error)?;
        let store = self
            .shared
            .run_store
            .get()
            .ok_or_else(|| error("no run store"))?;
        let request = GraphControlWrite {
            expected: authority.expected().clone(),
            effect: effect.clone(),
            action: GraphControlAction::Release,
        };
        let result = match store.handle.write_recovery_graph_control(request) {
            Ok(result) => result,
            Err(write_error) => store
                .with_database(|db| db.recovery_graph_effect(&effect.operation_id))
                .map_err(error)?
                .filter(|r| {
                    r.revoked
                        && r.effect == effect
                        && r.manifest_id == authority.expected().manifest.manifest_id
                })
                .ok_or_else(|| error(write_error))?,
        };
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
    pub fn cleanup_owned_graph_receipts(
        &self,
        authority: &DisposalAuthority,
        effect: GraphEffectKey,
        shared_threads: &BTreeSet<String>,
    ) -> Result<crate::ownership::DisposalOwnerReceipt, RuntimeError> {
        #[cfg(test)]
        self.record_graph_effect(2);
        self.validate_disposal_runtime(authority)?;
        let store = self
            .shared
            .run_store
            .get()
            .ok_or_else(|| error("no run store"))?;
        if !store
            .with_database(|db| {
                db.recovery_graph_control(&authority.expected().manifest.manifest_id)
            })
            .map_err(error)?
            .is_some_and(|c| c.revoked)
        {
            return Err(error("eligibility revocation not confirmed"));
        }
        crate::ownership::disposal_support::cleanup(authority, effect, shared_threads)
            .map_err(error)
    }
    fn validate_disposal_runtime(&self, authority: &DisposalAuthority) -> Result<(), RuntimeError> {
        if !Weak::ptr_eq(authority.runtime(), &Arc::downgrade(&self.shared)) {
            return Err(error("foreign disposal authority"));
        }
        Ok(())
    }
}
fn error(reason: impl ToString) -> RuntimeError {
    crate::quiesce::runtime::error(reason)
}

// One operation-independent tuple in both the durable journal and scratch memory.
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
            key.owner_generation,
        )
    }
}
