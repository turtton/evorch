use core_launcher::handoff::{CurrentElf, HandoffDescription, ProbeLimits, SchemaRange};
use evorch_core::restart::{RestartCoordinator, RestartId, public::*};
use std::{
    os::unix::fs::PermissionsExt,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};
use storage::admin_audit::*;
use tokio::sync::mpsc;
use workbench_authority::{IngressContext, LocalConnection};
use workbench_host::{
    admin::*,
    areas::tasks::AreaTasks,
    bootstrap::{BootstrapOptions, BootstrappedHost, TestSeams, services::BackgroundServices},
};
use workbench_protocol::{
    envelope::{Provenance, Surface},
    errors::{ErrorCode, RetryAdvice, WorkbenchError},
    ids::ClientId,
    namespaces::lifecycle::{owned_graphs::*, restart::*, *},
    wire::{InstanceEpoch, Revision},
};
#[path = "../../shared-test-support/restart_model.rs"]
pub mod model;
pub const EPOCH: InstanceEpoch = InstanceEpoch(7);
pub fn id(s: &str) -> AdminId {
    AdminId::new(s).unwrap()
}
pub fn conflict() -> AdminFailure {
    WorkbenchError::new(ErrorCode::Conflict, "fixture conflict", RetryAdvice::Never)
}
#[derive(Default)]
pub struct RecipesSource(
    pub Mutex<Recipes>,
    pub workbench_host::lane::BindingTable,
    pub Mutex<Option<(Arc<tokio::sync::Barrier>, Arc<tokio::sync::Barrier>)>>,
);
impl RecipeSource for RecipesSource {
    fn collect(&self) -> AdminIo<'_, ObservedRecipes> {
        let bindings = self.1.observe();
        let recipes = self.0.lock().unwrap().clone();
        let pause = self.2.lock().unwrap().take();
        Box::pin(async move {
            if let Some((entered, release)) = pause {
                entered.wait().await;
                release.wait().await;
            }
            Ok(ObservedRecipes { recipes, bindings })
        })
    }
}
// The same lock covers the live revision and the supplied gate transition.
pub struct Graphs {
    pub state: Mutex<(Revision, u64)>,
    pub transition_hook: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}
impl Default for Graphs {
    fn default() -> Self {
        Self {
            state: Mutex::new((Revision(1), 0)),
            transition_hook: Mutex::new(None),
        }
    }
}
impl UndisposedAdmission for Graphs {
    fn admit(
        &self,
        revision: Revision,
        transition: &mut dyn FnMut() -> Result<RestartId, AdminFailure>,
    ) -> Result<RestartId, AdminFailure> {
        let live = self.state.lock().unwrap();
        if live.0 != revision || live.1 != 0 {
            return Err(conflict());
        }
        if let Some(hook) = self.transition_hook.lock().unwrap().take() {
            hook();
        }
        let result = transition();
        drop(live);
        result
    }
}
impl AdminFacade for Graphs {
    fn inspect<'a>(
        &'a self,
        _: &'a AdminIntent,
        _: &'a VerifiedConnection,
    ) -> AdminIo<'a, AdminInspection> {
        Box::pin(async { Err(conflict()) })
    }
    fn query<'a>(&'a self, _: &'a AdminQuery, _: &'a VerifiedConnection) -> AdminIo<'a, AdminView> {
        Box::pin(async {
            let live = self.state.lock().unwrap();
            Ok(LifecycleReply::OwnedGraphs(OwnedGraphsReply::Undisposed(
                UndisposedGraphs {
                    count: Revision(live.1),
                    references: vec![],
                    snapshot_revision: live.0,
                    truncated: false,
                    over_limit_count: Revision(0),
                    incomplete_disposals: IncompleteDisposals::empty(),
                },
            )))
        })
    }
    fn commit(
        &self,
        _: AdminIntent,
        _: AdminInspection,
        _: CommitAdmission,
        _: &VerifiedConnection,
    ) -> Result<AdminAccepted, AdminFailure> {
        Err(conflict())
    }
}
#[derive(Default)]
pub struct Finalizer {
    pub calls: AtomicUsize,
    pub reached: tokio::sync::Notify,
}
impl FinalizationOwner for Finalizer {
    fn finalize(
        &self,
        _: RestartCoordinator,
        _: RestartId,
        _: Arc<CurrentElf>,
        _: core_launcher::handoff::targets::CaptureCheckedTarget,
    ) -> AdminContinuation {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.reached.notify_one();
        Box::pin(async { Ok(AdminExecutionOutcome::AwaitRestartRecovery) })
    }
}
type Held = (
    AuditReplyCallback,
    Result<AuditReply, storage::StorageError>,
);
pub struct Port {
    writer: storage::StorageHandle,
    pub fail: AtomicUsize,
    pub hold: AtomicUsize,
    held: Arc<Mutex<Vec<Held>>>,
    events: mpsc::UnboundedSender<(usize, String)>,
}
impl AuditPort for Port {
    fn submit(
        &self,
        write: AuditWrite,
        reply: AuditReplyCallback,
    ) -> Result<(), storage::StorageError> {
        let operation = match &write {
            AuditWrite::Accept(a) => a.key.operation_id.clone(),
            AuditWrite::Start { key }
            | AuditWrite::BindRestart { key, .. }
            | AuditWrite::Finalize { key, .. } => key.operation_id.clone(),
            _ => String::new(),
        };
        let stage = match &write {
            AuditWrite::Accept(_) => 1,
            AuditWrite::Start { .. } => 2,
            AuditWrite::BindRestart { .. } => 3,
            AuditWrite::Finalize { .. } => 4,
            _ => 0,
        };
        if stage != 0 && self.fail.load(Ordering::SeqCst) == stage {
            let _ = self.events.send((stage, operation.clone()));
            return Err(storage::StorageError::Io("injected audit failure".into()));
        }
        let hold = stage != 0 && self.hold.load(Ordering::SeqCst) == stage;
        let held = self.held.clone();
        let events = self.events.clone();
        self.writer.try_submit_admin_audit(
            write,
            Box::new(move |result| {
                if hold {
                    held.lock().unwrap().push((reply, result));
                } else {
                    reply(result);
                }
                let _ = events.send((stage, operation));
            }),
        )
    }
}
impl Port {
    pub fn release(&self) {
        self.hold.store(0, Ordering::SeqCst);
        for (reply, result) in std::mem::take(&mut *self.held.lock().unwrap()) {
            reply(result);
        }
    }
}
pub struct Connection {
    pub context: IngressContext,
    pub verified: VerifiedConnection,
    pub lease: Option<LocalConnection>,
    _sockets: (tokio::net::UnixStream, tokio::net::UnixStream),
}
impl Connection {
    pub fn new() -> Self {
        let (a, b) = tokio::net::UnixStream::pair().unwrap();
        let lease = workbench_authority::verify_unix_peer(&a).unwrap();
        let authority = lease.authority();
        let verified = authority.connection().unwrap();
        let context = IngressContext::new(
            Provenance::human(Surface::Desktop, ClientId::new("restart-human").unwrap()),
            authority,
        );
        Self {
            context,
            verified,
            lease: Some(lease),
            _sockets: (a, b),
        }
    }
}
pub struct Fixture {
    pub dir: tempfile::TempDir,
    pub host: BootstrappedHost,
    pub core: RestartCoordinator,
    pub leaf: Arc<PublicRestartCoordinator>,
    pub service: Arc<AdminService>,
    pub graphs: Arc<Graphs>,
    pub recipes: Arc<RecipesSource>,
    pub finalizer: Arc<Finalizer>,
    pub port: Arc<Port>,
    pub connection: Connection,
    pub current: Arc<CurrentElf>,
    pub description: HandoffDescription,
    pub events: mpsc::UnboundedReceiver<(usize, String)>,
    tasks: AreaTasks,
    services: BackgroundServices,
}
impl Fixture {
    pub async fn new() -> Self {
        Self::with_model(Arc::new(model::Model::default())).await
    }
    pub async fn with_model(agent_model: Arc<dyn runtime::AgentModel>) -> Self {
        let dir = tempfile::tempdir_in("/tmp").unwrap();
        let root = dir.path().canonicalize().unwrap();
        let mut sidebar = workspace_state::SidebarState::default();
        sidebar
            .add_project(workspace_state::ProjectId::new("p"), "repo", &root)
            .unwrap();
        std::fs::write(
            root.join("sidebar.json"),
            serde_json::to_vec(&sidebar).unwrap(),
        )
        .unwrap();
        let mut host = BootstrappedHost::open_for_core_with_test_seams(
            BootstrapOptions {
                config_dir: root.clone(),
                project_root: root.clone(),
                ownership_root: root.join("o"),
            },
            TestSeams {
                model: agent_model,
                sandbox: Arc::new(model::Direct),
            },
            &evorch_core::recovery_service::CoreGraphRecovery,
        )
        .await
        .unwrap();
        static CURRENT: std::sync::OnceLock<Arc<CurrentElf>> = std::sync::OnceLock::new();
        let current = CURRENT
            .get_or_init(|| Arc::new(CurrentElf::record().unwrap()))
            .clone();
        let elf = root.join("described-elf");
        std::fs::copy("/bin/sh", &elf).unwrap();
        let description = HandoffDescription {
            version: 1,
            build_identity: core_instance::handoff::elf_identity(&elf).unwrap(),
            elf_path: elf,
            handoff_format: 1,
            snapshot_format: 1,
            schema: SchemaRange {
                minimum: 1,
                maximum: 999,
            },
            protocol: 1,
            platform: format!("linux-{}", std::env::consts::ARCH),
            tool_catalog: "fixture-catalog".into(),
        };
        let wrapper = root.join("target.sh");
        std::fs::write(
            &wrapper,
            format!(
                "#!/bin/sh\nprintf '%s\\n' '{}'\n",
                serde_json::to_string(&description)
                    .unwrap()
                    .replace('\'', "'\"'\"'")
            ),
        )
        .unwrap();
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::write(root.join("config.toml"), format!("[core.restart]\ntimeout_secs = 300\n[core.restart.targets.selected]\npath = {wrapper:?}\n[core.restart.targets.other]\npath = {wrapper:?}\n")).unwrap();
        let (events, rx) = mpsc::unbounded_channel();
        let port = Arc::new(Port {
            writer: host.composition.storage_writer.clone(),
            fail: AtomicUsize::new(0),
            hold: AtomicUsize::new(0),
            held: Arc::new(Mutex::new(vec![])),
            events,
        });
        let tasks = AreaTasks::new(tokio::runtime::Handle::current());
        let gate = host.host.lock().unwrap().service().restart_gate();
        let service = AdminService::new(AdminAudit::new(port.clone(), gate), tasks.clone(), EPOCH);
        service.publish_for_test();
        host.admin = service.clone();
        let core = RestartCoordinator::new(&host);
        let graphs = Arc::new(Graphs::default());
        let recipes = Arc::new(RecipesSource::default());
        let finalizer = Arc::new(Finalizer::default());
        let targets = Arc::new(LocalTargets::new(
            root,
            current.clone(),
            Requirements {
                handoff: 1,
                snapshot: 1,
                storage_schema: 62,
                protocol: 1,
                platform: description.platform.clone(),
            },
            ProbeLimits::default(),
        ));
        let leaf = Arc::new(PublicRestartCoordinator::new(
            core.clone(),
            EPOCH,
            targets,
            recipes.clone(),
            graphs.clone(),
            finalizer.clone(),
        ));
        service.install_restart(leaf.clone());
        service.install_owned_graphs(graphs.clone());
        service.install_operation_reader(Arc::new(host.composition.storage.clone()));
        let mut services = BackgroundServices::default();
        service.register_worker(&mut services);
        Self {
            dir,
            host,
            core,
            leaf,
            service,
            graphs,
            recipes,
            finalizer,
            port,
            connection: Connection::new(),
            current,
            description,
            events: rx,
            tasks,
            services,
        }
    }
    pub fn root(&self) -> &std::path::Path {
        self.dir.path()
    }
    pub fn submit(&self, intent: AdminIntent) -> Result<AdminReplyReceiver, AdminFailure> {
        self.submit_on(&self.connection, intent)
    }
    pub fn submit_on(
        &self,
        connection: &Connection,
        intent: AdminIntent,
    ) -> Result<AdminReplyReceiver, AdminFailure> {
        self.service.submit(
            self.service
                .authorize_request(connection.context.clone(), intent)?,
        )
    }
    pub async fn request(&self, op: &str) -> RequestRestart {
        let LifecycleReply::Restart(RestartReply::Targets(targets)) = self
            .leaf
            .query(
                &LifecycleQuery::Restart(RestartQuery::Targets),
                &self.connection.verified,
            )
            .await
            .unwrap()
        else {
            panic!()
        };
        RequestRestart {
            binding: RestartBinding {
                target_id: id("selected"),
                expected_core_epoch: EPOCH,
                target_revision: targets.revision,
                operation_id: id(op),
            },
            confirmed: true,
        }
    }
    pub fn cancel(&self, op: &str) -> AdminIntent {
        let snapshot = self.core.snapshot();
        LifecycleCommand::Restart(RestartCommand::Cancel(CancelRestart {
            restart_id: id(snapshot.to_dto().restart_id.as_ref().unwrap().as_str()),
            expected_core_epoch: EPOCH,
            expected_revision: Revision(snapshot.revision),
            operation_id: id(op),
        }))
    }
    pub async fn event(&mut self, stage: usize, operation: &str) {
        while self.events.recv().await.unwrap() != (stage, operation.to_owned()) {}
    }
    pub fn assert_running_untouched(&self) {
        let snapshot = self.core.snapshot();
        assert!(snapshot.id.is_none());
        assert_eq!(snapshot.revision, 0);
        assert_eq!(snapshot.phase, evorch_core::restart::RestartPhase::Running);
        assert!(
            self.host
                .host
                .lock()
                .unwrap()
                .service()
                .restart_gate()
                .enter(workbench_host::bootstrap::restart::CommandAccess::NewWork)
                .is_ok()
        );
        assert_eq!(self.finalizer.calls.load(Ordering::SeqCst), 0);
        assert!(
            storage::Database::open(&self.host.composition.storage)
                .unwrap()
                .restart_transactions()
                .unwrap()
                .is_empty()
        );
    }
    pub async fn shutdown(mut self) {
        if let Some(id) = self.core.snapshot().id {
            let _ = self.core.cancel(id);
        }
        self.tasks.request_stop();
        self.tasks.shutdown().await.unwrap();
        self.services.shutdown().await.unwrap();
        let failed_closed =
            self.core.snapshot().phase == evorch_core::restart::RestartPhase::FailedClosed;
        let result = self.host.shutdown().await;
        if failed_closed {
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("exec environment not owned")
            );
        } else {
            result.unwrap();
        }
    }
}
pub fn command(request: RequestRestart) -> AdminIntent {
    LifecycleCommand::Restart(RestartCommand::Request(request))
}
