use super::view;
use crate::{
    admin::{self, *},
    bootstrap::core_startup::{
        CoreStartupDiagnostics, RecoveryAdminError, RecoveryAdminFuture, RecoveryAdminPort,
        RecoveryResumeRequest,
    },
};
use runtime::{
    AgentRuntime, OwnedThreadCas,
    ownership::OwnerHost,
    runtime::recovery_disposal::{DisposalReservation, DisposalResult},
};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};
use storage::{
    StorageConfig,
    recovery::{graph_control::GraphEffectKey, owned_graphs::OwnedGraphSnapshot},
};
use workbench_protocol::{
    errors::ErrorCode,
    namespaces::lifecycle::{
        AdminId, AdminOperationState, AdminOutcome, AdminReceipt, owned_graphs::*,
    },
};

/// Forward only the host-owned port; workbench-host never imports the core.
pub struct DiagnosticsPort(pub Arc<dyn CoreStartupDiagnostics>);
impl RecoveryAdminPort for DiagnosticsPort {
    fn resume(
        &self,
        request: RecoveryResumeRequest,
    ) -> Result<RecoveryAdminFuture, RecoveryAdminError> {
        self.0.recovery_admin().resume(request)
    }
    fn retry_automatic(
        &self,
        manifest_id: String,
    ) -> Result<RecoveryAdminFuture, RecoveryAdminError> {
        self.0.recovery_admin().retry_automatic(manifest_id)
    }
}
struct Inspection {
    binding: OwnedGraphBinding,
    graph: OwnedGraphSnapshot,
    owners: Vec<OwnedThreadCas>,
    connection: VerifiedConnection,
}
struct UncertainDisposal {
    effect: GraphEffectKey,
    reservation: DisposalReservation,
    finished: std::sync::atomic::AtomicBool,
}
struct FinishedDisposal(Arc<UncertainDisposal>);
impl Drop for FinishedDisposal {
    fn drop(&mut self) {
        self.0
            .finished
            .store(true, std::sync::atomic::Ordering::Release);
    }
}
pub struct OwnedGraphsFacade {
    runtime: AgentRuntime,
    owner: Arc<OwnerHost>,
    storage: StorageConfig,
    reader: Arc<AdminReader>,
    port: Arc<dyn RecoveryAdminPort>,
    diagnostics: Arc<dyn CoreStartupDiagnostics>,
    inspections: Mutex<BTreeMap<String, Inspection>>,
    uncertain: Arc<Mutex<BTreeMap<String, Arc<UncertainDisposal>>>>,
    snapshots: Arc<Mutex<super::snapshot::SnapshotClock>>,
}
impl OwnedGraphsFacade {
    pub fn new(
        runtime: AgentRuntime,
        owner: Arc<OwnerHost>,
        storage: StorageConfig,
        reader: Arc<AdminReader>,
        diagnostics: Arc<dyn CoreStartupDiagnostics>,
    ) -> Arc<Self> {
        Arc::new(Self {
            runtime,
            owner,
            storage,
            reader,
            port: Arc::new(DiagnosticsPort(diagnostics.clone())),
            diagnostics,
            inspections: Mutex::default(),
            uncertain: Arc::default(),
            snapshots: Arc::default(),
        })
    }
    /// Original supervisor ONLY, on its blocking worker. A finished original
    /// job may settle its policy; no new owner or scratch effect starts here.
    pub fn reconcile_disposals(&self) -> Result<usize, AdminFailure> {
        let pending = self
            .uncertain
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, job)| job.finished.load(std::sync::atomic::Ordering::Acquire))
            .map(|(id, job)| (id.clone(), job.clone()))
            .collect::<Vec<_>>();
        let mut resolved = 0;
        for (id, job) in pending {
            if self
                .runtime
                .reconcile_owned_graph_disposal(&job.effect, &job.reservation)
                .map_err(|_| unavailable())?
            {
                let mut state = self.uncertain.lock().unwrap();
                if state
                    .get(&id)
                    .is_some_and(|current| Arc::ptr_eq(current, &job))
                {
                    state.remove(&id);
                    resolved += 1;
                }
            }
        }
        Ok(resolved)
    }
}
impl AdminFacade for OwnedGraphsFacade {
    fn inspect<'a>(
        &'a self,
        intent: &'a AdminIntent,
        connection: &'a VerifiedConnection,
    ) -> AdminIo<'a, AdminInspection> {
        Box::pin(async move {
            if !connection.matches(connection.epoch()) {
                return Err(admin::conflict());
            }
            let (confirmed, resume) = match intent {
                AdminIntent::OwnedGraphs(OwnedGraphsCommand::Release(c)) => (c, false),
                AdminIntent::OwnedGraphs(OwnedGraphsCommand::Resume(c)) => (c, true),
                _ => return Err(admin::conflict()),
            };
            confirmed.validate().map_err(|_| admin::conflict())?;
            let binding = confirmed.binding.clone();
            let storage = self.storage.clone();
            let owner = self.owner.clone();
            let runtime = self.runtime.clone();
            let verified = connection.clone();
            let inspection = self
                .reader
                .read(move || {
                    let graphs = storage::Database::read_owned_graphs(&storage, |db| {
                        db.owned_graph_snapshots()
                    })
                    .map_err(|_| unavailable())?;
                    let mut selected = None;
                    for graph in graphs {
                        let (current, owners) = view::binding_for(
                            &owner,
                            &graph.cas,
                            binding.graph.operation_id.clone(),
                        )?;
                        if current.graph.graph_id != binding.graph.graph_id {
                            continue;
                        }
                        if selected.is_some() || current != binding || owners.len() > 64 {
                            return Err(admin::conflict());
                        }
                        let detail = view::detail(&runtime, &owner, &graph)?;
                        let action = if resume {
                            detail.resume
                        } else {
                            detail.release
                        };
                        if !matches!(action, ActionAvailability::Available) {
                            return Err(admin::conflict());
                        }
                        selected = Some(Inspection {
                            binding: current,
                            graph,
                            owners,
                            connection: verified.clone(),
                        });
                    }
                    selected.ok_or_else(admin::conflict)
                })
                .await?;
            let expected = inspection.binding.clone();
            let mut inspections = self.inspections.lock().unwrap();
            if inspections.len() >= 64
                && !inspections.contains_key(expected.graph.operation_id.as_str())
            {
                // Inspection is a disposable read cache, never a reservation
                // or executable authority. A stale confirmation fails closed.
                inspections.pop_first();
            }
            inspections.insert(expected.graph.operation_id.as_str().to_owned(), inspection);
            Ok(AdminInspection::OwnedGraph(expected))
        })
    }
    fn query<'a>(
        &'a self,
        query: &'a AdminQuery,
        connection: &'a VerifiedConnection,
    ) -> AdminIo<'a, AdminView> {
        Box::pin(async move {
            if !connection.matches(connection.epoch()) {
                return Err(admin::conflict());
            }
            let principal = connection.principal().clone();
            let AdminQuery::OwnedGraphs(query) = query else {
                return Err(admin::conflict());
            };
            let query = query.clone();
            let storage = self.storage.clone();
            let owner = self.owner.clone();
            let runtime = self.runtime.clone();
            let snapshots = self.snapshots.clone();
            let diagnostics = self.diagnostics.clone();
            self.reader
                .read(move || {
                    let graphs = storage::Database::read_owned_graphs(&storage, |db| {
                        db.owned_graph_snapshots()
                    })
                    .map_err(|_| unavailable())?;
                    let reply = match query {
                        OwnedGraphsQuery::Undisposed => {
                            let mut snapshot = view::collect_undisposed(&runtime, &owner, &graphs)?;
                            snapshot.snapshot_revision = workbench_protocol::wire::Revision(0);
                            let fingerprint =
                                serde_json::to_string(&snapshot).map_err(|_| unavailable())?;
                            snapshot.snapshot_revision = snapshots
                                .lock()
                                .unwrap()
                                .observe("undisposed", fingerprint)?;
                            OwnedGraphsReply::Undisposed(snapshot)
                        }
                        OwnedGraphsQuery::Detail { graph_ref } => {
                            let mut selected = None;
                            for graph in &graphs {
                                let (binding, _) =
                                    view::binding_for(&owner, &graph.cas, id("unassigned")?)?;
                                if binding.graph.graph_id == graph_ref {
                                    if selected.is_some() {
                                        return Err(admin::conflict());
                                    }
                                    selected = Some(view::detail(&runtime, &owner, graph)?);
                                }
                            }
                            let mut detail = selected.ok_or_else(not_found)?;
                            detail.snapshot_revision = workbench_protocol::wire::Revision(0);
                            let fingerprint =
                                serde_json::to_string(&detail).map_err(|_| unavailable())?;
                            detail.snapshot_revision = snapshots
                                .lock()
                                .unwrap()
                                .observe(graph_ref.as_str(), fingerprint)?;
                            OwnedGraphsReply::Detail(Box::new(detail))
                        }
                        OwnedGraphsQuery::Operation { operation_id } => {
                            let row = storage
                                .operation(operation_id.as_str())
                                .map_err(|_| unavailable())?
                                .ok_or_else(not_found)?;
                            if row.accepted.principal.method != principal.method()
                                || row.accepted.principal.subject != principal.subject()
                            {
                                return Err(not_found());
                            }
                            if !matches!(
                                row.accepted.operation,
                                storage::admin_audit::AuditOperation::Release
                                    | storage::admin_audit::AuditOperation::Resume
                            ) {
                                return Err(not_found());
                            }
                            let effect = storage::Database::read_owned_graphs(&storage, |db| {
                                db.recovery_graph_effect(operation_id.as_str())
                            })
                            .map_err(|_| unavailable())?;
                            // No effect row does not prove non-application: an
                            // accepted writer may still be pending. Never retry it.
                            let effect = effect.ok_or_else(admin::unknown)?;
                            if effect.effect.attempt_nonce != row.accepted.key.attempt_nonce {
                                return Err(admin::conflict());
                            }
                            let graph = graphs
                                .iter()
                                .find(|g| g.cas.manifest.manifest_id == effect.manifest_id)
                                .ok_or_else(admin::unknown)?;
                            let (binding, _) =
                                view::binding_for(&owner, &graph.cas, operation_id.clone())?;
                            let progress = view::progress(graph);
                            let succeeded = progress.completion == DisposalCompletion::Complete
                                || (!effect.revoked
                                    && graph.cas.claim.as_ref().is_some_and(|c| {
                                        c.state == storage::recovery::RecoveryClaimState::Activated
                                    }));
                            let next_action = operation_next_action(
                                &view::detail(&runtime, &owner, graph)?,
                                effect.mode.as_str(),
                                succeeded,
                                diagnostics.automatic_retry_pending(&effect.manifest_id),
                            );
                            let outcome = match row.outcome {
                                Some(storage::admin_audit::AuditOutcome::Succeeded) => {
                                    AdminOutcome::Succeeded
                                }
                                Some(storage::admin_audit::AuditOutcome::Conflict) => {
                                    AdminOutcome::Conflict
                                }
                                Some(storage::admin_audit::AuditOutcome::Failed) => {
                                    AdminOutcome::Failed
                                }
                                Some(storage::admin_audit::AuditOutcome::Cancelled) => {
                                    AdminOutcome::Cancelled
                                }
                                _ => AdminOutcome::Unknown,
                            };
                            OwnedGraphsReply::Operation(GraphOperationView {
                                receipt: AdminReceipt {
                                    operation_id,
                                    state: if row.state
                                        == storage::admin_audit::AuditState::Finished
                                    {
                                        AdminOperationState::Completed
                                    } else {
                                        AdminOperationState::Unknown
                                    },
                                    outcome,
                                    audit_warning: row.state
                                        != storage::admin_audit::AuditState::Finished,
                                },
                                graph_ref: binding.graph.graph_id,
                                step: if effect.revoked {
                                    EffectStep::Scratch
                                } else {
                                    EffectStep::Activation
                                },
                                observation: if succeeded {
                                    StepObservation::ConfirmedApplied
                                } else {
                                    StepObservation::Unknown
                                },
                                progress,
                                next_action,
                            })
                        }
                    };
                    reply.validate().map_err(|_| unavailable())?;
                    Ok(AdminView::OwnedGraphs(reply))
                })
                .await
        })
    }
    fn commit(
        &self,
        intent: AdminIntent,
        expected: AdminInspection,
        admission: CommitAdmission,
        connection: &VerifiedConnection,
    ) -> Result<AdminAccepted, AdminFailure> {
        if !connection.matches(admission.connection().epoch()) {
            return Err(admin::conflict());
        }
        if !expected.matches(&intent) {
            return Err(admin::conflict());
        }
        let AdminInspection::OwnedGraph(binding) = expected else {
            return Err(admin::conflict());
        };
        let inspection = self
            .inspections
            .lock()
            .unwrap()
            .remove(binding.graph.operation_id.as_str())
            .ok_or_else(admin::conflict)?;
        if inspection.binding != binding || !connection.matches(inspection.connection.epoch()) {
            return Err(admin::conflict());
        }
        let audit = admission.restart_audit();
        let key = audit.operation_key();
        let effect = GraphEffectKey {
            operation_id: key.operation_id.clone(),
            attempt_nonce: key.attempt_nonce.clone(),
        };
        if effect.operation_id != binding.graph.operation_id.as_str() {
            return Err(admin::conflict());
        }
        match intent {
            AdminIntent::OwnedGraphs(OwnedGraphsCommand::Release(_)) => {
                let reservation = self
                    .runtime
                    .reserve_owned_graph_disposal(
                        &inspection.graph.cas.manifest.manifest_id,
                        effect.clone(),
                        inspection.graph.cas.control_revision,
                    )
                    .map_err(|_| admin::busy())?;
                let runtime = self.runtime.clone();
                let uncertain = self.uncertain.clone();
                Ok(admission.register(Box::pin(async move {
                    let job = Arc::new(UncertainDisposal {
                        effect: effect.clone(),
                        reservation,
                        finished: std::sync::atomic::AtomicBool::new(false),
                    });
                    uncertain
                        .lock()
                        .unwrap()
                        .insert(effect.operation_id.clone(), job.clone());
                    let worker_job = job.clone();
                    let result = tokio::task::spawn_blocking(move || {
                        let _finished = FinishedDisposal(worker_job.clone());
                        let authority = match runtime
                            .disposal_authority(inspection.graph.cas, inspection.owners)
                        {
                            Ok(authority) => authority,
                            Err(_) => return Err(runtime::RuntimeError::GraphControlConflict),
                        };
                        runtime.dispose_owned_graph(
                            &authority,
                            worker_job.effect.clone(),
                            &worker_job.reservation,
                        )
                    })
                    .await
                    .unwrap_or(Err(runtime::RuntimeError::GraphControlUnknown));
                    let outcome = finish_disposal(&uncertain, job, result);
                    Ok(AdminExecutionOutcome::Complete(outcome))
                })))
            }
            AdminIntent::OwnedGraphs(OwnedGraphsCommand::Resume(_)) => {
                let reservation = self
                    .runtime
                    .reserve_manual_activation(
                        &inspection.graph.cas.manifest.manifest_id,
                        effect.clone(),
                        inspection.graph.cas.control_revision,
                    )
                    .map_err(|_| admin::busy())?;
                let port = self.port.clone();
                Ok(admission.register(Box::pin(async move {
                    let result = match port.resume(RecoveryResumeRequest {
                        manifest_id: inspection.graph.cas.manifest.manifest_id,
                        binding,
                        effect,
                        reservation,
                    }) {
                        Ok(result) => result.await,
                        Err(error) => Err(error),
                    };
                    let outcome = match result {
                        Ok(_) => AdminOutcome::Succeeded,
                        Err(RecoveryAdminError::Unavailable) => AdminOutcome::Unknown,
                        Err(RecoveryAdminError::Conflict) => AdminOutcome::Conflict,
                        Err(_) => AdminOutcome::Failed,
                    };
                    Ok(AdminExecutionOutcome::Complete(outcome))
                })))
            }
            _ => Err(admin::conflict()),
        }
    }
}
fn finish_disposal(
    uncertain: &Mutex<BTreeMap<String, Arc<UncertainDisposal>>>,
    job: Arc<UncertainDisposal>,
    result: Result<DisposalResult, runtime::RuntimeError>,
) -> AdminOutcome {
    let outcome = match result {
        Ok(result) if !result.unknown => {
            if result.complete() {
                AdminOutcome::Succeeded
            } else {
                AdminOutcome::Failed
            }
        }
        Err(runtime::RuntimeError::GraphControlConflict) => AdminOutcome::Conflict,
        Err(runtime::RuntimeError::GraphControlNotApplied) => AdminOutcome::Failed,
        _ => AdminOutcome::Unknown,
    };
    if outcome != AdminOutcome::Unknown {
        let mut state = uncertain.lock().unwrap();
        if state
            .get(&job.effect.operation_id)
            .is_some_and(|current| Arc::ptr_eq(current, &job))
        {
            state.remove(&job.effect.operation_id);
        }
    }
    outcome
}
fn id(value: &str) -> Result<AdminId, AdminFailure> {
    AdminId::new(value).map_err(|_| unavailable())
}
fn unavailable() -> AdminFailure {
    admin::failure(
        ErrorCode::StorageUnavailable,
        "owned graph observation unavailable",
    )
}
fn not_found() -> AdminFailure {
    admin::failure(ErrorCode::NotFound, "owned graph or operation not found")
}

/// Journal manual_running may outlive activation. A positive durable result is
/// not an unknown ACK and must not direct the user to observe forever.
pub(super) fn operation_next_action(
    graph: &workbench_protocol::namespaces::lifecycle::owned_graphs::OwnedGraphView,
    mode: &str,
    succeeded: bool,
    automatic: bool,
) -> workbench_protocol::namespaces::lifecycle::owned_graphs::NextAction {
    view::next_action(
        graph,
        !succeeded && matches!(mode, "manual_running" | "revoked_running"),
        automatic,
    )
}

#[cfg(test)]
#[path = "facade_tests.rs"]
mod tests;
