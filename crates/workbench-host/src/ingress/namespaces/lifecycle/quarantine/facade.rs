use super::{
    store::QuarantineStore,
    view::{Rendered, render, resolve},
};
use crate::admin::{
    AdminAccepted, AdminExecutionOutcome, AdminFacade, AdminFailure, AdminInspection, AdminIntent,
    AdminIo, AdminQuery, AdminReader, AdminView, CommitAdmission, conflict, failure,
};
use std::sync::Arc;
use storage::recovery::quarantine::{
    DiscardRequest, QUARANTINE_LIST_CAP, QUARANTINE_SCAN_CAP, QuarantineCompletion as Completion,
    QuarantineSnapshot, QuarantinedGraph,
};
use workbench_protocol::{
    errors::ErrorCode,
    namespaces::lifecycle::{
        AdminOutcome, GraphCas, LifecycleCommand, LifecycleQuery, LifecycleReply,
        quarantine::{QuarantineCommand, QuarantineDetail, QuarantineQuery, QuarantineReply},
    },
    wire::{InstanceEpoch, Revision},
};

/// Q1's facade. It holds no state: every answer is read from storage again, so
/// a reload or a new instance observes the same revocations.
pub struct QuarantineFacade {
    store: Arc<dyn QuarantineStore>,
    reader: Arc<AdminReader>,
    epoch: InstanceEpoch,
}
impl QuarantineFacade {
    pub fn new(
        store: Arc<dyn QuarantineStore>,
        reader: Arc<AdminReader>,
        epoch: InstanceEpoch,
    ) -> Arc<Self> {
        Arc::new(Self {
            store,
            reader,
            epoch,
        })
    }
}

fn storage_unavailable() -> AdminFailure {
    failure(
        ErrorCode::StorageUnavailable,
        "quarantine storage unavailable",
    )
}
fn snapshot(store: &dyn QuarantineStore, limit: usize) -> Result<QuarantineSnapshot, AdminFailure> {
    store.snapshot(limit).map_err(|_| storage_unavailable())
}

impl AdminFacade for QuarantineFacade {
    /// The current state of the one graph a discard names, as the binding a
    /// client would have to hold. A graph with an owner is not this leaf's.
    fn inspect<'a>(&'a self, intent: &'a AdminIntent) -> AdminIo<'a, AdminInspection> {
        Box::pin(async move {
            let LifecycleCommand::Quarantine(QuarantineCommand::Discard(confirmed)) = intent else {
                return Err(conflict());
            };
            let operation_id = confirmed.binding.operation_id.clone();
            let graph_id = confirmed.binding.graph_id.clone();
            let store = self.store.clone();
            let epoch = self.epoch;
            self.reader
                .read(move || {
                    let found = resolve(snapshot(&*store, QUARANTINE_SCAN_CAP)?, &graph_id)?;
                    if !found.item.cold || !found.raw.terminal {
                        return Err(conflict());
                    }
                    Ok(AdminInspection::Graph(GraphCas {
                        graph_id,
                        restart_tx_id: found.item.restart_tx_id,
                        capture_id: found.item.capture_id,
                        expected_revision: found.item.revision,
                        expected_core_epoch: epoch,
                        owner_identity: None,
                        owner_generation: None,
                        operation_id,
                    }))
                })
                .await
        })
    }

    fn query<'a>(&'a self, query: &'a AdminQuery) -> AdminIo<'a, AdminView> {
        Box::pin(async move {
            let LifecycleQuery::Quarantine(query) = query else {
                return Err(conflict());
            };
            let store = self.store.clone();
            let epoch = self.epoch;
            let query = query.clone();
            let reply = self
                .reader
                .read(move || match query {
                    QuarantineQuery::List => {
                        let snapshot = snapshot(&*store, QUARANTINE_LIST_CAP)?;
                        let truncated = snapshot.truncated;
                        let items: Vec<_> = render(snapshot.graphs)?
                            .into_iter()
                            .map(|r| r.item)
                            .collect();
                        let revision = items.iter().fold(items.len() as u64, |sum, i| {
                            sum.saturating_add(i.revision.get())
                        });
                        Ok(QuarantineReply::List {
                            items,
                            revision: Revision(revision),
                            truncated,
                        })
                    }
                    QuarantineQuery::Detail { graph_id } => {
                        let found = resolve(snapshot(&*store, QUARANTINE_SCAN_CAP)?, &graph_id)?;
                        Ok(QuarantineReply::Detail(QuarantineDetail {
                            item: found.item,
                            core_epoch: epoch,
                        }))
                    }
                })
                .await?;
            reply.validate().map_err(|_| conflict())?;
            Ok(LifecycleReply::Quarantine(reply))
        })
    }

    /// Memory only: the effect itself runs in the registered continuation.
    fn commit(
        &self,
        intent: AdminIntent,
        expected: AdminInspection,
        admission: CommitAdmission,
    ) -> Result<AdminAccepted, AdminFailure> {
        let LifecycleCommand::Quarantine(QuarantineCommand::Discard(confirmed)) = intent else {
            return Err(conflict());
        };
        let binding = confirmed.binding;
        if expected != AdminInspection::Graph(binding.clone())
            || binding.expected_core_epoch != self.epoch
            || binding.owner_identity.is_some()
            || binding.owner_generation.is_some()
        {
            return Err(conflict());
        }
        let store = self.store.clone();
        let reader = self.reader.clone();
        let nonce = uuid::Uuid::new_v4().to_string();
        Ok(admission.register(Box::pin(async move {
            let job = move || discard(&*store, &binding, nonce);
            match reader.read(job).await {
                Ok(outcome) => Ok(AdminExecutionOutcome::Complete(outcome)),
                // The job was never queued: nothing can have been applied.
                Err(e) if e.code == ErrorCode::Busy => {
                    Ok(AdminExecutionOutcome::Complete(AdminOutcome::Failed))
                }
                Err(e) => Err(e),
            }
        })))
    }
}

/// The overall outcome of a discard. `Succeeded` means the eligibility is
/// revoked and nothing is left to clean up; revocation alone is not success.
fn outcome_of(graph: &QuarantinedGraph) -> AdminOutcome {
    match (graph.revoked, graph.completion) {
        (true, Completion::Complete) => AdminOutcome::Succeeded,
        (true, Completion::Unknown) => AdminOutcome::Unknown,
        _ => AdminOutcome::Failed,
    }
}

fn find<'a>(
    snapshot: &'a QuarantineSnapshot,
    raw: &QuarantinedGraph,
) -> Option<&'a QuarantinedGraph> {
    snapshot
        .graphs
        .iter()
        .find(|g| g.restart_id == raw.restart_id && g.manifest_id == raw.manifest_id)
}

/// Blocking: resolves the reference again, then writes with the revision fence.
fn discard(
    store: &dyn QuarantineStore,
    binding: &GraphCas,
    nonce: String,
) -> Result<AdminOutcome, AdminFailure> {
    // Nothing has been written yet, so a failed read is a definite failure.
    let Ok(window) = store.snapshot(QUARANTINE_SCAN_CAP) else {
        return Ok(AdminOutcome::Failed);
    };
    let Some(Rendered { raw, item }) = render(window.graphs).ok().and_then(|all| {
        all.into_iter()
            .find(|r| r.item.graph_id == binding.graph_id)
    }) else {
        return Ok(AdminOutcome::Conflict);
    };
    if item.restart_tx_id != binding.restart_tx_id || item.capture_id != binding.capture_id {
        return Ok(AdminOutcome::Conflict);
    }
    let request = DiscardRequest {
        restart_id: raw.restart_id.clone(),
        capture_id: raw.capture_id.clone(),
        manifest_id: raw.manifest_id.clone(),
        expected_revision: binding.expected_revision.get(),
        operation_id: binding.operation_id.as_str().to_owned(),
        attempt_nonce: nonce,
    };
    match store.discard(request.clone()) {
        Ok(Ok(result)) => Ok(outcome_of(&result.graph)),
        // A definite refusal wrote nothing: the graph changed since inspection.
        Ok(Err(_)) => Ok(AdminOutcome::Conflict),
        Err(_) => settle(store, &request, &raw),
    }
}

/// An unknown acknowledgement is settled from storage by the same operation ID:
/// the effect is never issued twice.
fn settle(
    store: &dyn QuarantineStore,
    request: &DiscardRequest,
    raw: &QuarantinedGraph,
) -> Result<AdminOutcome, AdminFailure> {
    let window = store
        .snapshot(QUARANTINE_SCAN_CAP)
        .map_err(|_| crate::admin::unknown())?;
    let Some(graph) = find(&window, raw) else {
        return Err(crate::admin::unknown());
    };
    let ours = graph
        .control
        .as_ref()
        .is_some_and(|c| c.effect.operation_id == request.operation_id);
    Ok(match (graph.revoked, ours) {
        (true, true) => outcome_of(graph),
        (true, false) => AdminOutcome::Conflict,
        (false, _) => AdminOutcome::Failed,
    })
}
