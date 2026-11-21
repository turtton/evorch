//! Fake facades and verifier exist only in this contract test binary.
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};
use storage::{StorageError, admin_audit::*};
use tokio::sync::mpsc;
use workbench_authority::{IngressContext, test_support};
use workbench_host::{
    admin::*,
    areas::tasks::AreaTasks,
    bootstrap::restart::{HostRestartGate, HostRestartPhase},
};
use workbench_protocol::{
    envelope::{Provenance, Surface},
    errors::{ErrorCode, RetryAdvice, WorkbenchError},
    ids::{ClientId, RunId},
    namespaces::lifecycle::{restart::*, *},
    wire::{InstanceEpoch, Revision},
};
fn id(s: &str) -> AdminId {
    AdminId::new(s).unwrap()
}
fn fail(code: ErrorCode) -> WorkbenchError {
    WorkbenchError::new(code, "contract failure", RetryAdvice::Never)
}
fn request(n: &str) -> AdminIntent {
    LifecycleCommand::Restart(RestartCommand::Request(RequestRestart {
        binding: RestartBinding {
            target_id: id("current"),
            expected_core_epoch: InstanceEpoch(7),
            target_revision: Revision(2),
            operation_id: id(n),
        },
        confirmed: true,
    }))
}
fn inspection(intent: &AdminIntent) -> AdminInspection {
    match intent {
        LifecycleCommand::Restart(RestartCommand::Request(r)) => {
            AdminInspection::Restart(r.binding.clone())
        }
        LifecycleCommand::Restart(RestartCommand::Interrupt(r)) => {
            AdminInspection::Interrupt(r.binding.clone())
        }
        LifecycleCommand::Quarantine(quarantine::QuarantineCommand::Discard(r)) => {
            AdminInspection::Graph(r.binding.clone())
        }
        LifecycleCommand::OwnedGraphs(
            owned_graphs::OwnedGraphsCommand::Release(r)
            | owned_graphs::OwnedGraphsCommand::Resume(r),
        ) => AdminInspection::OwnedGraph(r.binding.clone()),
        _ => panic!("not inspected"),
    }
}
fn status() -> AdminView {
    LifecycleReply::Restart(RestartReply::Status(RestartManagementStatus {
        audit: Default::default(),
        core_epoch: InstanceEpoch(999),
        status: RestartStatus {
            restart_id: None,
            phase: RestartPhase::Running,
            revision: 1,
            blockers: vec![],
            blocker_total: 0,
            blocker_omitted: 0,
            active_producers: 0,
        },
    }))
}
fn owned(graph: GraphCas) -> owned_graphs::ConfirmedOwnedGraph {
    owned_graphs::ConfirmedOwnedGraph {
        binding: owned_graphs::OwnedGraphBinding {
            graph,
            owners: vec![],
            expected_control_revision: Revision(0),
        },
        confirmed: true,
    }
}
struct Facade {
    live: Mutex<AdminInspection>,
    cancelled: AtomicBool,
    effects: Arc<AtomicUsize>,
    after_register: AtomicUsize,
    inspections: AtomicUsize,
    connections: Mutex<Vec<(&'static str, InstanceEpoch)>>,
    await_recovery: AtomicBool,
    bind_recovery: AtomicBool,
}
impl Facade {
    fn new(intent: &AdminIntent) -> Arc<Self> {
        Arc::new(Self {
            live: Mutex::new(inspection(intent)),
            cancelled: AtomicBool::new(false),
            effects: Arc::new(AtomicUsize::new(0)),
            after_register: AtomicUsize::new(0),
            inspections: AtomicUsize::new(0),
            connections: Mutex::new(vec![]),
            await_recovery: AtomicBool::new(false),
            bind_recovery: AtomicBool::new(false),
        })
    }
}
impl AdminFacade for Facade {
    fn inspect<'a>(
        &'a self,
        _: &'a AdminIntent,
        connection: &'a VerifiedConnection,
    ) -> AdminIo<'a, AdminInspection> {
        self.connections
            .lock()
            .unwrap()
            .push(("inspect", connection.epoch()));
        self.inspections.fetch_add(1, Ordering::SeqCst);
        let snapshot = self.live.lock().unwrap().clone();
        Box::pin(async move { Ok(snapshot) })
    }
    fn query<'a>(
        &'a self,
        q: &'a AdminQuery,
        connection: &'a VerifiedConnection,
    ) -> AdminIo<'a, AdminView> {
        self.connections
            .lock()
            .unwrap()
            .push(("query", connection.epoch()));
        Box::pin(async move {
            if matches!(q, LifecycleQuery::OwnedGraphs(_)) {
                Ok(LifecycleReply::OwnedGraphs(
                    owned_graphs::OwnedGraphsReply::Undisposed(owned_graphs::UndisposedGraphs {
                        count: Revision(0),
                        references: vec![],
                        snapshot_revision: Revision(1),
                        truncated: false,
                        over_limit_count: Revision(0),
                        incomplete_disposals: owned_graphs::IncompleteDisposals::empty(),
                    }),
                ))
            } else {
                Ok(status())
            }
        })
    }
    fn status(&self, connection: &VerifiedConnection) -> Result<AdminView, AdminFailure> {
        self.connections
            .lock()
            .unwrap()
            .push(("status", connection.epoch()));
        Ok(status())
    }
    fn cancel(&self, _: &AdminIntent) -> Result<AdminOutcome, AdminFailure> {
        self.cancelled.store(true, Ordering::SeqCst);
        Ok(AdminOutcome::Cancelled)
    }
    fn commit(
        &self,
        intent: AdminIntent,
        expected: AdminInspection,
        admission: CommitAdmission,
        connection: &VerifiedConnection,
    ) -> Result<AdminAccepted, AdminFailure> {
        self.connections
            .lock()
            .unwrap()
            .push(("commit", connection.epoch()));
        assert_eq!(connection.epoch(), admission.connection().epoch());
        assert!(admission.connection().matches(connection.epoch()));
        let live = self.live.lock().unwrap();
        if *live != expected || !expected.matches(&intent) || self.cancelled.load(Ordering::SeqCst)
        {
            return Err(fail(ErrorCode::Conflict));
        }
        let effects = self.effects.clone();
        let binding = admission.restart_audit();
        let await_recovery = self.await_recovery.load(Ordering::SeqCst);
        let bind_recovery = self.bind_recovery.load(Ordering::SeqCst);
        let accepted = admission.register(Box::pin(async move {
            effects.fetch_add(1, Ordering::SeqCst);
            if bind_recovery {
                binding.bind(&id("bound-restart")).await?;
            }
            if await_recovery {
                return Ok(AdminExecutionOutcome::AwaitRestartRecovery);
            }
            Ok(AdminExecutionOutcome::Complete(AdminOutcome::Succeeded))
        }));
        match self.after_register.load(Ordering::SeqCst) {
            1 => Err(fail(ErrorCode::Conflict)),
            2 => panic!("after registration"),
            _ => Ok(accepted),
        }
    }
}
type Held = (AuditReplyCallback, Result<AuditReply, StorageError>);
struct Memory {
    rows: BTreeMap<String, AuditRecord>,
    held: VecDeque<Held>,
}
struct Port {
    memory: Mutex<Memory>,
    hold_accept: AtomicBool,
    fail_accept: AtomicBool,
    fail_start: AtomicBool,
    fail_finalize: AtomicBool,
    lose_accept_ack: AtomicBool,
    events: mpsc::UnboundedSender<&'static str>,
}
impl Port {
    fn new() -> (Arc<Self>, mpsc::UnboundedReceiver<&'static str>) {
        let (events, rx) = mpsc::unbounded_channel();
        (
            Arc::new(Self {
                memory: Mutex::new(Memory {
                    rows: BTreeMap::new(),
                    held: VecDeque::new(),
                }),
                hold_accept: AtomicBool::new(false),
                fail_accept: AtomicBool::new(false),
                fail_start: AtomicBool::new(false),
                fail_finalize: AtomicBool::new(false),
                lose_accept_ack: AtomicBool::new(false),
                events,
            }),
            rx,
        )
    }
    fn release(&self) {
        self.hold_accept.store(false, Ordering::SeqCst);
        let held = std::mem::take(&mut self.memory.lock().unwrap().held);
        for (reply, result) in held {
            reply(result);
        }
    }
    fn record(&self, n: &str) -> AuditRecord {
        self.memory.lock().unwrap().rows[n].clone()
    }
}
impl AuditPort for Port {
    fn submit(&self, write: AuditWrite, reply: AuditReplyCallback) -> Result<(), StorageError> {
        let mut m = self.memory.lock().unwrap();
        let accept = matches!(write, AuditWrite::Accept(_));
        let result = match write {
            AuditWrite::Accept(a) => {
                let _ = self.events.send("accept");
                if self.fail_accept.load(Ordering::SeqCst) {
                    Err(StorageError::Io("accept failed".into()))
                } else if let Some(old) = m.rows.get(&a.key.operation_id) {
                    Ok(AuditReply::Reservation(AuditReservation::Existing(
                        old.clone(),
                    )))
                } else {
                    let row = AuditRecord {
                        accepted: a,
                        state: AuditState::Prepared,
                        restart_id: None,
                        outcome: None,
                        finished_at_ms: None,
                    };
                    m.rows
                        .insert(row.accepted.key.operation_id.clone(), row.clone());
                    Ok(AuditReply::Reservation(AuditReservation::Created(row)))
                }
            }
            AuditWrite::Start { key } => {
                let _ = self.events.send("start");
                if self.fail_start.load(Ordering::SeqCst) {
                    Err(StorageError::Io("start failed".into()))
                } else {
                    let r = m.rows.get_mut(&key.operation_id).unwrap();
                    assert_eq!(r.accepted.key, key);
                    r.state = AuditState::Started;
                    Ok(AuditReply::Applied(r.clone()))
                }
            }
            AuditWrite::Read { operation_id } => {
                let _ = self.events.send("read");
                Ok(AuditReply::Record(m.rows.get(&operation_id).cloned()))
            }
            AuditWrite::Finalize {
                key,
                outcome,
                at_ms,
            } => {
                let _ = self.events.send("finalize");
                if self.fail_finalize.load(Ordering::SeqCst) {
                    Err(StorageError::Io("finalize failed".into()))
                } else {
                    let r = m.rows.get_mut(&key.operation_id).unwrap();
                    assert_eq!(r.accepted.key, key);
                    r.state = AuditState::Finished;
                    r.outcome = Some(outcome);
                    r.finished_at_ms = Some(at_ms);
                    Ok(AuditReply::Applied(r.clone()))
                }
            }
            AuditWrite::BindRestart { key, restart_id } => {
                let r = m.rows.get_mut(&key.operation_id).unwrap();
                r.restart_id = Some(restart_id);
                Ok(AuditReply::Applied(r.clone()))
            }
            AuditWrite::Denials { rows, .. } => Ok(AuditReply::Denials {
                stored: rows.len(),
                dropped: 0,
            }),
            AuditWrite::Cleanup { .. } | AuditWrite::Reconcile { .. } => {
                Ok(AuditReply::Cleanup(AuditCleanup {
                    accepted_removed: 0,
                    denials_removed: 0,
                    reconciled: 0,
                    more: false,
                }))
            }
            AuditWrite::Stats => unreachable!(),
        };
        if accept && self.hold_accept.load(Ordering::SeqCst) {
            m.held.push_back((reply, result));
            return Ok(());
        }
        drop(m);
        if accept && self.lose_accept_ack.swap(false, Ordering::SeqCst) {
            drop(reply);
        } else {
            reply(result);
        }
        Ok(())
    }
}
struct Harness {
    services: Mutex<Option<workbench_host::bootstrap::services::BackgroundServices>>,
    service: Arc<AdminService>,
    tasks: AreaTasks,
    port: Arc<Port>,
    facade: Arc<Facade>,
    context: IngressContext,
    _lease: test_support::Lease,
    events: mpsc::UnboundedReceiver<&'static str>,
}
impl Harness {
    fn new(intent: &AdminIntent) -> Self {
        Self::with_switch(intent, true)
    }
    fn with_switch(intent: &AdminIntent, published: bool) -> Self {
        let (port, events) = Port::new();
        let tasks = AreaTasks::new(tokio::runtime::Handle::current());
        let service = AdminService::new(
            AdminAudit::new(port.clone(), HostRestartGate::default()),
            tasks.clone(),
            InstanceEpoch(7),
        );
        if published {
            service.enable_management();
        }
        let facade = Facade::new(intent);
        service.install_restart(facade.clone());
        service.install_quarantine(facade.clone());
        service.install_owned_graphs(facade.clone());
        service.install_operation_reader(port.clone());
        let mut services = workbench_host::bootstrap::services::BackgroundServices::default();
        service.register_worker(&mut services);
        let (proof, lease) = test_support::local();
        let context = IngressContext::new(
            Provenance::human(Surface::Desktop, ClientId::new("contract-human").unwrap()),
            proof,
        );
        Self {
            services: Mutex::new(Some(services)),
            service,
            tasks,
            port,
            facade,
            context,
            _lease: lease,
            events,
        }
    }
    fn submit(&self, intent: AdminIntent) -> Result<AdminReplyReceiver, AdminFailure> {
        self.service.submit(
            self.service
                .authorize_request(self.context.clone(), intent)?,
        )
    }
    async fn event(&mut self, want: &str) {
        while self.events.recv().await.unwrap() != want {}
    }
    async fn shutdown(&self) {
        self.tasks.request_stop();
        self.tasks.shutdown().await.unwrap();
        let services = { self.services.lock().unwrap().take() };
        if let Some(mut services) = services {
            services.shutdown().await.unwrap();
        }
    }
}
#[tokio::test]
async fn every_operation_rejects_unverified_humans_agents_and_system_before_audit() {
    let h = Harness::new(&request("auth"));
    for action in AdminAction::ALL {
        for surface in [Surface::Desktop, Surface::Cli, Surface::Web, Surface::None] {
            let forged = Provenance::human(surface, ClientId::new("claimed-human").unwrap());
            let saved: Provenance =
                serde_json::from_str(&serde_json::to_string(&forged).unwrap()).unwrap();
            assert_eq!(
                h.service
                    .authorize(&IngressContext::unverified(saved), action)
                    .unwrap_err()
                    .code,
                ErrorCode::PermissionDenied
            );
        }
        let proof = h.context.authority().clone();
        assert!(
            h.service
                .authorize(
                    &IngressContext::new(Provenance::agent(RunId::new("agent").unwrap()), proof),
                    action
                )
                .is_err()
        );
        assert!(
            h.service
                .authorize(
                    &IngressContext::system(h.context.provenance().clone()),
                    action
                )
                .is_err()
        );
    }
    assert!(h.port.memory.lock().unwrap().rows.is_empty());
    assert_eq!(h.facade.effects.load(Ordering::SeqCst), 0);
    h.shutdown().await;
}
#[tokio::test]
async fn audit_accept_and_start_fail_closed_for_all_five_irreversible_effects() {
    let graph = ConfirmedGraph {
        binding: GraphCas {
            graph_id: id("g"),
            restart_tx_id: id("tx"),
            capture_id: id("cap"),
            expected_revision: Revision(1),
            expected_core_epoch: InstanceEpoch(7),
            owner_identity: None,
            owner_generation: None,
            operation_id: id("op"),
        },
        confirmed: true,
    };
    let interrupt = LifecycleCommand::Restart(RestartCommand::Interrupt(InterruptShell {
        binding: InterruptBinding {
            restart_id: id("restart"),
            core_epoch: InstanceEpoch(7),
            connection_epoch: InstanceEpoch(3),
            run_incarnation: id("inc"),
            blocker_identity: id("opaque"),
            revision: Revision(1),
            phase: RestartPhase::WaitingSafePoint,
        },
        operation_id: id("interrupt"),
        confirmed: true,
    }));
    for intent in [
        request("restart"),
        interrupt,
        LifecycleCommand::Quarantine(quarantine::QuarantineCommand::Discard(graph.clone())),
        LifecycleCommand::OwnedGraphs(owned_graphs::OwnedGraphsCommand::Release(owned(
            graph.binding.clone(),
        ))),
        LifecycleCommand::OwnedGraphs(owned_graphs::OwnedGraphsCommand::Resume(owned(
            graph.binding.clone(),
        ))),
    ] {
        for fail_start in [false, true] {
            let h = Harness::new(&intent);
            h.port.fail_accept.store(!fail_start, Ordering::SeqCst);
            h.port.fail_start.store(fail_start, Ordering::SeqCst);
            assert!(h.submit(intent.clone()).unwrap().await.unwrap().is_err());
            h.shutdown().await;
            assert_eq!(h.facade.effects.load(Ordering::SeqCst), 0);
        }
    }
}
#[tokio::test]
async fn cancel_and_status_bypass_pending_ack_and_full_general_capacity() {
    let mut h = Harness::new(&request("op"));
    h.port.hold_accept.store(true, Ordering::SeqCst);
    let mut replies = Vec::new();
    for n in 0..32 {
        let intent = request(&format!("op{n}"));
        *h.facade.live.lock().unwrap() = inspection(&intent);
        replies.push(h.submit(intent).unwrap());
        h.event("accept").await;
    }
    assert!(h.submit(request("overflow")).is_err());
    let view = h
        .service
        .query(
            h.service
                .authorize_query(
                    h.context.clone(),
                    LifecycleQuery::Restart(RestartQuery::Status),
                )
                .unwrap(),
        )
        .unwrap()
        .await
        .unwrap()
        .unwrap();
    let mut expected = status();
    if let LifecycleReply::Restart(RestartReply::Status(s)) = &mut expected {
        s.core_epoch = InstanceEpoch(7);
    }
    assert_eq!(view, expected);
    let cancel = LifecycleCommand::Restart(RestartCommand::Cancel(CancelRestart {
        restart_id: id("restart"),
        expected_core_epoch: InstanceEpoch(7),
        expected_revision: Revision(1),
        operation_id: id("cancel"),
    }));
    let cancelled = h.submit(cancel).unwrap().await.unwrap().unwrap();
    assert_eq!(cancelled.receipt().outcome, AdminOutcome::Cancelled);
    assert!(cancelled.receipt().audit_warning);
    h.port.release();
    for reply in replies {
        assert!(reply.await.unwrap().is_err());
    }
    h.shutdown().await;
    assert_eq!(h.facade.effects.load(Ordering::SeqCst), 0);
}
#[tokio::test]
async fn lost_ack_readback_and_caller_disconnect_never_repeat_effect() {
    let mut h = Harness::new(&request("once"));
    h.port.lose_accept_ack.store(true, Ordering::SeqCst);
    let rx = h.submit(request("once")).unwrap();
    drop(rx);
    h.event("finalize").await;
    h.shutdown().await;
    assert_eq!(h.facade.effects.load(Ordering::SeqCst), 1);
    assert_eq!(h.port.record("once").outcome, Some(AuditOutcome::Succeeded));
    // A second live registry sees the same durable row; it can only read back.
    let tasks = AreaTasks::new(tokio::runtime::Handle::current());
    let service = AdminService::new(
        AdminAudit::new(h.port.clone(), HostRestartGate::default()),
        tasks.clone(),
        InstanceEpoch(7),
    );
    service.enable_management();
    service.install_restart(h.facade.clone());
    service.install_owned_graphs(h.facade.clone());
    let a = service
        .authorize_request(h.context.clone(), request("once"))
        .unwrap();
    let replay = service.submit(a).unwrap().await.unwrap().unwrap();
    assert_eq!(replay.receipt().outcome, AdminOutcome::Succeeded);
    assert_eq!(h.facade.effects.load(Ordering::SeqCst), 1);
    tasks.request_stop();
    tasks.shutdown().await.unwrap();
}
#[tokio::test]
async fn stale_target_after_durable_accept_is_conflict_before_continuation() {
    let mut h = Harness::new(&request("stale"));
    h.port.hold_accept.store(true, Ordering::SeqCst);
    let rx = h.submit(request("stale")).unwrap();
    h.event("accept").await;
    {
        let mut current = h.facade.live.lock().unwrap();
        let AdminInspection::Restart(ref mut live) = *current else {
            panic!()
        };
        live.target_revision = Revision(3);
    }
    h.port.release();
    assert_eq!(rx.await.unwrap().unwrap_err().code, ErrorCode::Conflict);
    h.shutdown().await;
    assert_eq!(h.facade.effects.load(Ordering::SeqCst), 0);
    assert_eq!(
        h.port.record("stale").outcome,
        Some(AuditOutcome::AbortedBeforeEffect)
    );
}
#[tokio::test]
async fn outcome_record_failure_is_unknown_and_not_reexecuted() {
    let mut h = Harness::new(&request("unknown"));
    h.port.fail_finalize.store(true, Ordering::SeqCst);
    h.submit(request("unknown"))
        .unwrap()
        .await
        .unwrap()
        .unwrap();
    h.event("finalize").await;
    h.shutdown().await;
    assert_eq!(h.facade.effects.load(Ordering::SeqCst), 1);
    assert_eq!(h.port.record("unknown").state, AuditState::Started);
}
#[tokio::test(start_paused = true)]
async fn response_budget_never_releases_a_still_owned_writer_command() {
    let (port, mut events) = Port::new();
    port.hold_accept.store(true, Ordering::SeqCst);
    let gate = HostRestartGate::default();
    let tasks = AreaTasks::new(tokio::runtime::Handle::current());
    let service = AdminService::new(
        AdminAudit::new(port.clone(), gate.clone()),
        tasks.clone(),
        InstanceEpoch(7),
    );
    service.enable_management();
    let intent = request("budget");
    let facade = Facade::new(&intent);
    service.install_restart(facade.clone());
    service.install_owned_graphs(facade.clone());
    let (proof, _lease) = test_support::local();
    let context = IngressContext::new(
        Provenance::human(Surface::Cli, ClientId::new("cli").unwrap()),
        proof,
    );
    let rx = service
        .submit(service.authorize_request(context, intent).unwrap())
        .unwrap();
    assert_eq!(events.recv().await, Some("read"));
    assert_eq!(events.recv().await, Some("accept"));
    gate.transition(1, HostRestartPhase::Sealing);
    facade.cancelled.store(true, Ordering::SeqCst);
    tokio::time::advance(std::time::Duration::from_secs(10)).await;
    assert_eq!(events.recv().await, Some("read"));
    let _ = rx.await.unwrap();
    let mut drained = Box::pin(gate.drained());
    assert!(
        std::future::poll_fn(|cx| std::task::Poll::Ready(drained.as_mut().poll(cx).is_pending()))
            .await
    );
    port.release();
    drained.await;
    tasks.request_stop();
    tasks.shutdown().await.unwrap();
    assert_eq!(facade.effects.load(Ordering::SeqCst), 0);
}

impl OperationReader for Port {
    fn operation(&self, id: &str) -> Result<Option<AuditRecord>, StorageError> {
        Ok(self.memory.lock().unwrap().rows.get(id).cloned())
    }
}

#[tokio::test]
async fn operation_readback_hides_foreign_existence_and_unknown_never_reexecutes() {
    let mut h = Harness::new(&request("private"));
    h.port.fail_finalize.store(true, Ordering::SeqCst);
    h.submit(request("private"))
        .unwrap()
        .await
        .unwrap()
        .unwrap();
    h.event("finalize").await;
    h.shutdown().await;
    let tasks = AreaTasks::new(tokio::runtime::Handle::current());
    let service = AdminService::new(
        AdminAudit::new(h.port.clone(), HostRestartGate::default()),
        tasks.clone(),
        InstanceEpoch(7),
    );
    service.enable_management();
    service.install_operation_reader(h.port.clone());
    let mut services = workbench_host::bootstrap::services::BackgroundServices::default();
    service.register_worker(&mut services);
    let query = LifecycleQuery::OperationStatus {
        operation_id: id("private"),
    };
    let own = service
        .query(
            service
                .authorize_query(h.context.clone(), query.clone())
                .unwrap(),
        )
        .unwrap()
        .await
        .unwrap()
        .unwrap();
    let LifecycleReply::OperationStatus(receipt) = own else {
        panic!()
    };
    assert_eq!(receipt.state, AdminOperationState::Unknown);
    assert_eq!(receipt.outcome, AdminOutcome::Unknown);
    assert!(receipt.audit_warning);
    let wire = serde_json::to_string(&receipt).unwrap();
    assert!(!wire.contains("principal"));
    assert!(!wire.contains("subject"));
    assert!(!wire.contains("target"));
    let (principal, _lease) = test_support::web(
        "different-human",
        tokio::time::Instant::now() + std::time::Duration::from_secs(60),
    );
    let foreign = IngressContext::new(
        Provenance::human(Surface::Web, ClientId::new("foreign").unwrap()),
        workbench_authority::TransportAuthority::from_web_context(Some(&principal)),
    );
    let denied = service
        .query(
            service
                .authorize_query(foreign.clone(), query.clone())
                .unwrap(),
        )
        .unwrap()
        .await
        .unwrap()
        .unwrap_err();
    let missing = service
        .query(
            service
                .authorize_query(
                    foreign,
                    LifecycleQuery::OperationStatus {
                        operation_id: id("missing"),
                    },
                )
                .unwrap(),
        )
        .unwrap()
        .await
        .unwrap()
        .unwrap_err();
    assert_eq!(denied, missing);
    assert_eq!(denied.code, ErrorCode::NotFound);
    let forged = IngressContext::unverified(h.context.provenance().clone());
    assert!(service.authorize_query(forged, query.clone()).is_err());
    let agent = IngressContext::new(
        Provenance::agent(RunId::new("agent").unwrap()),
        h.context.authority().clone(),
    );
    assert!(service.authorize_query(agent, query.clone()).is_err());
    assert!(
        service
            .authorize_query(
                IngressContext::system(h.context.provenance().clone()),
                query
            )
            .is_err()
    );
    assert_eq!(h.facade.effects.load(Ordering::SeqCst), 1);
    tasks.request_stop();
    tasks.shutdown().await.unwrap();
    services.shutdown().await.unwrap();
}

#[tokio::test]
async fn registered_continuation_survives_commit_error_or_panic() {
    for mode in [1, 2] {
        let mut h = Harness::new(&request("registered"));
        h.facade.after_register.store(mode, Ordering::SeqCst);
        let accepted = h
            .submit(request("registered"))
            .unwrap()
            .await
            .unwrap()
            .unwrap();
        assert_eq!(accepted.receipt().state, AdminOperationState::Started);
        h.event("finalize").await;
        h.shutdown().await;
        assert_eq!(h.facade.effects.load(Ordering::SeqCst), 1);
        assert_eq!(
            h.port.record("registered").outcome,
            Some(AuditOutcome::Succeeded)
        );
    }
}
#[tokio::test]
async fn every_interrupt_binding_is_rechecked_after_audit_before_effect() {
    let binding = InterruptBinding {
        restart_id: id("restart"),
        core_epoch: InstanceEpoch(7),
        connection_epoch: InstanceEpoch(3),
        run_incarnation: id("inc"),
        blocker_identity: id("opaque"),
        revision: Revision(1),
        phase: RestartPhase::WaitingSafePoint,
    };
    for field in 0..7 {
        let intent = LifecycleCommand::Restart(RestartCommand::Interrupt(InterruptShell {
            binding: binding.clone(),
            operation_id: id("stop"),
            confirmed: true,
        }));
        let mut h = Harness::new(&intent);
        h.port.hold_accept.store(true, Ordering::SeqCst);
        let rx = h.submit(intent).unwrap();
        h.event("accept").await;
        {
            let mut live = h.facade.live.lock().unwrap();
            let AdminInspection::Interrupt(b) = &mut *live else {
                panic!()
            };
            match field {
                0 => b.restart_id = id("different"),
                1 => b.core_epoch = InstanceEpoch(8),
                2 => b.connection_epoch = InstanceEpoch(4),
                3 => b.run_incarnation = id("reused-run"),
                4 => b.blocker_identity = id("replacement-job"),
                5 => b.revision = Revision(2),
                _ => b.phase = RestartPhase::PreparingDrains,
            }
        }
        h.port.release();
        assert_eq!(rx.await.unwrap().unwrap_err().code, ErrorCode::Conflict);
        h.shutdown().await;
        assert_eq!(h.facade.effects.load(Ordering::SeqCst), 0);
    }
}
#[tokio::test]
async fn reused_operation_id_with_different_contents_conflicts_without_second_effect() {
    let mut h = Harness::new(&request("idempotent"));
    h.submit(request("idempotent"))
        .unwrap()
        .await
        .unwrap()
        .unwrap();
    h.event("finalize").await;
    let mut changed = request("idempotent");
    let LifecycleCommand::Restart(RestartCommand::Request(ref mut r)) = changed else {
        panic!()
    };
    r.binding.target_id = id("other-configured-id");
    *h.facade.live.lock().unwrap() = inspection(&changed);
    assert_eq!(
        h.submit(changed).unwrap().await.unwrap().unwrap_err().code,
        ErrorCode::Conflict
    );
    h.shutdown().await;
    assert_eq!(h.facade.effects.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn missing_owned_collection_and_old_epoch_fail_before_audit() {
    let h = Harness::new(&request("epoch"));
    let mut old = request("epoch");
    let LifecycleCommand::Restart(RestartCommand::Request(ref mut r)) = old else {
        panic!()
    };
    r.binding.expected_core_epoch = InstanceEpoch(6);
    assert!(h.submit(old).is_err());
    assert!(h.port.memory.lock().unwrap().rows.is_empty());
    h.shutdown().await;
    let (port, _events) = Port::new();
    let tasks = AreaTasks::new(tokio::runtime::Handle::current());
    let service = AdminService::new(
        AdminAudit::new(port.clone(), HostRestartGate::default()),
        tasks.clone(),
        InstanceEpoch(7),
    );
    service.enable_management();
    service.install_restart(Facade::new(&request("missing")));
    let (proof, _lease) = test_support::local();
    let context = IngressContext::new(
        Provenance::human(Surface::Cli, ClientId::new("cli").unwrap()),
        proof,
    );
    let result = service
        .submit(
            service
                .authorize_request(context, request("missing"))
                .unwrap(),
        )
        .unwrap()
        .await
        .unwrap();
    assert!(result.is_err());
    assert!(port.memory.lock().unwrap().rows.is_empty());
    tasks.request_stop();
    tasks.shutdown().await.unwrap();
}

fn owned_request() -> owned_graphs::ConfirmedOwnedGraph {
    let graph: GraphCas = serde_json::from_value(serde_json::json!({"graph_id":"g","restart_tx_id":"tx","capture_id":"cap","expected_revision":"1","expected_core_epoch":"7","owner_identity":null,"owner_generation":null,"operation_id":"owned-op"})).unwrap();
    let mut c = owned(graph);
    c.binding.owners = vec![
        owned_graphs::OwnerCas {
            thread_ref: id("thread-a"),
            owner_identity: None,
            owner_generation: None,
        },
        owned_graphs::OwnerCas {
            thread_ref: id("thread-b"),
            owner_identity: Some(id("self")),
            owner_generation: Some(Revision(7)),
        },
    ];
    c
}
#[test]
fn owned_inspection_compares_every_thread_absence_and_control_expectation() {
    let c = owned_request();
    c.validate().unwrap();
    for resume in [false, true] {
        let intent = LifecycleCommand::OwnedGraphs(if resume {
            owned_graphs::OwnedGraphsCommand::Resume(c.clone())
        } else {
            owned_graphs::OwnedGraphsCommand::Release(c.clone())
        });
        let expected = c.binding.clone();
        assert!(AdminInspection::OwnedGraph(expected.clone()).matches(&intent));
        let mut changed = expected.clone();
        changed.expected_control_revision = Revision(1);
        assert!(!AdminInspection::OwnedGraph(changed).matches(&intent));
        let mut changed = expected.clone();
        changed.owners.pop();
        assert!(!AdminInspection::OwnedGraph(changed).matches(&intent));
        let mut changed = expected.clone();
        changed.owners[1].owner_generation = Some(Revision(8));
        assert!(!AdminInspection::OwnedGraph(changed).matches(&intent));
        let mut changed = expected.clone();
        changed.owners[0].owner_identity = Some(id("self"));
        changed.owners[0].owner_generation = Some(Revision(7));
        assert!(!AdminInspection::OwnedGraph(changed).matches(&intent));
        let mut changed = expected;
        changed.graph.capture_id = id("new-cap");
        assert!(!AdminInspection::OwnedGraph(changed).matches(&intent));
    }
}
#[tokio::test]
async fn owned_same_id_is_readback_before_inspection_even_after_control_and_owner_advance() {
    for resume in [false, true] {
        let c = owned_request();
        let intent = LifecycleCommand::OwnedGraphs(if resume {
            owned_graphs::OwnedGraphsCommand::Resume(c.clone())
        } else {
            owned_graphs::OwnedGraphsCommand::Release(c.clone())
        });
        let mut h = Harness::new(&intent);
        h.submit(intent.clone()).unwrap().await.unwrap().unwrap();
        h.event("finalize").await;
        let mut next = c.binding.clone();
        next.expected_control_revision = Revision(8);
        next.owners[1].owner_generation = Some(Revision(8));
        *h.facade.live.lock().unwrap() = AdminInspection::OwnedGraph(next);
        let got = h.submit(intent).unwrap().await.unwrap().unwrap();
        assert_eq!(got.receipt().state, AdminOperationState::Completed);
        assert_eq!(h.facade.inspections.load(Ordering::SeqCst), 1);
        let mut changed = c;
        changed.binding.expected_control_revision = Revision(9);
        assert_eq!(
            h.submit(LifecycleCommand::OwnedGraphs(
                owned_graphs::OwnedGraphsCommand::Release(changed)
            ))
            .unwrap()
            .await
            .unwrap()
            .unwrap_err()
            .code,
            ErrorCode::Conflict
        );
        assert_eq!(h.facade.inspections.load(Ordering::SeqCst), 1);
        assert_eq!(h.facade.effects.load(Ordering::SeqCst), 1);
        h.shutdown().await;
    }
}

#[tokio::test]
async fn only_a_bound_request_can_defer_finalization_to_recovery_and_all_release_slots() {
    for (intent, bind, deferred) in [
        (request("bound-request"), true, true),
        (request("unbound-request"), false, false),
        (
            LifecycleCommand::OwnedGraphs(owned_graphs::OwnedGraphsCommand::Release(owned(
                GraphCas {
                    graph_id: id("g"),
                    restart_tx_id: id("tx"),
                    capture_id: id("cap"),
                    expected_revision: Revision(1),
                    expected_core_epoch: InstanceEpoch(7),
                    owner_identity: None,
                    owner_generation: None,
                    operation_id: id("other-bound"),
                },
            ))),
            true,
            false,
        ),
    ] {
        let mut h = Harness::new(&intent);
        h.facade.await_recovery.store(true, Ordering::SeqCst);
        h.facade.bind_recovery.store(bind, Ordering::SeqCst);
        let operation = intent.operation_id().as_str().to_owned();
        let accepted = h.submit(intent).unwrap().await.unwrap().unwrap();
        assert_eq!(accepted.receipt().state, AdminOperationState::Started);
        if !deferred {
            h.event("finalize").await;
        }
        h.service.wait_for_request_slots().await;
        assert_eq!(h.service.available_admission_slots(), (64, 32));
        h.shutdown().await;
        let row = h.port.record(&operation);
        assert_eq!(row.restart_id.is_some(), bind);
        if deferred {
            assert_eq!(row.state, AuditState::Started);
            assert_eq!(row.outcome, None);
        } else {
            assert_eq!(row.state, AuditState::Finished);
            assert_eq!(row.outcome, Some(AuditOutcome::Unknown));
        }
        assert_eq!(h.service.available_admission_slots(), (64, 32));
        assert_eq!(h.facade.effects.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn service_passes_actual_connection_to_inspect_commit_query_and_fast_status() {
    let intent = request("transport-context");
    let h = Harness::new(&intent);
    let epoch = h.context.authority().connection().unwrap().epoch();
    h.submit(intent).unwrap().await.unwrap().unwrap();
    for query in [RestartQuery::Targets, RestartQuery::Status] {
        h.service
            .query(
                h.service
                    .authorize_query(h.context.clone(), LifecycleQuery::Restart(query))
                    .unwrap(),
            )
            .unwrap()
            .await
            .unwrap()
            .unwrap();
    }
    h.service.wait_for_request_slots().await;
    let observed = h.facade.connections.lock().unwrap().clone();
    for stage in ["inspect", "commit", "query", "status"] {
        assert!(
            observed.contains(&(stage, epoch)),
            "missing verified context at {stage}"
        );
    }
    assert!(observed.iter().all(|(_, got)| *got == epoch));
    h.shutdown().await;
}
fn every_command() -> Vec<AdminIntent> {
    let graph = GraphCas {
        graph_id: id("g"),
        restart_tx_id: id("tx"),
        capture_id: id("cap"),
        expected_revision: Revision(1),
        expected_core_epoch: InstanceEpoch(7),
        owner_identity: None,
        owner_generation: None,
        operation_id: id("op"),
    };
    vec![
        request("restart"),
        LifecycleCommand::Restart(RestartCommand::Cancel(CancelRestart {
            restart_id: id("restart"),
            expected_core_epoch: InstanceEpoch(7),
            expected_revision: Revision(1),
            operation_id: id("cancel"),
        })),
        LifecycleCommand::Restart(RestartCommand::Interrupt(InterruptShell {
            binding: InterruptBinding {
                restart_id: id("restart"),
                core_epoch: InstanceEpoch(7),
                connection_epoch: InstanceEpoch(3),
                run_incarnation: id("inc"),
                blocker_identity: id("opaque"),
                revision: Revision(1),
                phase: RestartPhase::WaitingSafePoint,
            },
            operation_id: id("interrupt"),
            confirmed: true,
        })),
        LifecycleCommand::Quarantine(quarantine::QuarantineCommand::Discard(ConfirmedGraph {
            binding: graph.clone(),
            confirmed: true,
        })),
        LifecycleCommand::OwnedGraphs(owned_graphs::OwnedGraphsCommand::Release(owned(
            graph.clone(),
        ))),
        LifecycleCommand::OwnedGraphs(owned_graphs::OwnedGraphsCommand::Resume(owned(graph))),
    ]
}
fn every_query() -> Vec<AdminQuery> {
    vec![
        LifecycleQuery::Restart(RestartQuery::Targets),
        LifecycleQuery::Restart(RestartQuery::Status),
        LifecycleQuery::Quarantine(quarantine::QuarantineQuery::List),
        LifecycleQuery::OwnedGraphs(owned_graphs::OwnedGraphsQuery::Undisposed),
    ]
}
/// The publication switch is off by default: a verified local human is refused
/// by all nine operations at the first step, before any leaf, audit row, gate,
/// transaction or owner is touched.
#[tokio::test]
async fn unpublished_management_refuses_all_nine_operations_without_side_effects() {
    let seed = request("seed");
    let mut off = Harness::with_switch(&seed, false);
    assert!(!off.service.management_enabled());
    // Authorized requests and queries can only come from a published service;
    // the unpublished one must still refuse to run them.
    let on = Harness::new(&seed);
    let commands = every_command();
    let queries = every_query();
    let operations: Vec<_> = commands
        .iter()
        .map(|c| c.action())
        .chain(queries.iter().filter_map(|q| q.action()))
        .collect();
    for action in AdminAction::ALL {
        assert!(operations.contains(&action), "{action:?} is not exercised");
    }
    let refused = |error: AdminFailure| {
        assert_eq!(error.code, ErrorCode::UnsupportedCapability);
        assert_eq!(error.message, "management is not published");
    };
    for action in AdminAction::ALL {
        refused(off.service.authorize(&off.context, action).unwrap_err());
        assert!(on.service.authorize(&on.context, action).is_ok());
    }
    for intent in commands {
        refused(
            off.service
                .authorize_request(off.context.clone(), intent.clone())
                .err()
                .unwrap(),
        );
        let authorized = on
            .service
            .authorize_request(on.context.clone(), intent)
            .unwrap();
        refused(off.service.submit(authorized).err().unwrap());
    }
    for query in queries {
        refused(
            off.service
                .authorize_query(off.context.clone(), query.clone())
                .err()
                .unwrap(),
        );
        let authorized = on
            .service
            .authorize_query(on.context.clone(), query)
            .unwrap();
        refused(off.service.query(authorized).err().unwrap());
    }
    // The operation-status query is refused too.
    let status = LifecycleQuery::OperationStatus {
        operation_id: id("seed"),
    };
    refused(
        off.service
            .authorize_query(off.context.clone(), status)
            .err()
            .unwrap(),
    );
    // Unverified callers are refused by the switch as well: it comes first.
    let forged = Provenance::human(Surface::Web, ClientId::new("claimed-human").unwrap());
    refused(
        off.service
            .authorize(&IngressContext::unverified(forged), AdminAction::Request)
            .unwrap_err(),
    );
    assert_eq!(off.facade.inspections.load(Ordering::SeqCst), 0);
    assert_eq!(off.facade.effects.load(Ordering::SeqCst), 0);
    assert!(!off.facade.cancelled.load(Ordering::SeqCst));
    assert!(off.facade.connections.lock().unwrap().is_empty());
    assert!(off.port.memory.lock().unwrap().rows.is_empty());
    assert!(off.events.try_recv().is_err());
    assert_eq!(off.service.denial_persistence_failures(), 0);
    off.shutdown().await;
    on.shutdown().await;
}
/// Registering the leaves is not publication.
#[tokio::test]
async fn installing_leaves_does_not_publish_management() {
    let intent = request("installed");
    let off = Harness::with_switch(&intent, false);
    assert!(off.service.installed_facades().all());
    assert!(!off.service.management_enabled());
    assert!(off.submit(intent).is_err());
    off.shutdown().await;
}
