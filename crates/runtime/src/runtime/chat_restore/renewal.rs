//! Current callers may reuse stopped root history; disk delivery cannot renew authority.

use super::*;

impl AgentRuntime {
    pub(super) fn validate_history_restore(
        &self,
        record: &storage::RunContextRecord,
        descriptor: &RunRestoreDescriptor,
        authority: &RunConfig,
    ) -> Result<(), RuntimeError> {
        let fail = |reason: String| RuntimeError::RunRestoreFailed {
            run_id: record.run_id.clone(),
            reason: RunRestoreFailure::UnsupportedConfig(reason),
        };
        if let Some(permit) = &authority.ownership {
            permit
                .validate_generation()
                .map_err(|_| RuntimeError::StaleOwnership {
                    run_id: record.run_id.clone(),
                })?;
        }
        if descriptor.has_uncertain_effects() {
            return Err(fail(
                "unresolved_tool_calls: inspect actual effects before starting a new run".into(),
            ));
        }
        if (record.restorable && descriptor.restorable)
            || descriptor.renewable_ownership_only()
            || descriptor.renewable_root_context()
        {
            self.validate_stopped_history_tree(record, "root")?;
            return Ok(());
        }
        if !descriptor.renewable_team_root() {
            return Err(fail(
                descriptor
                    .non_restorable_reason
                    .clone()
                    .unwrap_or_else(|| "実行設定".into()),
            ));
        }
        let team = descriptor
            .renewable_team
            .as_ref()
            .expect("validated team identity");
        if team.coordinator_run_id.to_string() != record.run_id
            || record.parent_run_id.is_some()
            || record.role != agents::Role::Orchestrator.name()
        {
            return Err(fail("team coordinator identity mismatch".into()));
        }
        let store = authority.team_store.as_ref().ok_or_else(|| {
            fail("current_team_authority_required: provide the current durable team store".into())
        })?;
        if store.id != team.team_id
            || authority.topology.worker_limit().is_none()
            || authority
                .delegation_value
                .as_deref()
                .is_none_or(|value| value.trim().is_empty())
            || authority.team_task.is_some()
        {
            return Err(fail("current_team_authority_required: team identity, topology and delegation value must be explicitly renewed".into()));
        }
        self.validate_stopped_history_tree(record, "team")?;
        // Read only from caller-granted storage. Expired claims are also blocked:
        // expiry alone cannot prove a prior process has stopped applying effects.
        let current = crate::team_context::TeamContext::persistent(
            team.coordinator_run_id,
            store,
            authority
                .topology
                .worker_limit()
                .expect("validated team topology"),
        )
        .map_err(|error| fail(format!("team storage validation failed: {error}")))?;
        if current
            .board
            .snapshot()
            .map_err(|error| fail(error.to_string()))?
            .iter()
            .any(|task| matches!(task.state, crate::team::ClaimState::Claimed(_)))
        {
            return Err(fail("team_reconciliation_required: persisted task claims must be reconciled before continuing".into()));
        }
        Ok(())
    }

    fn validate_stopped_history_tree(
        &self,
        record: &storage::RunContextRecord,
        scope: &str,
    ) -> Result<(), RuntimeError> {
        let root = crate::meta::parse_run_id(&record.run_id).map_err(|reason| {
            RuntimeError::RunRestoreFailed {
                run_id: record.run_id.clone(),
                reason: RunRestoreFailure::CorruptContext(reason),
            }
        })?;
        // Fence pending admission and registered runs together. Admission holds
        // this lock while registering, so a child cannot disappear between the
        // two views. Match that lock order: admissions, then runs.
        {
            let admissions = self
                .shared
                .admissions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let runs = lock_runs(&self.shared.runs);
            let active = runs.iter().filter_map(|(id, entry)| {
                matches!(
                    *entry.phase_rx.borrow(),
                    AgentRunPhase::Pending | AgentRunPhase::Running | AgentRunPhase::Waiting
                )
                .then_some(*id)
            });
            let pending = admissions
                .iter()
                .filter_map(|(id, admission)| admission.snapshot().result.is_none().then_some(*id));
            for id in active.chain(pending) {
                let mut cursor = Some(id);
                let mut visited = std::collections::HashSet::new();
                while let Some(candidate) = cursor {
                    if !visited.insert(candidate) {
                        break;
                    }
                    if candidate == root {
                        return Err(RuntimeError::RunRestoreFailed {
                            run_id: record.run_id.clone(),
                            reason: RunRestoreFailure::UnsupportedConfig(format!(
                                "{scope}_reconciliation_required: the previous root or a descendant is still running or awaiting admission"
                            )),
                        });
                    }
                    cursor = runs.get(&candidate).and_then(|run| run.parent).or_else(|| {
                        admissions
                            .get(&candidate)
                            .and_then(|admission| admission.snapshot().parent)
                    });
                }
            }
        }
        Ok(())
    }
}
