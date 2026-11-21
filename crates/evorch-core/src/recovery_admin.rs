//! Existing-Prepared-only manual execution. Disposal never enters this module.
use crate::{
    recovery_service::admin::{RecoveryAdminContext, RecoveryAdminHandler},
    recovery_startup::StartupBlocked,
};
use runtime::{ActivationReservation, OwnedGraphOwnershipState};
use std::{collections::BTreeMap, sync::Mutex};
use storage::recovery::{
    RecoveryClaimState,
    graph_control::{GraphControlRecord, GraphEffectKey},
};
use workbench_host::bootstrap::{
    core_startup::{RecoveryAdminError, RecoveryAdminResult, RecoveryResumeRequest},
    recovery_disposal::binding_for,
};
use workbench_protocol::namespaces::lifecycle::owned_graphs::{
    GraphAuthorityProblem, GraphProblemDetail,
};

struct UncertainPolicy {
    // Original admission token, not reconstructed from a durable control row.
    _reservation: ActivationReservation,
    effect: GraphEffectKey,
    manifest: String,
}
#[derive(Default)]
pub struct ExistingPreparedResume {
    uncertain: Mutex<BTreeMap<String, UncertainPolicy>>,
}
impl ExistingPreparedResume {
    /// Original supervisor readback only: finish a policy whose activation was
    /// NEVER attempted. This must not be invoked by operation queries/replays.
    pub fn reconcile_policy(
        &self,
        storage: &storage::StorageConfig,
        writer: &storage::StorageHandle,
    ) -> Result<usize, RecoveryAdminError> {
        let keys = self
            .uncertain
            .lock()
            .unwrap()
            .iter()
            .map(|(id, job)| (id.clone(), job.effect.clone(), job.manifest.clone()))
            .collect::<Vec<_>>();
        let mut resolved = 0;
        for (id, effect, manifest) in keys {
            let observed =
                storage::Database::read_owned_graphs(storage, |db| db.recovery_graph_effect(&id))
                    .map_err(|_| RecoveryAdminError::Unavailable)?;
            let Some(control) =
                observed.filter(|c| c.effect == effect && c.manifest_id == manifest && !c.revoked)
            else {
                continue;
            };
            if control.mode != "manual_idle"
                && writer.finish_recovery_graph_control(control).is_err()
            {
                continue;
            }
            let settled = self.uncertain.lock().unwrap().remove(&id);
            if let Some(job) = settled {
                job._reservation.confirm_graph_observation();
                resolved += 1;
            }
        }
        Ok(resolved)
    }
    fn retain(&self, manifest: String, effect: GraphEffectKey, reservation: ActivationReservation) {
        // The shared graph admission limit is 64. Unknown entries are never
        // evicted to admit another job.
        self.uncertain.lock().unwrap().insert(
            effect.operation_id.clone(),
            UncertainPolicy {
                _reservation: reservation,
                effect,
                manifest,
            },
        );
    }
    fn policy_write_failed(
        &self,
        manifest: String,
        effect: GraphEffectKey,
        reservation: ActivationReservation,
        error: runtime::RuntimeError,
    ) -> RecoveryAdminError {
        match error {
            runtime::RuntimeError::GraphControlConflict => {
                reservation.confirm_graph_observation();
                RecoveryAdminError::Conflict
            }
            runtime::RuntimeError::GraphControlNotApplied => {
                reservation.confirm_graph_observation();
                RecoveryAdminError::Stopped
            }
            _ => {
                self.retain(manifest, effect, reservation);
                RecoveryAdminError::Unavailable
            }
        }
    }
    fn failed(
        &self,
        context: &RecoveryAdminContext,
        control: GraphControlRecord,
        reservation: ActivationReservation,
        error: RecoveryAdminError,
    ) -> RecoveryAdminResult {
        let result = context
            .startup
            .writer
            .finish_recovery_graph_control(control.clone())
            .or_else(|_| {
                storage::Database::read_owned_graphs(&context.startup.storage, |db| {
                    db.recovery_graph_effect(&control.effect.operation_id)?
                        .filter(|c| {
                            c.effect == control.effect && c.mode == "manual_idle" && !c.revoked
                        })
                        .ok_or_else(|| {
                            storage::StorageError::Io("manual policy result unknown".into())
                        })
                })
            });
        if result.is_err() {
            self.retain(control.manifest_id, control.effect, reservation);
            return Err(RecoveryAdminError::Unavailable);
        }
        // Failed manual Resume deliberately preserves the existing Prepared
        // claim. No decline(), release(), or automatic-policy fallback here.
        reservation.confirm_graph_observation();
        drop(reservation);
        Err(error)
    }
}
impl RecoveryAdminHandler for ExistingPreparedResume {
    fn reconcile(&self, context: &RecoveryAdminContext) {
        if let Err(error) = self.reconcile_policy(&context.startup.storage, &context.startup.writer)
        {
            tracing::warn!(?error, "manual policy readback remains unresolved");
        }
    }
    fn resume(
        &self,
        context: &RecoveryAdminContext,
        request: RecoveryResumeRequest,
    ) -> RecoveryAdminResult {
        let observe = request.reservation.observation_readback();
        let result = self.resume_observed(context, request);
        if !matches!(result, Err(RecoveryAdminError::Unavailable)) {
            let _ = observe();
        }
        result
    }
}
impl ExistingPreparedResume {
    fn resume_observed(
        &self,
        context: &RecoveryAdminContext,
        request: RecoveryResumeRequest,
    ) -> RecoveryAdminResult {
        let runtime = &context.startup.runtime;
        let graph = storage::Database::read_owned_graphs(&context.startup.storage, |db| {
            let mut graphs = db
                .owned_graph_snapshots()?
                .into_iter()
                .filter(|g| g.cas.manifest.manifest_id == request.manifest_id);
            let graph = graphs
                .next()
                .ok_or_else(|| storage::StorageError::Io("missing resume graph".into()))?;
            if graphs.next().is_some() {
                return Err(storage::StorageError::Io("ambiguous resume graph".into()));
            }
            Ok(graph)
        })
        .map_err(|_| RecoveryAdminError::Unavailable)?;
        let (current, owners) = binding_for(
            &context.startup.owner,
            &graph.cas,
            request.binding.graph.operation_id.clone(),
        )
        .map_err(|_| RecoveryAdminError::Unavailable)?;
        if current != request.binding
            || owners.len() > 64
            || request.effect.operation_id != current.graph.operation_id.as_str()
            || graph.cas.phase != storage::recovery::restart::RestartPhase::Adopted
            || !graph
                .cas
                .claim
                .as_ref()
                .is_some_and(|c| c.state == RecoveryClaimState::Prepared)
        {
            return Err(blocked(GraphProblemDetail::NeedsAttention));
        }
        let reservation: ActivationReservation = request.reservation.into();
        if runtime
            .reserved_owned_graph_ownership_diagnostic(&graph.cas, &reservation)
            .map_err(|_| RecoveryAdminError::Unavailable)?
            .state()
            != OwnedGraphOwnershipState::LiveSelf
        {
            return Err(blocked(GraphProblemDetail::OwnerAbsent));
        }
        let permits = owners
            .iter()
            .map(|o| {
                context
                    .startup
                    .owner
                    .owned_permit(&o.thread_id)
                    .map_err(|_| blocked(GraphProblemDetail::ForeignOwner))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let control = match runtime.begin_owned_graph_resume(&graph.cas, &permits, &reservation) {
            Ok(control) => control,
            Err(error) => {
                return Err(self.policy_write_failed(
                    request.manifest_id,
                    request.effect,
                    reservation,
                    error,
                ));
            }
        };
        let coordinator = match context.coordinator() {
            Ok(coordinator) => coordinator,
            Err(reason) => {
                return self.failed(context, control, reservation, map_startup_blocked(reason));
            }
        };
        let prepared = match coordinator.prepare_existing(&request.manifest_id) {
            Ok(prepared) => prepared,
            Err(reason) => {
                return self.failed(context, control, reservation, map_startup_blocked(reason));
            }
        };
        let binding = {
            context
                .host
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .service()
                .begin_root_recovery(prepared.command_bindings())
        };
        let guard = match binding {
            Ok(guard) => guard,
            Err(_) => {
                return self.failed(
                    context,
                    control,
                    reservation,
                    blocked(GraphProblemDetail::CurrentBindingRequired),
                );
            }
        };
        // No host mutex spans FULL activation or provider execution. The
        // non-Clone reservation is held until binding publication finishes.
        match prepared.activate(runtime, reservation) {
            Ok((runs, publication)) => {
                context
                    .host
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .service_mut()
                    .complete_root_recovery(guard);
                publication.release();
                Ok(runs)
            }
            Err(failure) => {
                drop(guard);
                match failure.reservation {
                    Some(reservation) => self.failed(
                        context,
                        control,
                        reservation,
                        blocked(GraphProblemDetail::NeedsAttention),
                    ),
                    None => Err(RecoveryAdminError::Unavailable), // original runtime supervisor owns Unknown
                }
            }
        }
    }
}
fn blocked(detail: GraphProblemDetail) -> RecoveryAdminError {
    RecoveryAdminError::Blocked {
        problem: GraphAuthorityProblem::CurrentBindingRequired,
        detail,
    }
}
pub fn map_startup_blocked(reason: StartupBlocked) -> RecoveryAdminError {
    use GraphAuthorityProblem as P;
    use GraphProblemDetail as D;
    let (problem, detail) = match reason {
        StartupBlocked::OwnerBlocked => (P::OwnerBlocked, D::OwnerAbsent),
        StartupBlocked::CurrentBindingRequired => {
            (P::CurrentBindingRequired, D::CurrentBindingRequired)
        }
        StartupBlocked::CurrentSettingsRequired => {
            (P::CurrentSettingsRequired, D::CurrentSettingsRequired)
        }
        StartupBlocked::CliContinuityRequired => {
            (P::CliContinuityRequired, D::CliContinuityRequired)
        }
        StartupBlocked::TeamReconciliationRequired => {
            (P::TeamReconciliationRequired, D::TeamReconciliationRequired)
        }
        StartupBlocked::StorageUnavailable => (P::StorageUnavailable, D::StorageUnavailable),
        StartupBlocked::CatalogUnavailable => (P::CurrentSettingsRequired, D::CatalogUnavailable),
        StartupBlocked::AlreadyActivated => (P::CurrentBindingRequired, D::AlreadyActivated),
        StartupBlocked::Released => (P::CurrentBindingRequired, D::Released),
        StartupBlocked::NeedsAttention => (P::CurrentBindingRequired, D::NeedsAttention),
    };
    RecoveryAdminError::Blocked { problem, detail }
}

#[cfg(test)]
mod policy_reservation_tests {
    use super::*;
    use std::sync::Arc;
    struct Never;
    #[async_trait::async_trait]
    impl runtime::AgentModel for Never {
        fn selected_model(&self, _: runtime::Role, _: Option<&str>) -> String {
            "never".into()
        }
        async fn complete(
            &self,
            _: &runtime::AgentInvocationContext,
            _: runtime::Role,
            _: &[providers::Message],
            _: &[providers::ToolSpec],
        ) -> Result<providers::ChatResponse, runtime::RuntimeError> {
            panic!("policy refusal cannot request provider")
        }
    }
    fn check(error: runtime::RuntimeError, expected: RecoveryAdminError, retained: bool) {
        let bus = Arc::new(event_bus::EventBus::new(16));
        let runtime = runtime::AgentRuntime::new(
            bus.clone(),
            Arc::new(::tools::ToolExecutor::new(bus)),
            Arc::new(Never),
        );
        let handler = ExistingPreparedResume::default();
        let effect = GraphEffectKey {
            operation_id: "first".into(),
            attempt_nonce: "nonce".into(),
        };
        let reservation = runtime
            .reserve_manual_activation("graph", effect.clone(), 0)
            .unwrap()
            .into();
        assert_eq!(
            handler.policy_write_failed("graph".into(), effect, reservation, error),
            expected
        );
        assert_eq!(
            handler.uncertain.lock().unwrap().len(),
            usize::from(retained)
        );
        let next = runtime.reserve_manual_activation(
            "graph",
            GraphEffectKey {
                operation_id: "next".into(),
                attempt_nonce: "next".into(),
            },
            0,
        );
        assert_eq!(next.is_ok(), !retained);
        assert!(runtime.list_agents().is_empty());
    }
    #[test]
    fn resume_conflict_releases_original_reservation() {
        check(
            runtime::RuntimeError::GraphControlConflict,
            RecoveryAdminError::Conflict,
            false,
        );
    }
    #[test]
    fn resume_confirmed_not_applied_releases_original_reservation() {
        check(
            runtime::RuntimeError::GraphControlNotApplied,
            RecoveryAdminError::Stopped,
            false,
        );
    }
    #[test]
    fn resume_unknown_keeps_original_reservation() {
        check(
            runtime::RuntimeError::GraphControlUnknown,
            RecoveryAdminError::Unavailable,
            true,
        );
    }
}
