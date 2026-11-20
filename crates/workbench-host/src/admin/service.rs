use super::{
    audit::now_ms,
    denial::{Denials, surface},
    *,
};
use crate::areas::tasks::AreaTasks;
use crate::ingress::namespaces::lifecycle::{owned_graphs, quarantine, restart};
use futures_util::FutureExt;
use std::{
    collections::BTreeSet,
    sync::{Arc, Mutex, RwLock},
};
use storage::admin_audit::*;
use tokio::sync::{Semaphore, oneshot};
use workbench_authority::IngressContext;
use workbench_protocol::{
    errors::ErrorCode,
    namespaces::lifecycle::{
        AdminAction,
        restart::{RestartQuery, RestartReply},
    },
    wire::InstanceEpoch,
};
pub type AdminReplyReceiver = oneshot::Receiver<Result<AdminAccepted, AdminFailure>>;
pub type AdminViewReceiver = oneshot::Receiver<Result<AdminView, AdminFailure>>;
pub struct AuthorizedAdminRequest {
    intent: AdminIntent,
    context: IngressContext,
}
pub struct AuthorizedAdminQuery {
    query: AdminQuery,
    context: IngressContext,
}
/// Cannot be cloned, deserialized or constructed by a leaf/payload.
/// register consumes the one admitted operation; a second call cannot compile.
///
/// ```no_run
/// use workbench_host::admin::{CommitAdmission, AdminExecutionOutcome};
/// use workbench_protocol::namespaces::lifecycle::AdminOutcome;
/// fn once(admission: CommitAdmission) {
///     let _ = admission.register(Box::pin(async { Ok(AdminExecutionOutcome::Complete(AdminOutcome::Succeeded)) }));
/// }
/// ```
/// ```compile_fail
/// use workbench_host::admin::{CommitAdmission, AdminExecutionOutcome};
/// use workbench_protocol::namespaces::lifecycle::AdminOutcome;
/// fn twice(admission: CommitAdmission) {
///     let _ = admission.register(Box::pin(async { Ok(AdminExecutionOutcome::Complete(AdminOutcome::Succeeded)) }));
///     let _ = admission.register(Box::pin(async { Ok(AdminExecutionOutcome::Complete(AdminOutcome::Succeeded)) }));
/// }
/// ```
pub struct CommitAdmission {
    reply: oneshot::Sender<AdminContinuation>,
    receipt: AdminReceipt,
    binding: RestartAuditBinding,
    undisposed_revision: Option<workbench_protocol::wire::Revision>,
}
impl CommitAdmission {
    /// Restart commits must recheck this snapshot revision against the live owned-graph collector in their memory-only gate transition.
    pub fn undisposed_revision(&self) -> Option<workbench_protocol::wire::Revision> {
        self.undisposed_revision
    }
    pub fn restart_audit(&self) -> RestartAuditBinding {
        self.binding.clone()
    }
    pub fn register(self, continuation: AdminContinuation) -> AdminAccepted {
        let _ = self.reply.send(continuation);
        AdminAccepted {
            receipt: self.receipt,
        }
    }
}
#[derive(Default)]
struct Facades {
    restart: restart::Registry,
    quarantine: quarantine::Registry,
    owned_graphs: owned_graphs::Registry,
}
pub struct AdminService {
    facades: Facades,
    audit: Arc<AdminAudit>,
    denials: Arc<Denials>,
    tasks: AreaTasks,
    requests: Arc<Semaphore>,
    continuations: Arc<Semaphore>,
    background: runtime::producer::ProducerGate,
    epoch: InstanceEpoch,
    reader: Arc<AdminReader>,
    operation_reader: RwLock<Option<Arc<dyn OperationReader>>>,
    active: Arc<Mutex<BTreeSet<String>>>,
    operation_queries: Arc<Semaphore>,
}
impl AdminService {
    pub fn new(audit: AdminAudit, tasks: AreaTasks, epoch: InstanceEpoch) -> Arc<Self> {
        Arc::new(Self {
            facades: Facades::default(),
            audit: Arc::new(audit),
            denials: Arc::default(),
            tasks,
            requests: Arc::new(Semaphore::new(64)),
            continuations: Arc::new(Semaphore::new(32)),
            background: Default::default(),
            epoch,
            reader: AdminReader::new(),
            operation_reader: RwLock::default(),
            active: Arc::default(),
            operation_queries: Arc::new(Semaphore::new(8)),
        })
    }
    pub fn install_restart(&self, facade: Arc<dyn AdminFacade>) {
        self.facades.restart.install(facade);
    }
    pub fn install_quarantine(&self, facade: Arc<dyn AdminFacade>) {
        self.facades.quarantine.install(facade);
    }
    pub fn install_owned_graphs(&self, facade: Arc<dyn AdminFacade>) {
        self.facades.owned_graphs.install(facade);
    }
    pub fn producer_gate(&self) -> runtime::producer::ProducerGate {
        self.background.clone()
    }
    pub fn install_operation_reader(&self, reader: Arc<dyn OperationReader>) {
        *self.operation_reader.write().unwrap() = Some(reader);
    }
    pub fn reader(&self) -> &Arc<AdminReader> {
        &self.reader
    }
    pub fn audit(&self) -> &Arc<AdminAudit> {
        &self.audit
    }
    pub fn register_worker(
        self: &Arc<Self>,
        services: &mut crate::bootstrap::services::BackgroundServices,
    ) {
        self.reader.register(services);
        let service = self.clone();
        services.start("admin-audit", move |stop| async move {
            service
                .denials
                .clone()
                .run(service.audit.clone(), service.background.clone(), stop)
                .await;
            Ok(())
        });
    }
    pub fn authorize(
        &self,
        context: &IngressContext,
        action: AdminAction,
    ) -> Result<(), AdminFailure> {
        context.authorize().map(|_| ()).map_err(|reason| {
            self.denials.record(context, action, reason);
            failure(
                ErrorCode::PermissionDenied,
                "verified human management authority required",
            )
        })
    }
    fn refusal(
        &self,
        context: &IngressContext,
        action: AdminAction,
        error: AdminFailure,
    ) -> AdminFailure {
        if error.code == ErrorCode::PermissionDenied {
            return error;
        }
        let reason = match error.code {
            ErrorCode::Conflict | ErrorCode::InvalidRequest => DenialReason::StaleBinding,
            ErrorCode::Busy => DenialReason::Capacity,
            _ => DenialReason::Unavailable,
        };
        self.denials.record_reason(context, action, reason);
        error
    }
    pub fn authorize_request(
        &self,
        context: IngressContext,
        intent: AdminIntent,
    ) -> Result<AuthorizedAdminRequest, AdminFailure> {
        self.authorize(&context, intent.action())?;
        intent.validate().map_err(|_| {
            self.refusal(
                &context,
                intent.action(),
                failure(
                    ErrorCode::InvalidRequest,
                    "invalid management confirmation or binding",
                ),
            )
        })?;
        let epoch=match &intent{LifecycleCommand::Restart(workbench_protocol::namespaces::lifecycle::restart::RestartCommand::Request(r))=>r.binding.expected_core_epoch,LifecycleCommand::Restart(workbench_protocol::namespaces::lifecycle::restart::RestartCommand::Cancel(r))=>r.expected_core_epoch,LifecycleCommand::Restart(workbench_protocol::namespaces::lifecycle::restart::RestartCommand::Interrupt(r))=>r.binding.core_epoch,LifecycleCommand::Quarantine(workbench_protocol::namespaces::lifecycle::quarantine::QuarantineCommand::Discard(r))=>r.binding.expected_core_epoch,LifecycleCommand::OwnedGraphs(workbench_protocol::namespaces::lifecycle::owned_graphs::OwnedGraphsCommand::Release(r)|workbench_protocol::namespaces::lifecycle::owned_graphs::OwnedGraphsCommand::Resume(r))=>r.binding.graph.expected_core_epoch};
        if epoch != self.epoch {
            return Err(self.refusal(&context, intent.action(), conflict()));
        }
        Ok(AuthorizedAdminRequest { intent, context })
    }
    pub fn authorize_query(
        &self,
        context: IngressContext,
        query: AdminQuery,
    ) -> Result<AuthorizedAdminQuery, AdminFailure> {
        self.authorize(&context, query.action().ok_or_else(unavailable)?)?;
        Ok(AuthorizedAdminQuery { query, context })
    }
    fn facade(
        &self,
        action: AdminAction,
        owned: bool,
    ) -> Result<Arc<dyn AdminFacade>, AdminFailure> {
        let f = &self.facades;
        match action {
            AdminAction::Quarantine | AdminAction::Discard => f.quarantine.get(),
            AdminAction::Release | AdminAction::Resume => f.owned_graphs.get(),
            _ if owned => f.owned_graphs.get(),
            _ => f.restart.get(),
        }
        .ok_or_else(unavailable)
    }
    pub fn submit(
        self: &Arc<Self>,
        request: AuthorizedAdminRequest,
    ) -> Result<AdminReplyReceiver, AdminFailure> {
        self.authorize(&request.context, request.intent.action())?;
        let facade = self
            .facade(request.intent.action(), false)
            .map_err(|e| self.refusal(&request.context, request.intent.action(), e))?;
        let (tx, rx) = oneshot::channel();
        if request.intent.action() == AdminAction::Cancel {
            let entry = self.entry(&request.intent, &request.context)?;
            self.authorize(&request.context, AdminAction::Cancel)?;
            let outcome = facade
                .cancel(&request.intent)
                .map_err(|e| self.refusal(&request.context, AdminAction::Cancel, e))?;
            // Never wait for audit ACK, a general slot or an audit gate. Warn even if the
            // best-effort record later succeeds: the response does not claim durability.
            let receipt = AdminReceipt {
                operation_id: request.intent.operation_id().clone(),
                state: AdminOperationState::Completed,
                outcome,
                audit_warning: true,
            };
            let service = self.clone();
            let _ = self.tasks.spawn("admin-cancel-audit", move |_| async move {
                let _ = service
                    .audit
                    .background_cancel(entry, outcome, &service.background)
                    .await;
                Ok(())
            });
            let _ = tx.send(Ok(AdminAccepted { receipt }));
            return Ok(rx);
        }
        #[cfg(not(target_os = "linux"))]
        return Err(failure(
            ErrorCode::UnsupportedCapability,
            "management effects require Linux",
        ));
        #[cfg(target_os = "linux")]
        {
            let request_slot = self
                .requests
                .clone()
                .try_acquire_owned()
                .map_err(|_| busy())?;
            let continuation_slot = self
                .continuations
                .clone()
                .try_acquire_owned()
                .map_err(|_| busy())?;
            let service = self.clone();
            self.tasks
                .spawn("admin-operation", move |_| async move {
                    let _request_slot = request_slot;
                    let _continuation_slot = continuation_slot;
                    service.run(request, facade, tx).await;
                    Ok(())
                })
                .map_err(|_| busy())?;
            Ok(rx)
        }
    }
    async fn run(
        &self,
        request: AuthorizedAdminRequest,
        facade: Arc<dyn AdminFacade>,
        reply: oneshot::Sender<Result<AdminAccepted, AdminFailure>>,
    ) {
        let action = request.intent.action();
        if let Err(error) = self.authorize(&request.context, action) {
            let _ = reply.send(Err(error));
            return;
        }
        let entry = match self.entry(&request.intent, &request.context) {
            Ok(e) => e,
            Err(e) => {
                let _ = reply.send(Err(self.refusal(&request.context, action, e)));
                return;
            }
        };
        // Readback precedes current-state collection/inspection. A prior
        // effect may already have advanced state; that is not a new admission.
        match self.audit.record(&entry.key).await {
            Ok(Some(row)) => {
                let result = if audit::same_intent(&entry, &row.accepted) {
                    Ok(self.readback_receipt(&request.intent, &row))
                } else {
                    Err(conflict())
                };
                let _ = reply.send(result);
                return;
            }
            Ok(None) => {
                if let Some(prior) = self.audit.pending_entry(&entry.key.operation_id) {
                    let result = if audit::same_intent(&entry, &prior) {
                        Ok(AdminAccepted {
                            receipt: AdminReceipt {
                                operation_id: request.intent.operation_id().clone(),
                                state: AdminOperationState::Unknown,
                                outcome: AdminOutcome::Unknown,
                                audit_warning: true,
                            },
                        })
                    } else {
                        Err(conflict())
                    };
                    let _ = reply.send(result);
                    return;
                }
            }
            Err(e) => {
                let _ = reply.send(Err(e));
                return;
            }
        }
        let _effect_admission = if action == AdminAction::Request {
            None
        } else {
            match self.audit.effect_admission() {
                Ok(p) => Some(p),
                Err(e) => {
                    let _ = reply.send(Err(self.refusal(&request.context, action, e)));
                    return;
                }
            }
        };
        let producer = if matches!(
            action,
            AdminAction::Discard | AdminAction::Release | AdminAction::Resume
        ) {
            match self.background.try_enter() {
                Some(p) => Some(p),
                None => {
                    let _ = reply.send(Err(self.refusal(&request.context, action, busy())));
                    return;
                }
            }
        } else {
            None
        };
        let undisposed_revision = if action == AdminAction::Request {
            let Some(collector) = self.facades.owned_graphs.get() else {
                let _ = reply.send(Err(self.refusal(&request.context, action, unavailable())));
                return;
            };
            let query=LifecycleQuery::OwnedGraphs(workbench_protocol::namespaces::lifecycle::owned_graphs::OwnedGraphsQuery::Undisposed);
            match tokio::time::timeout(audit::ADMIN_IO_ACK_BUDGET,collector.query(&query)).await{
    Ok(Ok(LifecycleReply::OwnedGraphs(workbench_protocol::namespaces::lifecycle::owned_graphs::OwnedGraphsReply::Undisposed(snapshot)))) if snapshot.validate().is_ok() && snapshot.count.get()==0 =>Some(snapshot.snapshot_revision),
    _=>{let _=reply.send(Err(conflict()));return;}
   }
        } else {
            None
        };
        let inspected =
            match tokio::time::timeout(audit::ADMIN_IO_ACK_BUDGET, facade.inspect(&request.intent))
                .await
            {
                Ok(Ok(i)) if i.matches(&request.intent) => i,
                Ok(Err(e)) => {
                    let _ = reply.send(Err(self.refusal(&request.context, action, e)));
                    return;
                }
                _ => {
                    let _ = reply.send(Err(self.refusal(&request.context, action, conflict())));
                    return;
                }
            };
        let key = entry.key.clone();
        let digest = entry.intent_digest.clone();
        match self.audit.accept(entry).await {
            Ok(AuditReservation::Created(_)) => {}
            Ok(AuditReservation::Existing(r)) => {
                let result = if r.accepted.intent_digest != digest {
                    Err(conflict())
                } else {
                    Ok(self.readback_receipt(&request.intent, &r))
                };
                let _ = reply.send(result);
                return;
            }
            Err(e) => {
                let _ = reply.send(Err(self.refusal(&request.context, action, e)));
                return;
            }
        }
        self.active.lock().unwrap().insert(key.operation_id.clone());
        let _active = ActiveOperation {
            active: self.active.clone(),
            operation_id: key.operation_id.clone(),
        };
        if let Err(e) = self.audit.start(&key).await {
            let _ = self
                .audit
                .finalize(&key, AuditOutcome::AbortedBeforeEffect)
                .await;
            let _ = reply.send(Err(self.refusal(&request.context, action, e)));
            return;
        }
        if let Err(e) = self.authorize(&request.context, action) {
            let _ = self
                .audit
                .finalize(&key, AuditOutcome::AbortedBeforeEffect)
                .await;
            let _ = reply.send(Err(self.refusal(&request.context, action, e)));
            return;
        }
        let binding = self.audit.binding(key.clone());
        let (continuation_tx, mut continuation_rx) = oneshot::channel();
        let receipt = AdminReceipt {
            operation_id: request.intent.operation_id().clone(),
            state: AdminOperationState::Started,
            outcome: AdminOutcome::Pending,
            audit_warning: false,
        };
        let admission = CommitAdmission {
            reply: continuation_tx,
            receipt: receipt.clone(),
            binding: binding.clone(),
            undisposed_revision,
        };
        let committed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            facade.commit(request.intent, inspected, admission)
        }))
        .unwrap_or_else(|_| Err(unknown()));
        drop(request.context);
        // Registration is synchronous. A registered continuation remains ours even
        // if the facade then returns an error or panics; never lose admitted work.
        let continuation = match continuation_rx.try_recv() {
            Ok(c) => c,
            Err(_) => {
                let (error, outcome) = match committed {
                    Err(e) => (e, AuditOutcome::AbortedBeforeEffect),
                    Ok(_) => (unknown(), AuditOutcome::Unknown),
                };
                let _ = self.audit.finalize(&key, outcome).await;
                let _ = reply.send(Err(error));
                return;
            }
        };
        let _ = reply.send(Ok(AdminAccepted { receipt }));
        let outcome = match std::panic::AssertUnwindSafe(continuation)
            .catch_unwind()
            .await
        {
            Ok(Ok(AdminExecutionOutcome::AwaitRestartRecovery))
                if action == AdminAction::Request && binding.is_bound() =>
            {
                drop(producer);
                return;
            }
            Ok(Ok(AdminExecutionOutcome::Complete(o))) => audit_outcome(o),
            _ => AuditOutcome::Unknown,
        };
        let _ = self.audit.finalize(&key, outcome).await;
    }
    fn readback_receipt(&self, intent: &AdminIntent, row: &AuditRecord) -> AdminAccepted {
        let active = self
            .active
            .lock()
            .unwrap()
            .contains(&row.accepted.key.operation_id)
            || (row.restart_id.is_some() && row.accepted.accepted_core_epoch == self.epoch.get());
        let state = record_state(row, active);
        AdminAccepted {
            receipt: AdminReceipt {
                operation_id: intent.operation_id().clone(),
                state,
                outcome: if state == AdminOperationState::Unknown {
                    AdminOutcome::Unknown
                } else {
                    record_outcome(row)
                },
                audit_warning: row.state != AuditState::Finished,
            },
        }
    }
    /// Denial aggregate rows explicitly dropped or with unknown persistence.
    pub fn denial_persistence_failures(&self) -> u64 {
        self.denials.persistence_failures()
    }
    pub fn query(
        self: &Arc<Self>,
        request: AuthorizedAdminQuery,
    ) -> Result<AdminViewReceiver, AdminFailure> {
        self.authorize(
            &request.context,
            request.query.action().ok_or_else(unavailable)?,
        )?;
        if let LifecycleQuery::OperationStatus { operation_id } = &request.query {
            let operation_id = operation_id.clone();
            let reader = self
                .operation_reader
                .read()
                .unwrap()
                .clone()
                .ok_or_else(unavailable)?;
            let slot = self
                .operation_queries
                .clone()
                .try_acquire_owned()
                .map_err(|_| busy())?;
            let (tx, rx) = oneshot::channel();
            let service = self.clone();
            self.tasks
                .spawn("admin-operation-status", move |_| async move {
                    let _slot = slot;
                    let read_id = operation_id.as_str().to_owned();
                    let result = service
                        .reader
                        .read(move || {
                            reader.operation(&read_id).map_err(|_| {
                                failure(
                                    ErrorCode::StorageUnavailable,
                                    "operation readback unavailable",
                                )
                            })
                        })
                        .await;
                    let result = service
                        .authorize(&request.context, AdminAction::Status)
                        .and(result)
                        .and_then(|row| {
                            let principal = request
                                .context
                                .authority()
                                .principal()
                                .ok_or_else(unavailable)?;
                            let row = row
                                .filter(|r| {
                                    r.accepted.principal.method == principal.method()
                                        && r.accepted.principal.subject == principal.subject()
                                })
                                .ok_or_else(|| {
                                    failure(ErrorCode::NotFound, "operation not found")
                                })?;
                            let active = service
                                .active
                                .lock()
                                .unwrap()
                                .contains(operation_id.as_str())
                                || (row.restart_id.is_some()
                                    && row.accepted.accepted_core_epoch == service.epoch.get());
                            let state = record_state(&row, active);
                            let outcome = if state == AdminOperationState::Unknown {
                                AdminOutcome::Unknown
                            } else {
                                record_outcome(&row)
                            };
                            Ok(LifecycleReply::OperationStatus(AdminReceipt {
                                operation_id,
                                state,
                                outcome,
                                audit_warning: state == AdminOperationState::Unknown,
                            }))
                        });
                    let _ = tx.send(result);
                    Ok(())
                })
                .map_err(|_| busy())?;
            return Ok(rx);
        }
        let facade = self.facade(
            request.query.action().ok_or_else(unavailable)?,
            matches!(&request.query, AdminQuery::OwnedGraphs(_)),
        )?;
        let (tx, rx) = oneshot::channel();
        if matches!(request.query, AdminQuery::Restart(RestartQuery::Status)) {
            let _ = tx.send(
                facade
                    .status()
                    .map(|view| self.stamp_management_epoch(view)),
            );
            return Ok(rx);
        }
        let slot = self
            .requests
            .clone()
            .try_acquire_owned()
            .map_err(|_| busy())?;
        let service = self.clone();
        self.tasks
            .spawn("admin-query", move |_| async move {
                let _slot = slot;
                let result = match tokio::time::timeout(
                    audit::ADMIN_IO_ACK_BUDGET,
                    facade.query(&request.query),
                )
                .await
                {
                    Ok(result) => result,
                    Err(_) => Err(unknown()),
                };
                let result = service
                    .authorize(&request.context, request.query.action().unwrap())
                    .and(result)
                    .map(|view| service.stamp_management_epoch(view));
                let _ = tx.send(result);
                Ok(())
            })
            .map_err(|_| busy())?;
        Ok(rx)
    }
    fn stamp_management_epoch(&self, mut view: AdminView) -> AdminView {
        match &mut view {
            LifecycleReply::Restart(RestartReply::Targets(t)) => t.core_epoch = self.epoch,
            LifecycleReply::Restart(RestartReply::Status(s)) => {
                s.core_epoch = self.epoch;
                s.audit = self.denials.diagnostics();
            }
            LifecycleReply::OwnedGraphs(
                workbench_protocol::namespaces::lifecycle::owned_graphs::OwnedGraphsReply::Detail(
                    d,
                ),
            ) => d.binding.graph.expected_core_epoch = self.epoch,
            _ => {}
        }
        view
    }
    /// Test-only observation after joining owned tasks; no polling is needed.
    #[cfg(feature = "test-support")]
    pub fn available_admission_slots(&self) -> (usize, usize) {
        (
            self.requests.available_permits(),
            self.continuations.available_permits(),
        )
    }
    /// Join admission completion without cancelling the request task.
    #[cfg(feature = "test-support")]
    pub async fn wait_for_request_slots(&self) {
        let _requests = self.requests.acquire_many(64).await.unwrap();
        let _continuations = self.continuations.acquire_many(32).await.unwrap();
    }
    fn entry(
        &self,
        intent: &AdminIntent,
        ctx: &IngressContext,
    ) -> Result<AuditAccepted, AdminFailure> {
        use sha2::Digest;
        let principal = ctx.authority().principal().ok_or_else(unavailable)?;
        let bytes = serde_json::to_vec(intent).map_err(|_| conflict())?;
        let operation = match intent.action() {
            AdminAction::Request => AuditOperation::RestartRequest,
            AdminAction::Cancel => AuditOperation::Cancel,
            AdminAction::Interrupt => AuditOperation::Interrupt,
            AdminAction::Discard => AuditOperation::Discard,
            AdminAction::Release => AuditOperation::Release,
            AdminAction::Resume => AuditOperation::Resume,
            _ => return Err(unavailable()),
        };
        Ok(AuditAccepted {
            key: AuditOperationKey {
                operation_id: intent.operation_id().as_str().to_owned(),
                attempt_nonce: uuid::Uuid::new_v4().to_string(),
            },
            operation,
            surface: surface(ctx),
            principal: AuditPrincipal {
                method: principal.method().to_owned(),
                subject: principal.subject().to_owned(),
                binding: principal.binding().to_owned(),
            },
            accepted_core_epoch: self.epoch.get(),
            intent_digest: format!("{:x}", sha2::Sha256::digest(bytes)),
            accepted_at_ms: now_ms(),
        })
    }
}
fn record_outcome(record: &AuditRecord) -> AdminOutcome {
    match record.outcome {
        Some(AuditOutcome::Succeeded) => AdminOutcome::Succeeded,
        Some(AuditOutcome::Cancelled) => AdminOutcome::Cancelled,
        Some(AuditOutcome::Conflict) => AdminOutcome::Conflict,
        Some(AuditOutcome::Unknown) => AdminOutcome::Unknown,
        Some(_) => AdminOutcome::Failed,
        None => AdminOutcome::Pending,
    }
}
fn audit_outcome(outcome: AdminOutcome) -> AuditOutcome {
    match outcome {
        AdminOutcome::Succeeded => AuditOutcome::Succeeded,
        AdminOutcome::Cancelled => AuditOutcome::Cancelled,
        AdminOutcome::Conflict => AuditOutcome::Conflict,
        AdminOutcome::Failed => AuditOutcome::Failed,
        AdminOutcome::Pending | AdminOutcome::Unknown => AuditOutcome::Unknown,
    }
}

pub trait OperationReader: Send + Sync + 'static {
    fn operation(&self, id: &str) -> Result<Option<AuditRecord>, storage::StorageError>;
}
impl OperationReader for storage::StorageConfig {
    fn operation(&self, id: &str) -> Result<Option<AuditRecord>, storage::StorageError> {
        storage::Database::read_admin_audit_record(self, id)
    }
}
struct ActiveOperation {
    active: Arc<Mutex<BTreeSet<String>>>,
    operation_id: String,
}
impl Drop for ActiveOperation {
    fn drop(&mut self) {
        self.active.lock().unwrap().remove(&self.operation_id);
    }
}
fn record_state(row: &AuditRecord, active: bool) -> AdminOperationState {
    match row.state {
        AuditState::Prepared => {
            if active {
                AdminOperationState::Pending
            } else {
                AdminOperationState::Unknown
            }
        }
        AuditState::Started => {
            if active {
                AdminOperationState::Started
            } else {
                AdminOperationState::Unknown
            }
        }
        AuditState::Finished => {
            if row.outcome == Some(AuditOutcome::Unknown) {
                AdminOperationState::Unknown
            } else {
                AdminOperationState::Completed
            }
        }
    }
}
