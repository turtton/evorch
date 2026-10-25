use super::*;

/// One execution's immutable cancellation channel and monotonic publication fence.
pub(crate) struct ExecutionControl {
    publication: PublicationToken,
    pub(super) cancel_tx: watch::Sender<RunInterrupt>,
}

#[derive(Clone)]
pub(crate) struct PublicationToken(Arc<PublicationFence>);

struct PublicationFence {
    gate: Mutex<()>,
    cancelled: AtomicBool,
}

impl ExecutionControl {
    pub(crate) fn new(cancelled: bool) -> Arc<Self> {
        Arc::new(Self {
            publication: PublicationToken(Arc::new(PublicationFence {
                gate: Mutex::new(()),
                cancelled: AtomicBool::new(cancelled),
            })),
            cancel_tx: watch::channel(if cancelled {
                RunInterrupt::Cancel
            } else {
                RunInterrupt::None
            })
            .0,
        })
    }

    pub(crate) fn publication(&self) -> PublicationToken {
        self.publication.clone()
    }
    pub(super) fn cancelled(&self) -> bool {
        self.publication.0.cancelled.load(Ordering::Acquire)
    }

    fn forbid_publication(&self) {
        self.publication.0.cancelled.store(true, Ordering::Release);
        self.cancel_tx.send_replace(RunInterrupt::Cancel);
    }
}

impl PublicationToken {
    pub(crate) fn commit<T>(
        &self,
        publish: impl FnOnce() -> Result<T, String>,
    ) -> Result<T, String> {
        let _gate = self
            .0
            .gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.0.cancelled.load(Ordering::Acquire) {
            return Err("image generation interrupted; no files saved".into());
        }
        publish()
    }
}

#[derive(Default)]
pub(super) struct SpawnIntent {
    pub(super) parent: Option<RunId>,
    // A subtree tombstone outlives the execution it cancelled.
    pub(super) cancelled: bool,
    pub(super) execution: Option<Arc<ExecutionControl>>,
}

pub(super) fn current_execution(
    run: RunId,
    intents: &HashMap<RunId, SpawnIntent>,
    admissions: &HashMap<RunId, admission::Admission>,
    runs: &HashMap<RunId, RunEntry>,
) -> Option<Arc<ExecutionControl>> {
    intents
        .get(&run)
        .and_then(|i| i.execution.clone())
        .or_else(|| admissions.get(&run).map(|a| Arc::clone(&a.execution)))
        .or_else(|| runs.get(&run).map(|r| Arc::clone(&r.execution)))
}

impl AgentRuntime {
    pub(crate) fn execution_snapshot(
        &self,
        run: RunId,
    ) -> Result<Arc<ExecutionControl>, RuntimeError> {
        let intents = self
            .shared
            .spawn_intents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let admissions = self
            .shared
            .admissions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let runs = lock_runs(&self.shared.runs);
        current_execution(run, &intents, &admissions, &runs).ok_or_else(|| unknown_run(run))
    }

    /// Global locks never wait for a gate. Retry drops the old gate before
    /// selecting a new generation; the notification uses only that generation.
    pub fn cancel(&self, run: RunId) -> Result<(), RuntimeError> {
        loop {
            let execution = self.execution_snapshot(run)?;
            #[cfg(test)]
            self.execution_hook(HookPoint::CancelSnapshot, &execution);
            let gate = execution
                .publication
                .0
                .gate
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            #[cfg(test)]
            self.execution_hook(HookPoint::CancelGate, &execution);
            let intents = self
                .shared
                .spawn_intents
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut admissions = self
                .shared
                .admissions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let runs = lock_runs(&self.shared.runs);
            let current = current_execution(run, &intents, &admissions, &runs)
                .ok_or_else(|| unknown_run(run))?;
            if !Arc::ptr_eq(&execution, &current) {
                drop(runs);
                drop(admissions);
                drop(intents);
                drop(gate);
                continue;
            }
            if let Some(admission) = admissions.get_mut(&run)
                && Arc::ptr_eq(&admission.execution, &execution)
            {
                admission.cancelled = true;
            }
            execution.forbid_publication();
            return Ok(());
        }
    }

    pub(super) fn cancel_execution(&self, execution: &Arc<ExecutionControl>) {
        let _gate = execution
            .publication
            .0
            .gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        execution.forbid_publication();
    }

    /// Validate before durable snapshot consumption; install only once it
    /// succeeds, while the caller still holds I -> A -> R.
    pub(super) fn prepare_restored_execution(
        run: RunId,
        parent: Option<RunId>,
        intents: &HashMap<RunId, SpawnIntent>,
    ) -> Result<Arc<ExecutionControl>, RuntimeError> {
        if intents.get(&run).is_some_and(|i| i.cancelled)
            || parent
                .and_then(|p| intents.get(&p))
                .is_some_and(|i| i.cancelled)
        {
            return Err(RuntimeError::RunTerminated {
                run_id: run.to_string(),
            });
        }
        Ok(ExecutionControl::new(false))
    }

    pub(super) fn install_restored_execution(
        run: RunId,
        parent: Option<RunId>,
        intents: &mut HashMap<RunId, SpawnIntent>,
        admissions: &mut HashMap<RunId, admission::Admission>,
        execution: &Arc<ExecutionControl>,
    ) {
        admissions.remove(&run);
        let intent = intents.entry(run).or_default();
        intent.parent = intent.parent.or(parent);
        intent.execution = Some(Arc::clone(execution));
    }

    #[cfg(test)]
    pub(crate) fn seed_spawn_intent(&self, run: RunId, owner: &str) {
        let Ok(parent) = owner.parse::<RunId>() else {
            return;
        };
        let mut intents = self
            .shared
            .spawn_intents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let cancelled = intents.get(&parent).is_some_and(|i| i.cancelled);
        intents.insert(
            run,
            SpawnIntent {
                parent: Some(parent),
                cancelled,
                execution: None,
            },
        );
    }

    pub(crate) fn spawn_cancelled(&self, run: RunId) -> bool {
        self.shared
            .spawn_intents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&run)
            .is_some_and(|i| i.cancelled)
    }
}

#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum HookPoint {
    CancelSnapshot,
    CancelGate,
    SubtreeCollected,
    AdmissionRegistered,
    AdmissionFailed,
    RootGoalUpdated,
    RootRestoreValidated,
    HandoffBeforeReserve,
    AdmissionCompleted,
    AdmissionGoalCheckpoint,
}
#[cfg(test)]
type ExecutionHook = (HookPoint, Weak<ExecutionControl>, Box<dyn FnOnce() + Send>);
#[cfg(test)]
#[derive(Default)]
pub(super) struct TestHooks(Mutex<Vec<ExecutionHook>>);
#[cfg(test)]
impl AgentRuntime {
    pub(crate) fn on_execution(
        &self,
        point: HookPoint,
        execution: &Arc<ExecutionControl>,
        hook: impl FnOnce() + Send + 'static,
    ) {
        self.shared.execution_hooks.0.lock().unwrap().push((
            point,
            Arc::downgrade(execution),
            Box::new(hook),
        ));
    }
    pub(super) fn execution_hook(&self, point: HookPoint, execution: &Arc<ExecutionControl>) {
        let hook = {
            let mut hooks = self.shared.execution_hooks.0.lock().unwrap();
            hooks
                .iter()
                .position(|(p, e, _)| {
                    *p == point && e.upgrade().is_some_and(|e| Arc::ptr_eq(&e, execution))
                })
                .map(|index| hooks.remove(index).2)
        };
        if let Some(hook) = hook {
            hook();
        }
    }
}

#[cfg(test)]
mod goal_tests;
#[cfg(test)]
mod tests;
