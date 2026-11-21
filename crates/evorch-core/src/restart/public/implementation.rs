//! Public adapter over the sole #710 lifecycle owner. Installation/capability is 5a-V's job.
//! Lock order: public state -> owned-graph admission guard -> core coordinator.
#[path = "local.rs"]
mod local;
#[path = "shell.rs"]
mod shell;
use super::{RestartCoordinator, RestartId};
use core_launcher::handoff::{
    CurrentElf,
    targets::{CaptureCheckedTarget, RestartTarget},
};
pub use local::{LocalTargets, Requirements};
use runtime::{RunId, quiesce::RecoveryRecipe};
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{Arc, Mutex},
};
use workbench_host::admin::*;
use workbench_protocol::{
    errors::{ErrorCode, RetryAdvice, WorkbenchError},
    namespaces::lifecycle::{
        AdminId, AdminOutcome, LifecycleCommand, LifecycleQuery, LifecycleReply,
        restart::{
            CancelRestart, InterruptBinding, RestartBinding, RestartCommand,
            RestartManagementStatus, RestartQuery, RestartReply,
            RestartTarget as RestartTargetView, RestartTargets,
        },
    },
    wire::{InstanceEpoch, Revision},
};

pub type Recipes = BTreeMap<RunId, RecoveryRecipe>;
/// Recipes and the binding generation inspected by their source. No always-valid default.
pub struct ObservedRecipes {
    pub recipes: Recipes,
    pub bindings: workbench_host::lane::BindingObservation,
}
/// The integration supplies CURRENT recipes without changing CLI ownership policy.
/// Runtime preflight remains authoritative; this port cannot grant execution authority.
pub trait RecipeSource: Send + Sync + 'static {
    fn collect(&self) -> AdminIo<'_, ObservedRecipes>;
}
/// Hold the live Q2 collector's revision/empty-set lock through the supplied memory-only
/// transition. A read followed by an unlocked comparison is NOT an implementation.
/// No default/empty collector exists: unavailable Q2 wiring must fail closed.
pub trait UndisposedAdmission: Send + Sync + 'static {
    fn admit(
        &self,
        revision: Revision,
        transition: &mut dyn FnMut() -> Result<RestartId, AdminFailure>,
    ) -> Result<RestartId, AdminFailure>;
}
/// Adapter over the SAME host collector as the Q2 aggregate. No storage access.
pub struct LiveUndisposedAdmission {
    pub collector: Arc<workbench_host::bootstrap::recovery_disposal::LiveGraphCollector>,
    pub core: RestartCoordinator,
    pub epoch: InstanceEpoch,
}
impl UndisposedAdmission for LiveUndisposedAdmission {
    fn admit(
        &self,
        revision: Revision,
        transition: &mut dyn FnMut() -> Result<RestartId, AdminFailure>,
    ) -> Result<RestartId, AdminFailure> {
        self.collector
            .admit(self.epoch, revision, transition, |id| {
                let id = *id;
                let core = Arc::downgrade(&self.core.0);
                Arc::new(move || {
                    core.upgrade().is_some_and(|core| {
                        // Canonical reopened() clears id but retains generation.
                        // Comparing the old id to snapshot.id would never release.
                        core.owner == id.owner
                            && core.state.lock().is_ok_and(|state| {
                                state.generation == id.generation
                                    && state.id.is_none()
                                    && state.phase == super::RestartPhase::Running
                            })
                    })
                })
            })
    }
}
/// V owns BootstrappedHost, transaction creation and the exec environment. This port
/// must create/read back a Preparing tx and invoke the existing start_finalization;
/// never implement another capture boundary or restart clock. It receives only #710's
/// capture-checked target. A failed/unknown result must not retry the effect.
pub trait FinalizationOwner: Send + Sync + 'static {
    fn finalize(
        &self,
        coordinator: RestartCoordinator,
        id: RestartId,
        current: Arc<CurrentElf>,
        target: CaptureCheckedTarget,
    ) -> AdminContinuation;
}
struct Prepared {
    binding: RestartBinding,
    target: RestartTarget,
    recipes: Recipes,
    preflight: runtime::RestartPreflight,
    bindings: workbench_host::lane::BindingObservation,
}
#[derive(Clone)]
struct Active {
    binding: RestartBinding,
    id: RestartId,
}
#[derive(Default)]
struct PublicState {
    // At most the common continuation capacity. Superseded inspections may conflict,
    // but can never execute a different plan or exhaust an unbounded registry.
    prepared: VecDeque<Prepared>,
    active: Option<Active>,
    cancelled: Option<CancelRestart>,
}
/// Does not register/advertise SafeRestart. Common AdminService owns authorization,
/// accepted/started auditing, operation readback and detached continuations.
pub struct PublicRestartCoordinator {
    core: RestartCoordinator,
    epoch: InstanceEpoch,
    targets: Arc<LocalTargets>,
    recipes: Arc<dyn RecipeSource>,
    graphs: Arc<dyn UndisposedAdmission>,
    finalization: Arc<dyn FinalizationOwner>,
    state: Arc<Mutex<PublicState>>,
}
impl PublicRestartCoordinator {
    pub fn new(
        core: RestartCoordinator,
        epoch: InstanceEpoch,
        targets: Arc<LocalTargets>,
        recipes: Arc<dyn RecipeSource>,
        graphs: Arc<dyn UndisposedAdmission>,
        finalization: Arc<dyn FinalizationOwner>,
    ) -> Self {
        Self {
            core,
            epoch,
            targets,
            recipes,
            graphs,
            finalization,
            state: Arc::default(),
        }
    }
    fn check_request(&self, binding: &RestartBinding) -> Result<(), AdminFailure> {
        if binding.expected_core_epoch != self.epoch || !self.targets.matches(binding) {
            return Err(conflict());
        }
        Ok(())
    }
    fn canonical(
        &self,
        state: &PublicState,
        binding: &RestartBinding,
    ) -> Result<Option<RestartId>, AdminFailure> {
        let snapshot = self.core.snapshot();
        let Some(active) = &state.active else {
            return Ok(None);
        };
        if snapshot.id != Some(active.id) || snapshot.phase == super::RestartPhase::Running {
            return Ok(None);
        }
        if same_conditions(&active.binding, binding) {
            Ok(Some(active.id))
        } else {
            Err(conflict())
        }
    }
    async fn inspect_request(
        &self,
        binding: &RestartBinding,
    ) -> Result<AdminInspection, AdminFailure> {
        // Strictly reload even for a new operation joining an existing request. ACK
        // resends of the same operation are handled by common readback before inspect.
        let _ = self.targets.targets(self.epoch).await?;
        self.check_request(binding)?;
        if self
            .canonical(&self.state.lock().unwrap(), binding)?
            .is_some()
        {
            return Ok(AdminInspection::Restart(binding.clone()));
        }
        let target = self.targets.prepare(binding).await?;
        let runtime = self.core.0.runtime.clone();
        let recipe_observation = runtime.observe_restart_recipes();
        let observed = self.recipes.collect().await?;
        if !observed.bindings.is_current() {
            return Err(conflict());
        }
        let recipes = observed.recipes;
        let (recipes, preflight) = tokio::task::spawn_blocking(move || {
            let proof = runtime
                .prepare_restart_quiesce_observed(&recipes, recipe_observation)
                .map_err(|_| conflict())?;
            Ok::<_, AdminFailure>((recipes, proof))
        })
        .await
        .map_err(|_| conflict())??;
        if !observed.bindings.is_current() {
            return Err(conflict());
        }
        let mut state = self.state.lock().unwrap();
        if state.prepared.len() == 32 {
            state.prepared.pop_front();
        }
        state
            .prepared
            .retain(|p| p.binding.operation_id != binding.operation_id);
        state.prepared.push_back(Prepared {
            binding: binding.clone(),
            target,
            recipes,
            preflight,
            bindings: observed.bindings,
        });
        Ok(AdminInspection::Restart(binding.clone()))
    }
    fn commit_request(
        &self,
        binding: RestartBinding,
        admission: CommitAdmission,
    ) -> Result<AdminAccepted, AdminFailure> {
        self.check_request(&binding)?;
        let mut state = self.state.lock().unwrap();
        if let Some(id) = self.canonical(&state, &binding)? {
            let audit = admission.restart_audit();
            return Ok(admission.register(Box::pin(async move {
                audit.bind(&public_id(id)?).await?;
                Ok(AdminExecutionOutcome::AwaitRestartRecovery)
            })));
        }
        let position = state
            .prepared
            .iter()
            .position(|p| p.binding == binding)
            .ok_or_else(conflict)?;
        let prepared = state.prepared.remove(position).unwrap();
        let revision = admission.undisposed_revision().ok_or_else(conflict)?;
        // Keep the existing #710 host drain open until the durable restart-ID
        // mapping is acknowledged. Otherwise a fast empty runtime could seal
        // before this continuation gets its first poll, rejecting its own audit.
        let mapping_admission = self
            .core
            .0
            .gate
            .enter_admin_audit()
            .map_err(|_| conflict())?;
        let mut recipes = Some(prepared.recipes);
        let timeout = prepared.target.timeout();
        let id = self.graphs.admit(revision, &mut || {
            let recipes = recipes.take().ok_or_else(conflict)?;
            if !prepared.bindings.is_current() {
                return Err(conflict());
            }
            self.core
                .0
                .runtime
                .validate_restart_preflight(&prepared.preflight, &recipes)
                .map_err(|_| conflict())?;
            self.core
                .request_inner(Some(recipes), timeout)
                .map_err(|_| conflict())
        })?;
        state.active = Some(Active { binding, id });
        state.cancelled = None;
        let core = self.core.clone();
        let targets = self.targets.clone();
        let finalization = self.finalization.clone();
        let audit = admission.restart_audit();
        Ok(admission.register(Box::pin(async move {
            // Mapping failure must not leave an unauditable restart progressing.
            if let Err(error) = audit.bind(&public_id(id)?).await {
                let _ = core.cancel(id);
                return Err(error);
            }
            drop(mapping_admission);
            if core.wait_sealed(id).await.is_err() {
                return Ok(AdminExecutionOutcome::Complete(AdminOutcome::Cancelled));
            }
            let target = match prepared
                .target
                .before_capture(
                    &targets.current,
                    targets.limits,
                    cancelled(core.clone(), id),
                )
                .await
            {
                Ok(target) => target,
                Err(_) => {
                    let _ = core.cancel(id);
                    return Ok(AdminExecutionOutcome::Complete(AdminOutcome::Failed));
                }
            };
            finalization
                .finalize(core, id, targets.current.clone(), target)
                .await
        })))
    }
}
impl AdminFacade for PublicRestartCoordinator {
    fn inspect<'a>(
        &'a self,
        intent: &'a AdminIntent,
        connection: &'a VerifiedConnection,
    ) -> AdminIo<'a, AdminInspection> {
        Box::pin(async move {
            supported()?;
            intent.validate().map_err(|_| invalid())?;
            if !connection.matches(connection.epoch()) {
                return Err(conflict());
            }
            match intent {
                LifecycleCommand::Restart(RestartCommand::Request(request)) => {
                    self.inspect_request(&request.binding).await
                }
                LifecycleCommand::Restart(RestartCommand::Interrupt(request)) => {
                    self.inspect_interrupt(&request.binding, connection)?;
                    Ok(AdminInspection::Interrupt(request.binding.clone()))
                }
                _ => Err(invalid()),
            }
        })
    }
    fn commit(
        &self,
        intent: AdminIntent,
        expected: AdminInspection,
        admission: CommitAdmission,
        connection: &VerifiedConnection,
    ) -> Result<AdminAccepted, AdminFailure> {
        supported()?;
        if !expected.matches(&intent)
            || intent.validate().is_err()
            || !connection.matches(admission.connection().epoch())
        {
            return Err(conflict());
        }
        match intent {
            LifecycleCommand::Restart(RestartCommand::Request(request)) => {
                self.commit_request(request.binding, admission)
            }
            LifecycleCommand::Restart(RestartCommand::Interrupt(request)) => {
                self.commit_interrupt(request.binding, admission, connection)
            }
            _ => Err(invalid()),
        }
    }
    fn cancel(&self, intent: &AdminIntent) -> Result<AdminOutcome, AdminFailure> {
        supported()?;
        let LifecycleCommand::Restart(RestartCommand::Cancel(request)) = intent else {
            return Err(invalid());
        };
        if request.expected_core_epoch != self.epoch {
            return Err(conflict());
        }
        let mut state = self.state.lock().unwrap();
        if state.cancelled.as_ref() == Some(request) {
            return Ok(AdminOutcome::Cancelled);
        }
        let id = state
            .active
            .as_ref()
            .filter(|a| public_id(a.id).as_ref() == Ok(&request.restart_id))
            .map(|a| a.id)
            .ok_or_else(conflict)?;
        self.core
            .cancel_checked(id, Some(request.expected_revision.get()))
            .map_err(|_| conflict())?;
        state.cancelled = Some(request.clone());
        Ok(AdminOutcome::Cancelled)
    }
    fn status(&self, connection: &VerifiedConnection) -> Result<AdminView, AdminFailure> {
        if !connection.matches(connection.epoch()) {
            return Err(conflict());
        }
        Ok(LifecycleReply::Restart(RestartReply::Status(
            RestartManagementStatus {
                audit: Default::default(),
                core_epoch: self.epoch,
                status: self.core.snapshot().to_dto(),
            },
        )))
    }
    fn query<'a>(
        &'a self,
        query: &'a AdminQuery,
        connection: &'a VerifiedConnection,
    ) -> AdminIo<'a, AdminView> {
        Box::pin(async move {
            if !connection.matches(connection.epoch()) {
                return Err(conflict());
            }
            let reply = match query {
                LifecycleQuery::Restart(RestartQuery::Targets) => {
                    RestartReply::Targets(self.targets.targets(self.epoch).await?)
                }
                LifecycleQuery::Restart(RestartQuery::Status) => return self.status(connection),
                LifecycleQuery::Restart(RestartQuery::ShellBlockers) => {
                    RestartReply::ShellBlockers(self.shell_blockers(connection)?)
                }
                _ => return Err(invalid()),
            };
            reply.validate().map_err(|_| failed())?;
            Ok(LifecycleReply::Restart(reply))
        })
    }
}
fn same_conditions(a: &RestartBinding, b: &RestartBinding) -> bool {
    a.target_id == b.target_id
        && a.expected_core_epoch == b.expected_core_epoch
        && a.target_revision == b.target_revision
}
fn public_id(id: RestartId) -> Result<AdminId, AdminFailure> {
    Ok(id.public_id())
}
fn admin_id(s: &str) -> Result<AdminId, AdminFailure> {
    AdminId::new(s).map_err(|_| failed())
}
fn error(code: ErrorCode, message: &str) -> AdminFailure {
    WorkbenchError::new(code, message, RetryAdvice::Never)
}
fn conflict() -> AdminFailure {
    error(ErrorCode::Conflict, "restart binding changed")
}
fn invalid() -> AdminFailure {
    error(
        ErrorCode::InvalidRequest,
        "restart preflight or confirmation rejected",
    )
}
fn failed() -> AdminFailure {
    error(
        ErrorCode::Internal,
        "restart result unknown; do not repeat the effect",
    )
}
fn supported() -> Result<(), AdminFailure> {
    if cfg!(target_os = "linux") {
        Ok(())
    } else {
        Err(error(
            ErrorCode::UnsupportedCapability,
            "safe restart requires a supported Linux core",
        ))
    }
}
async fn cancelled(core: RestartCoordinator, id: RestartId) {
    let mut changed = core.subscribe();
    loop {
        let snapshot = core.snapshot();
        if snapshot.id != Some(id)
            || matches!(
                snapshot.phase,
                super::RestartPhase::FailedClosed | super::RestartPhase::ShuttingDown
            )
        {
            return;
        }
        if changed.changed().await.is_err() {
            return;
        }
    }
}
