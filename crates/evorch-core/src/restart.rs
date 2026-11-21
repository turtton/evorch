//! One process-local lifecycle owner. There is deliberately no wire/production request handler.
//! Lock order: coordinator -> host gate / runtime. Direct runtime Stop enters the
//! installed boundary before taking any runtime lock; runtime never calls back while locked.
pub mod public;
use runtime::{
    AgentRuntime, RunId, RuntimeError, StopScope,
    producer::{ProducerGate, ProducerPause},
    quiesce::{RestartAttempt, RestartBlocker},
};
use std::sync::{Arc, Mutex};
use tokio::sync::watch;
use workbench_host::bootstrap::{
    BootstrappedHost,
    restart::{HostRestartGate, HostRestartPhase, RestartStopHandler, SealedRestart},
};

mod finalization;
#[cfg(target_os = "linux")]
pub use finalization::FinalizationEnvironment;
pub use finalization::FinalizationTarget;
mod preparing;
mod stop;
const PREPARATION_LIMIT: std::time::Duration = std::time::Duration::from_secs(300);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RestartPhase {
    Running,
    Requested,
    PreparingDrains,
    WaitingSafePoint,
    Sealing,
    Sealed,
    Capturing,
    Finalizing,
    Cancelling,
    FailedClosed,
    ShuttingDown,
}
impl RestartPhase {
    pub const fn retains_process(self) -> bool {
        !matches!(self, Self::Running)
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RestartId {
    owner: uuid::Uuid,
    generation: u64,
}
impl RestartId {
    // Public management IDs use the 5a-I AdminId alphabet (no colon). This is
    // projection only; generation/attempt ownership remain in the #710 owner.
    pub(crate) fn public_id(self) -> workbench_protocol::namespaces::lifecycle::AdminId {
        workbench_protocol::namespaces::lifecycle::AdminId::new(format!(
            "{}.{}",
            self.owner, self.generation
        ))
        .expect("bounded internal restart ID")
    }
    pub fn generation(self) -> u64 {
        self.generation
    }
}
#[derive(Clone, Debug)]
pub struct RestartSnapshot {
    pub id: Option<RestartId>,
    pub phase: RestartPhase,
    pub revision: u64,
    pub blockers: Vec<RestartBlocker>,
    pub active_producers: usize,
}
struct State {
    #[cfg(target_os = "linux")]
    shutdown_watchdog: Option<crate::lifecycle::shutdown_watchdog::Observer>,
    id: Option<RestartId>,
    phase: RestartPhase,
    revision: u64,
    generation: u64,
    attempt: Option<RestartAttempt>,
    pauses: Vec<ProducerPause>,
    sealed: Option<SealedRestart>,
    recipes: Option<std::collections::BTreeMap<RunId, runtime::quiesce::RecoveryRecipe>>,
    finalization:
        Option<Arc<tokio::sync::Mutex<workbench_host::bootstrap::planned_exit::PlannedExit>>>,
    #[cfg(target_os = "linux")]
    recipe_source: Option<Arc<dyn public::RecipeSource>>,
    capture_started: bool,
    finalization_id: Option<RestartId>,
    finalization_done: bool,
    completed_stage: Option<workbench_host::bootstrap::planned_exit::ExitStage>,
    worker: Option<tokio::task::AbortHandle>,
    learning_pause: Option<runtime::learning_suspend::LearningPause>,
    suspension: Option<Arc<runtime::learning_suspend::LearningSuspension>>,
    evaluation_pause: Option<ProducerPause>,
    preparation_deadline: Option<tokio::time::Instant>,
}
#[cfg(test)]
type PreflightHook = Arc<dyn Fn(RestartId) + Send + Sync>;
struct Inner {
    owner: uuid::Uuid,
    runtime: AgentRuntime,
    gate: HostRestartGate,
    producers: Vec<ProducerGate>,
    preparing: workbench_host::bootstrap::restart::PreparingServices,
    state: Mutex<State>,
    changed: watch::Sender<u64>,
    #[cfg(test)]
    preflight_hook: Mutex<Option<PreflightHook>>,
    #[cfg(test)]
    after_host_drain: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}
/// A revocable observation, NOT the consumable capture capability. Keeping any
/// number of waiters alive cannot delay the coordinator's capture boundary.
pub struct SealedObservation {
    owner: std::sync::Weak<Inner>,
    id: RestartId,
}
impl SealedObservation {
    pub fn validate(&self, runtime: &AgentRuntime) -> Result<(), RuntimeError> {
        let inner = self.owner.upgrade().ok_or(RuntimeError::RestartBusy)?;
        let state = inner.state.lock().unwrap();
        if state.id != Some(self.id) {
            return Err(RuntimeError::RestartBusy);
        }
        state
            .sealed
            .as_ref()
            .ok_or(RuntimeError::RestartBusy)?
            .validate(runtime)
    }
}
#[derive(Clone)]
pub struct RestartCoordinator(Arc<Inner>);
impl RestartCoordinator {
    pub fn new(host: &BootstrappedHost) -> Self {
        // Collect composition before taking the host lock: it also reads host services.
        let producers = host.restart_producers();
        let mut command_host = host.host.lock().unwrap();
        if let Some(handler) = command_host.service().restart_stop_handler() {
            let any: Arc<dyn std::any::Any + Send + Sync> = handler;
            let existing = any
                .downcast::<Self>()
                .unwrap_or_else(|_| panic!("host already has another lifecycle owner"));
            return (*existing).clone();
        }
        let coordinator = Self::from_parts(
            host.runtime.clone(),
            command_host.service().restart_gate(),
            producers,
            host.restart_preparing_services(),
        );
        command_host
            .service_mut()
            .set_restart_stop_handler(Arc::new(coordinator.clone()));
        coordinator
    }
    fn from_parts(
        runtime: AgentRuntime,
        gate: HostRestartGate,
        producers: Vec<ProducerGate>,
        preparing: workbench_host::bootstrap::restart::PreparingServices,
    ) -> Self {
        let coordinator = Self(Arc::new(Inner {
            owner: uuid::Uuid::new_v4(),
            runtime,
            gate,
            producers,
            preparing,
            state: Mutex::new(State {
                #[cfg(target_os = "linux")]
                shutdown_watchdog: None,
                id: None,
                phase: RestartPhase::Running,
                revision: 0,
                generation: 0,
                attempt: None,
                pauses: vec![],
                sealed: None,
                recipes: None,
                finalization: None,
                #[cfg(target_os = "linux")]
                recipe_source: None,
                capture_started: false,
                finalization_id: None,
                finalization_done: false,
                completed_stage: None,
                worker: None,
                learning_pause: None,
                suspension: None,
                evaluation_pause: None,
                preparation_deadline: None,
            }),
            changed: watch::channel(0).0,
            #[cfg(test)]
            preflight_hook: Mutex::new(None),
            #[cfg(test)]
            after_host_drain: Mutex::new(None),
        }));
        coordinator
            .0
            .runtime
            .install_stop_boundary(Arc::new(stop::CoreStopBoundary(Arc::downgrade(
                &coordinator.0,
            ))))
            .expect("runtime already has another lifecycle owner");
        coordinator
    }
    #[cfg(target_os = "linux")]
    pub(crate) fn set_shutdown_watchdog(
        &self,
        observer: crate::lifecycle::shutdown_watchdog::Observer,
    ) -> std::io::Result<()> {
        let mut state = self.0.state.lock().unwrap();
        if state.shutdown_watchdog.is_some() || state.capture_started {
            return Err(std::io::Error::other(
                "watchdog must be installed once, before capture",
            ));
        }
        observer.installed_publisher();
        state.shutdown_watchdog = Some(observer);
        Ok(())
    }
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.0.changed.subscribe()
    }
    fn publish(&self, state: &mut State, phase: RestartPhase) {
        state.phase = phase;
        state.revision = state.revision.wrapping_add(1);
        self.0.changed.send_replace(state.revision);
    }
    pub fn snapshot(&self) -> RestartSnapshot {
        let state = self.0.state.lock().unwrap();
        RestartSnapshot {
            id: state.id,
            phase: state.phase,
            revision: state.revision,
            blockers: self.0.runtime.restart_blockers(),
            active_producers: self.0.producers.iter().map(ProducerGate::active).sum(),
        }
    }
    pub fn busy(&self) -> bool {
        self.0.state.lock().unwrap().phase.retains_process()
    }
    /// Joining or dropping a waiter never owns cancellation. The detached worker does.
    pub fn request(&self) -> Result<RestartId, RuntimeError> {
        self.request_inner(None, PREPARATION_LIMIT)
    }
    /// Internal planned path only: no wire handler/capability advertises restart.
    pub fn request_planned(
        &self,
        recipes: std::collections::BTreeMap<RunId, runtime::quiesce::RecoveryRecipe>,
    ) -> Result<RestartId, RuntimeError> {
        self.0.runtime.preflight_restart_capture(&recipes)?;
        self.request_inner(Some(recipes), PREPARATION_LIMIT)
    }
    fn request_inner(
        &self,
        recipes: Option<std::collections::BTreeMap<RunId, runtime::quiesce::RecoveryRecipe>>,
        preparation_limit: std::time::Duration,
    ) -> Result<RestartId, RuntimeError> {
        self.request_with_source(recipes, preparation_limit, None)
    }
    fn request_with_source(
        &self,
        recipes: Option<std::collections::BTreeMap<RunId, runtime::quiesce::RecoveryRecipe>>,
        preparation_limit: std::time::Duration,
        #[cfg(target_os = "linux")] recipe_source: Option<Arc<dyn public::RecipeSource>>,
        #[cfg(not(target_os = "linux"))] _: Option<()>,
    ) -> Result<RestartId, RuntimeError> {
        let mut state = self.0.state.lock().unwrap();
        if matches!(
            state.phase,
            RestartPhase::Requested
                | RestartPhase::PreparingDrains
                | RestartPhase::WaitingSafePoint
                | RestartPhase::Sealing
                | RestartPhase::Sealed
        ) {
            // A caller cannot upgrade an existing reversible attempt to planned.
            if recipes.is_some() {
                return Err(RuntimeError::RestartBusy);
            }
            return Ok(state.id.unwrap());
        }
        if state.phase != RestartPhase::Running {
            return Err(RuntimeError::RestartBusy);
        }
        // Runtime does its atomic storage-health preflight BEFORE any host gate changes.
        self.0.runtime.check_restart_preparation()?;
        state.generation = state
            .generation
            .checked_add(1)
            .expect("restart generation exhausted");
        let id = RestartId {
            owner: self.0.owner,
            generation: state.generation,
        };
        state.id = Some(id);
        state.recipes = recipes;
        #[cfg(target_os = "linux")]
        {
            state.recipe_source = recipe_source;
        }
        state.learning_pause = Some(self.0.runtime.pause_learning());
        state.evaluation_pause = Some(self.0.preparing.evaluations.producers.pause());
        state.preparation_deadline = Some(tokio::time::Instant::now() + preparation_limit);
        self.publish(&mut state, RestartPhase::PreparingDrains);
        let worker = self.clone();
        state.worker = Some(
            tokio::spawn(async move {
                worker.drive_preparing(id).await;
            })
            .abort_handle(),
        );
        Ok(id)
    }
    async fn prepare(
        &self,
        id: RestartId,
        attempt: &RestartAttempt,
    ) -> Result<SealedRestart, RuntimeError> {
        for producer in &self.0.producers {
            producer.drained().await;
        }
        let _ = self
            .0
            .runtime
            .fence_dispatch_restart_attempt(attempt)
            .await?;
        {
            let mut state = self.0.state.lock().unwrap();
            if state.id != Some(id) {
                return Err(RuntimeError::RestartBusy);
            }
            self.0.runtime.begin_restart_sealing(attempt)?;
            self.0
                .gate
                .transition(id.generation, HostRestartPhase::Sealing);
            self.publish(&mut state, RestartPhase::Sealing);
        }
        self.0.gate.drained().await;
        #[cfg(test)]
        if let Some(hook) = self.0.after_host_drain.lock().unwrap().take() {
            hook();
        }
        // Accepted inputs may invalidate an ACK: re-evaluate after ingress drained.
        let fence = self
            .0
            .runtime
            .fence_dispatch_restart_attempt(attempt)
            .await?;
        {
            let state = self.0.state.lock().unwrap();
            if state.id != Some(id) {
                return Err(RuntimeError::RestartBusy);
            }
        }
        self.0.runtime.drain_dispatch_tails(&fence).await?;
        self.0
            .runtime
            .drain_learning_for_restart()
            .await
            .map_err(|reason| RuntimeError::Model { reason })?;
        self.0
            .gate
            .sealed(&self.0.runtime, id.generation, fence)
            .await
    }
    // Called only in coordinator -> runtime order. Watch notifications are hints;
    // the opaque attempt is rechecked under runtime's lock on every observation.
    fn synchronize_attempt(
        &self,
        state: &mut State,
        attempt: &RestartAttempt,
    ) -> runtime::quiesce::RestartAttemptState {
        use runtime::quiesce::RestartAttemptState as Status;
        let status = self.0.runtime.restart_attempt_state(attempt);
        match status {
            Status::Active => {}
            Status::Cancelling => {
                state.sealed = None;
                if state.phase != RestartPhase::Cancelling {
                    self.0
                        .gate
                        .transition(state.generation, HostRestartPhase::Cancelling);
                    self.publish(state, RestartPhase::Cancelling);
                }
            }
            Status::Cancelled => {
                state.worker = None;
                state.sealed = None;
                self.reopened(state);
            }
            Status::Failed | Status::Superseded => {
                state.worker = None;
                state.sealed = None;
                self.0
                    .gate
                    .transition(state.generation, HostRestartPhase::FailedClosed);
                self.publish(state, RestartPhase::FailedClosed);
            }
            Status::ShuttingDown => {
                self.0.runtime.learning_gate().close();
                self.0.preparing.evaluations.producers.close();
                state.worker = None;
                state.sealed = None;
                state.id = None;
                for producer in &self.0.producers {
                    producer.close();
                }
                self.0
                    .gate
                    .transition(state.generation, HostRestartPhase::ShuttingDown);
                self.publish(state, RestartPhase::ShuttingDown);
            }
        }
        status
    }
    async fn drive(&self, id: RestartId, attempt: RestartAttempt) {
        let deadline = {
            let mut state = self.0.state.lock().unwrap();
            if state.id != Some(id) {
                return;
            }
            self.publish(&mut state, RestartPhase::WaitingSafePoint);
            state.preparation_deadline.unwrap()
        };
        let mut runtime_changes = self.0.runtime.subscribe_restart_state();
        let mut producer_changes: Vec<_> = self
            .0
            .producers
            .iter()
            .map(ProducerGate::subscribe)
            .collect();
        let prepare = self.prepare(id, &attempt);
        tokio::pin!(prepare);
        let mut prepared = false;
        let mut expired = false;
        loop {
            {
                let mut state = self.0.state.lock().unwrap();
                if state.id != Some(id) {
                    return;
                }
                use runtime::quiesce::RestartAttemptState as Status;
                if !matches!(
                    self.synchronize_attempt(&mut state, &attempt),
                    Status::Active | Status::Cancelling
                ) {
                    return;
                }
            }
            tokio::select! {
                _ = tokio::time::sleep_until(deadline), if !expired => {
                    expired = true;
                    let _ = self.cancel(id);
                }
                result = &mut prepare, if !prepared => {
                    prepared = true;
                    let mut state = self.0.state.lock().unwrap();
                    if state.id != Some(id) { return; }
                    if self.synchronize_attempt(&mut state, &attempt) == runtime::quiesce::RestartAttemptState::Active {
                        match result {
                            Ok(sealed) => {
                                state.sealed = Some(sealed);
                                self.publish(&mut state, RestartPhase::Sealed);
                            }
                            Err(error) => {
                                // A concurrent direct Stop may have invalidated prepare.
                                // Atomically fail only an active attempt, then observe again.
                                let _ = self.0.runtime.fail_restart_attempt(&attempt, error);
                                self.synchronize_attempt(&mut state, &attempt);
                            }
                        }
                    }
                }
                _ = producer_change(&mut producer_changes) => {
                    let mut state = self.0.state.lock().unwrap();
                    if state.id != Some(id) { return; }
                    let phase = state.phase; self.publish(&mut state, phase);
                }
                changed = runtime_changes.changed() => {
                    if changed.is_err() { return; }
                    let mut state = self.0.state.lock().unwrap();
                    if state.id != Some(id) { return; }
                    let phase = state.phase; self.publish(&mut state, phase);
                }
            }
            // Remain alive after Sealed: direct runtime Stop is an existing API.
        }
    }
    pub async fn wait_sealed(&self, id: RestartId) -> Result<SealedObservation, RuntimeError> {
        let mut changed = self.subscribe();
        loop {
            {
                let state = self.0.state.lock().unwrap();
                if state.id != Some(id)
                    || matches!(
                        state.phase,
                        RestartPhase::FailedClosed | RestartPhase::ShuttingDown
                    )
                {
                    return Err(RuntimeError::RestartBusy);
                }
                if let Some(sealed) = &state.sealed {
                    sealed.validate(&self.0.runtime)?;
                    return Ok(SealedObservation {
                        owner: Arc::downgrade(&self.0),
                        id,
                    });
                }
            }
            changed
                .changed()
                .await
                .map_err(|_| RuntimeError::RestartBusy)?;
        }
    }
    fn abort_worker(state: &mut State) {
        if let Some(worker) = state.worker.take() {
            worker.abort();
        }
        state.sealed = None;
    }
    fn reopened(&self, state: &mut State) {
        if self.0.runtime.check_restart_preparation().is_err() {
            state.pauses = self.0.producers.iter().map(ProducerGate::pause).collect();
            self.0
                .gate
                .transition(state.generation, HostRestartPhase::FailedClosed);
            self.publish(state, RestartPhase::FailedClosed);
            return;
        }
        if let Some(pause) = state.learning_pause.take() {
            let restored = if let Some(suspension) = state.suspension.take() {
                self.0
                    .runtime
                    .cancel_learning_suspension(pause, suspension)
                    .is_ok()
            } else {
                self.0.runtime.resume_learning(pause)
            };
            if !restored {
                state.pauses = self.0.producers.iter().map(ProducerGate::pause).collect();
                self.0
                    .gate
                    .transition(state.generation, HostRestartPhase::FailedClosed);
                self.publish(state, RestartPhase::FailedClosed);
                return;
            }
        }
        if let Some(pause) = state.evaluation_pause.take() {
            self.0.preparing.evaluations.producers.resume(pause);
        }
        for (producer, pause) in self.0.producers.iter().zip(state.pauses.drain(..)) {
            producer.resume(pause);
        }
        state.id = None;
        state.attempt = None;
        state.recipes = None;
        #[cfg(target_os = "linux")]
        {
            state.recipe_source = None;
        }
        state.preparation_deadline = None;
        self.0
            .gate
            .transition(state.generation, HostRestartPhase::Open);
        self.publish(state, RestartPhase::Running);
    }
    pub fn cancel(&self, id: RestartId) -> Result<(), RuntimeError> {
        self.cancel_checked(id, None)
    }
    fn cancel_checked(&self, id: RestartId, revision: Option<u64>) -> Result<(), RuntimeError> {
        let mut state = self.0.state.lock().unwrap();
        if state.id != Some(id)
            || revision.is_some_and(|revision| revision != state.revision)
            || state.capture_started
            || matches!(
                state.phase,
                RestartPhase::FailedClosed | RestartPhase::ShuttingDown
            )
        {
            return Err(RuntimeError::RestartBusy);
        }
        if state.attempt.is_none() {
            Self::abort_worker(&mut state);
            self.reopened(&mut state);
            return if state.phase == RestartPhase::Running {
                Ok(())
            } else {
                Err(RuntimeError::RestartBusy)
            };
        }
        self.0
            .gate
            .transition(id.generation, HostRestartPhase::Cancelling);
        self.publish(&mut state, RestartPhase::Cancelling);
        let attempt = state.attempt.as_ref().unwrap().clone();
        let result = self.0.runtime.cancel_restart_quiesce(&attempt);
        self.synchronize_attempt(&mut state, &attempt);
        if state.phase == RestartPhase::FailedClosed {
            return Err(RuntimeError::RestartBusy);
        }
        result.map(|_| ())
    }
    /// Only operator shutdown calls the destructive host drain; this just closes admission.
    pub fn transition_to_shutdown(&self) {
        let mut state = self.0.state.lock().unwrap();
        if !state.capture_started {
            Self::abort_worker(&mut state);
        }
        state.id = None;
        for producer in &self.0.producers {
            producer.close();
        }
        self.0.runtime.learning_gate().close();
        self.0.preparing.evaluations.producers.close();
        self.0
            .gate
            .transition(state.generation, HostRestartPhase::ShuttingDown);
        self.publish(&mut state, RestartPhase::ShuttingDown);
    }
}
impl RestartStopHandler for RestartCoordinator {
    fn stop(&self, run: RunId, scope: StopScope) -> Result<(), RuntimeError> {
        // Direct and host Stop share the runtime-installed outer boundary.
        self.0.runtime.stop(run, scope)
    }
}

impl RestartSnapshot {
    /// Allowlist fields: never serialize runtime errors, incarnation strings or tool output.
    pub fn to_dto(&self) -> workbench_protocol::namespaces::lifecycle::RestartStatus {
        use workbench_protocol::namespaces::lifecycle as dto;
        let phase = match self.phase {
            RestartPhase::Running => dto::RestartPhase::Running,
            RestartPhase::Requested => dto::RestartPhase::Requested,
            RestartPhase::PreparingDrains => dto::RestartPhase::PreparingDrains,
            RestartPhase::WaitingSafePoint => dto::RestartPhase::WaitingSafePoint,
            RestartPhase::Sealing => dto::RestartPhase::Sealing,
            RestartPhase::Sealed => dto::RestartPhase::Sealed,
            RestartPhase::Capturing => dto::RestartPhase::Capturing,
            RestartPhase::Finalizing => dto::RestartPhase::Finalizing,
            RestartPhase::Cancelling => dto::RestartPhase::Cancelling,
            RestartPhase::FailedClosed => dto::RestartPhase::FailedClosed,
            RestartPhase::ShuttingDown => dto::RestartPhase::ShuttingDown,
        };
        let blockers: Vec<_> = self
            .blockers
            .iter()
            .take(dto::MAX_RESTART_BLOCKERS)
            .map(|blocker| {
                use runtime::quiesce::RestartBlockerKind as Kind;
                dto::RestartBlocker {
                    run: workbench_protocol::ids::RunId::new(blocker.run.to_string())
                        .expect("runtime run id"),
                    kind: match blocker.kind {
                        Kind::InFlightAdmission => dto::RestartBlockerKind::InFlightAdmission,
                        Kind::AcceptedResumePending => {
                            dto::RestartBlockerKind::AcceptedResumePending
                        }
                        Kind::AwaitingSafePoint => dto::RestartBlockerKind::AwaitingSafePoint,
                        Kind::ShellRunning => dto::RestartBlockerKind::ShellRunning,
                        Kind::ShellResultUnobserved => {
                            dto::RestartBlockerKind::ShellResultUnobserved
                        }
                        Kind::UnconfirmedRunJs => dto::RestartBlockerKind::UnconfirmedRunJs,
                        Kind::UnfinishedToolBatch => dto::RestartBlockerKind::UnfinishedToolBatch,
                    },
                }
            })
            .collect();
        dto::RestartStatus {
            restart_id: self.id.map(|id| {
                workbench_protocol::ids::RestartId::new(id.public_id().as_str())
                    .expect("restart id")
            }),
            phase,
            revision: self.revision,
            blocker_total: self.blockers.len() as u64,
            blocker_omitted: (self.blockers.len() - blockers.len()) as u64,
            blockers,
            active_producers: self.active_producers as u64,
        }
    }
}

async fn producer_change(receivers: &mut [watch::Receiver<u64>]) {
    std::future::poll_fn(|cx| {
        for receiver in receivers.iter_mut() {
            let changed = receiver.changed();
            tokio::pin!(changed);
            if std::future::Future::poll(changed.as_mut(), cx).is_ready() {
                return std::task::Poll::Ready(());
            }
        }
        std::task::Poll::Pending
    })
    .await
}
#[cfg(all(test, target_os = "linux"))]
mod shutdown_watchdog_tests;
#[cfg(test)]
pub(crate) mod tests;
