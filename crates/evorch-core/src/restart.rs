//! One process-local lifecycle owner. There is deliberately no wire/production request handler.
//! Lock order: coordinator -> host gate / runtime. Runtime never calls back into this owner.
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RestartPhase {
    Running,
    Requested,
    WaitingSafePoint,
    Sealing,
    Sealed,
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
    id: Option<RestartId>,
    phase: RestartPhase,
    revision: u64,
    generation: u64,
    attempt: Option<RestartAttempt>,
    pauses: Vec<ProducerPause>,
    sealed: Option<Arc<SealedRestart>>,
    worker: Option<tokio::task::AbortHandle>,
}
struct Inner {
    owner: uuid::Uuid,
    runtime: AgentRuntime,
    gate: HostRestartGate,
    producers: Vec<ProducerGate>,
    state: Mutex<State>,
    changed: watch::Sender<u64>,
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
    ) -> Self {
        Self(Arc::new(Inner {
            owner: uuid::Uuid::new_v4(),
            runtime,
            gate,
            producers,
            state: Mutex::new(State {
                id: None,
                phase: RestartPhase::Running,
                revision: 0,
                generation: 0,
                attempt: None,
                pauses: vec![],
                sealed: None,
                worker: None,
            }),
            changed: watch::channel(0).0,
        }))
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
        let mut state = self.0.state.lock().unwrap();
        if matches!(
            state.phase,
            RestartPhase::Requested
                | RestartPhase::WaitingSafePoint
                | RestartPhase::Sealing
                | RestartPhase::Sealed
        ) {
            return Ok(state.id.unwrap());
        }
        if state.phase != RestartPhase::Running {
            return Err(RuntimeError::RestartBusy);
        }
        // Runtime does its atomic storage-health preflight BEFORE any host gate changes.
        let attempt = self.0.runtime.begin_restart_quiesce()?;
        state.generation = state
            .generation
            .checked_add(1)
            .expect("restart generation exhausted");
        let id = RestartId {
            owner: self.0.owner,
            generation: state.generation,
        };
        state.id = Some(id);
        state.attempt = Some(attempt.clone());
        state.pauses = self.0.producers.iter().map(ProducerGate::pause).collect();
        self.0
            .gate
            .transition(id.generation, HostRestartPhase::Preparing);
        self.publish(&mut state, RestartPhase::Requested);
        let worker = self.clone();
        state.worker = Some(
            tokio::spawn(async move {
                worker.drive(id, attempt).await;
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
        self.0
            .gate
            .sealed(&self.0.runtime, id.generation, fence)
            .await
    }
    async fn drive(&self, id: RestartId, attempt: RestartAttempt) {
        {
            let mut state = self.0.state.lock().unwrap();
            if state.id != Some(id) {
                return;
            }
            self.publish(&mut state, RestartPhase::WaitingSafePoint);
        }
        let mut runtime_changes = self.0.runtime.subscribe_restart_state();
        let mut producer_changes: Vec<_> = self
            .0
            .producers
            .iter()
            .map(ProducerGate::subscribe)
            .collect();
        let prepare = self.prepare(id, &attempt);
        tokio::pin!(prepare);
        let result = loop {
            tokio::select! {
                result = &mut prepare => break result,
                _ = producer_change(&mut producer_changes) => {
                    let mut state = self.0.state.lock().unwrap();
                    if state.id != Some(id) { return; }
                    let phase = state.phase; self.publish(&mut state, phase);
                }
                changed = runtime_changes.changed() => {
                    if changed.is_err() { return; }
                    let mut state = self.0.state.lock().unwrap();
                    if state.id != Some(id) { return; }
                    let phase = state.phase;
                    self.publish(&mut state, phase);
                }
            }
        };
        let mut state = self.0.state.lock().unwrap();
        if state.id != Some(id) {
            return;
        }
        state.worker = None;
        match result {
            Ok(sealed) => {
                state.sealed = Some(Arc::new(sealed));
                self.publish(&mut state, RestartPhase::Sealed);
            }
            Err(_) => {
                self.0
                    .gate
                    .transition(id.generation, HostRestartPhase::FailedClosed);
                self.publish(&mut state, RestartPhase::FailedClosed);
            }
        }
    }
    pub async fn wait_sealed(&self, id: RestartId) -> Result<Arc<SealedRestart>, RuntimeError> {
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
                    return Ok(sealed.clone());
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
        for (producer, pause) in self.0.producers.iter().zip(state.pauses.drain(..)) {
            producer.resume(pause);
        }
        state.id = None;
        state.attempt = None;
        self.0
            .gate
            .transition(state.generation, HostRestartPhase::Open);
        self.publish(state, RestartPhase::Running);
    }
    pub fn cancel(&self, id: RestartId) -> Result<(), RuntimeError> {
        let mut state = self.0.state.lock().unwrap();
        if state.id != Some(id)
            || matches!(
                state.phase,
                RestartPhase::FailedClosed | RestartPhase::ShuttingDown
            )
        {
            return Err(RuntimeError::RestartBusy);
        }
        Self::abort_worker(&mut state);
        self.0
            .gate
            .transition(id.generation, HostRestartPhase::Cancelling);
        self.publish(&mut state, RestartPhase::Cancelling);
        match self
            .0
            .runtime
            .cancel_restart_quiesce(state.attempt.as_ref().unwrap())
        {
            Ok(_) => {
                self.reopened(&mut state);
                Ok(())
            }
            Err(error) => {
                self.0
                    .gate
                    .transition(id.generation, HostRestartPhase::FailedClosed);
                self.publish(&mut state, RestartPhase::FailedClosed);
                Err(error)
            }
        }
    }
    /// Only operator shutdown calls the destructive host drain; this just closes admission.
    pub fn transition_to_shutdown(&self) {
        let mut state = self.0.state.lock().unwrap();
        Self::abort_worker(&mut state);
        state.id = None;
        for producer in &self.0.producers {
            producer.close();
        }
        self.0
            .gate
            .transition(state.generation, HostRestartPhase::ShuttingDown);
        self.publish(&mut state, RestartPhase::ShuttingDown);
    }
}
impl RestartStopHandler for RestartCoordinator {
    fn stop(&self, run: RunId, scope: StopScope) -> Result<(), RuntimeError> {
        let mut state = self.0.state.lock().unwrap();
        let cancelling = matches!(state.phase, RestartPhase::Sealing | RestartPhase::Sealed);
        if cancelling {
            Self::abort_worker(&mut state);
            self.0
                .gate
                .transition(state.generation, HostRestartPhase::Cancelling);
            self.publish(&mut state, RestartPhase::Cancelling);
        }
        // Runtime first prepares dispatch, then delivers Stop, then durably resumes others.
        let result = self.0.runtime.stop(run, scope);
        if cancelling {
            if result.is_ok()
                && self.0.runtime.quiesce_state() == runtime::quiesce::QuiesceState::Running
            {
                self.reopened(&mut state);
            } else {
                self.0
                    .gate
                    .transition(state.generation, HostRestartPhase::FailedClosed);
                self.publish(&mut state, RestartPhase::FailedClosed);
            }
        }
        result
    }
}

impl RestartSnapshot {
    /// Allowlist fields: never serialize runtime errors, incarnation strings or tool output.
    pub fn to_dto(&self) -> workbench_protocol::namespaces::lifecycle::RestartStatus {
        use workbench_protocol::namespaces::lifecycle as dto;
        let phase = match self.phase {
            RestartPhase::Running => dto::RestartPhase::Running,
            RestartPhase::Requested => dto::RestartPhase::Requested,
            RestartPhase::WaitingSafePoint => dto::RestartPhase::WaitingSafePoint,
            RestartPhase::Sealing => dto::RestartPhase::Sealing,
            RestartPhase::Sealed => dto::RestartPhase::Sealed,
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
                workbench_protocol::ids::RestartId::new(format!("{}:{}", id.owner, id.generation))
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
#[cfg(test)]
mod tests;
