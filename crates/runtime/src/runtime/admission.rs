use super::*;
use event_bus::event::diagnostic_codes;
#[path = "queued_restart.rs"]
mod queued_restart;
pub(super) use queued_restart::QueuedLaunch;

pub(super) type Admissions = Mutex<HashMap<RunId, Admission>>;
/// Host-owned lane producers can still be waiting before a RunEntry exists.
#[derive(Default)]
pub(super) struct LaneAdmissions {
    state: Mutex<LaneAdmissionState>,
    draining: tokio::sync::Mutex<()>,
}

#[derive(Default)]
struct LaneAdmissionState {
    tasks: tokio::task::JoinSet<()>,
    executions: HashMap<tokio::task::Id, (RunId, Arc<ExecutionControl>)>,
    failure: Option<tokio::task::JoinError>,
}

impl LaneAdmissionState {
    fn reap(&mut self) {
        while let Some(result) = self.tasks.try_join_next_with_id() {
            match result {
                Ok((id, ())) => {
                    self.executions.remove(&id);
                }
                Err(error) => {
                    self.executions.remove(&error.id());
                    self.failure.get_or_insert(error);
                }
            }
        }
    }
}

impl LaneAdmissions {
    fn spawn(
        &self,
        run: RunId,
        execution: Arc<ExecutionControl>,
        task: impl std::future::Future<Output = ()> + Send + 'static,
    ) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.reap();
        let id = state.tasks.spawn(task).id();
        state.executions.insert(id, (run, execution));
    }
}
#[cfg(test)]
pub(super) type RegistrationHook = Arc<dyn Fn(RunId) + Send + Sync>;

pub(super) struct Admission {
    pub(super) queued: Option<QueuedLaunch>,
    parent: Option<RunId>,
    lane_root: bool,
    pub(super) cancelled: bool,
    pub(super) execution: Arc<ExecutionControl>,
    ownership: Option<crate::ownership::OwnerPermit>,
    completion: Option<watch::Sender<Option<Result<(), RuntimeError>>>>,
    pub(super) result: watch::Receiver<Option<Result<(), RuntimeError>>>,
}

pub(crate) struct AdmissionSnapshot {
    pub parent: Option<RunId>,
    pub ownership: Option<crate::ownership::OwnerPermit>,
    pub result: Option<Result<(), RuntimeError>>,
}

impl Admission {
    pub(super) fn finish_queued(&mut self, run: RunId) {
        if let Some(completion) = self.completion.take() {
            completion.send_replace(Some(Err(RuntimeError::RunTerminated {
                run_id: run.to_string(),
            })));
        }
    }
    pub(super) fn snapshot(&self) -> AdmissionSnapshot {
        AdmissionSnapshot {
            parent: self.parent,
            ownership: self.ownership.clone(),
            result: self.result.borrow().clone().or_else(|| {
                self.result.has_changed().is_err().then(|| {
                    Err(RuntimeError::Model {
                        reason: "provider admission was interrupted".into(),
                    })
                })
            }),
        }
    }
}

impl AgentRuntime {
    pub(crate) fn complete_restart_safe_points(&self) -> Result<bool, RuntimeError> {
        // I -> quiesce. Prepared has no live LoopState and only activation can
        // open its start gate; that producer must acquire I and dispatch first.
        let indexes = self
            .shared
            .spawn_intents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let unstarted = indexes
            .iter()
            .filter_map(|(id, index)| index.paused_recovery.as_ref().map(|_| *id))
            .collect();
        self.shared
            .quiesce
            .complete(
                self.shared.run_store.get(),
                &unstarted,
                !self.accepted_chat_resumes().is_empty(),
            )
            .map_err(crate::quiesce::runtime::error)
    }

    /// Lane root admissions not yet finished, including ones absent from list_agents.
    pub fn pending_lane_admissions(&self) -> Vec<RunId> {
        let mut runs: Vec<_> = {
            let mut state = self
                .shared
                .lane_admissions
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.reap();
            state.executions.values().map(|(run, _)| *run).collect()
        };
        let admissions = self
            .shared
            .admissions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        runs.extend(
            admissions
                .iter()
                .filter(|(_, a)| a.lane_root && a.result.borrow().is_none())
                .map(|(run, _)| *run),
        );
        runs.sort_unstable();
        runs.dedup();
        runs
    }

    /// After stopping lane producers, cancel and join every lane admission generation.
    /// The provider future is dropped by its cancellation branch, not detached.
    pub async fn drain_lane_admissions(&self) -> Result<(), tokio::task::JoinError> {
        let _draining = self.shared.lane_admissions.draining.lock().await;
        let mut failure = None;
        loop {
            // I serializes the snapshot with reservation and task registration.
            // A queued admission has no JoinSet task, but still owns a permit.
            let (mut tasks, executions) = {
                let _intents = self
                    .shared
                    .spawn_intents
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let (tasks, executions) = {
                    let mut state = self
                        .shared
                        .lane_admissions
                        .state
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    state.reap();
                    if let Some(error) = state.failure.take() {
                        failure.get_or_insert(error);
                    }
                    (
                        std::mem::take(&mut state.tasks),
                        std::mem::take(&mut state.executions),
                    )
                };
                let mut executions: Vec<_> = executions.into_values().collect();
                {
                    let admissions = self
                        .shared
                        .admissions
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    executions.extend(
                        admissions
                            .iter()
                            .filter(|(_, a)| {
                                a.lane_root && a.completion.is_some() && a.result.borrow().is_none()
                            })
                            .map(|(run, a)| (*run, Arc::clone(&a.execution))),
                    );
                }
                (tasks, executions)
            };
            if tasks.is_empty() && executions.is_empty() {
                break;
            }
            for (run, execution) in executions {
                let unregistered = {
                    // Registration holds A -> R too. Mark cancellation under
                    // that fence; never acquire a publication gate under A/R.
                    let mut admissions = self
                        .shared
                        .admissions
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let runs = lock_runs(&self.shared.runs);
                    let registered = runs
                        .get(&run)
                        .is_some_and(|entry| Arc::ptr_eq(&entry.execution, &execution));
                    if !registered
                        && let Some(admission) = admissions.get_mut(&run)
                        && Arc::ptr_eq(&admission.execution, &execution)
                    {
                        admission.cancelled = true;
                        let queued = self.note_fenced_intent_stop(
                            run,
                            &execution.publication().incarnation(),
                            crate::quiesce::StopReason::Cancelled,
                            runs.get(&run)
                                .is_some_and(|entry| Arc::ptr_eq(&entry.execution, &execution)),
                        );
                        // Registration is fenced by A/R: set the signal before a
                        // completing provider can choose its durable stop reason.
                        execution.cancel_tx.send_replace(RunInterrupt::Cancel);
                        execution.approvals.close();
                        if queued {
                            admission.finish_queued(run);
                        }
                    }
                    !registered
                };
                // Registered roots belong to the host's ordinary Stop/drain.
                if unregistered {
                    self.cancel_execution(&execution);
                }
            }
            while let Some(result) = tasks.join_next().await {
                if let Err(error) = result {
                    failure.get_or_insert(error);
                }
            }
        }
        self.discard_cancelled_restart_launches();
        failure.map_or(Ok(()), Err)
    }

    pub(super) fn fail_review_admission(
        &self,
        run_id: RunId,
        parent: Option<RunId>,
        config: &RunConfig,
        reason: String,
    ) {
        self.fail_admission(run_id, parent, config, Self::review_error(run_id, reason));
    }

    /// Complete a reserved admission with `error` instead of registering the run.
    pub(super) fn fail_admission(
        &self,
        run_id: RunId,
        parent: Option<RunId>,
        config: &RunConfig,
        error: RuntimeError,
    ) {
        self.drop_unstarted_host_input(run_id, config.initial_prompt_id.as_deref());
        let execution = self.reserve_admission(run_id, parent, config.ownership.clone());
        let mut admissions = self
            .shared
            .admissions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(completion) = admissions
            .get_mut(&run_id)
            .filter(|admission| Arc::ptr_eq(&admission.execution, &execution))
            .and_then(|admission| admission.completion.take())
        {
            completion.send_replace(Some(Err(error)));
        }
    }

    /// Reserve before publishing a handoff, so cancellation already has a
    /// generation-bound channel before provider admission or RunEntry exists.
    pub(crate) fn reserve_admission(
        &self,
        run_id: RunId,
        parent: Option<RunId>,
        ownership: Option<crate::ownership::OwnerPermit>,
    ) -> Arc<ExecutionControl> {
        let mut intents = self
            .shared
            .spawn_intents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let inherited = intents.get(&run_id).is_some_and(|i| i.cancelled)
            || parent
                .and_then(|p| intents.get(&p))
                .is_some_and(|i| i.cancelled);
        let intent = intents.entry(run_id).or_default();
        intent.parent = intent.parent.or(parent);
        let execution = Arc::clone(
            intent
                .execution
                .get_or_insert_with(|| ExecutionControl::new(inherited)),
        );
        self.reserve_admission_for(run_id, parent, ownership, Arc::clone(&execution), false);
        execution
    }

    pub(super) fn reserve_admission_for(
        &self,
        run_id: RunId,
        parent: Option<RunId>,
        ownership: Option<crate::ownership::OwnerPermit>,
        execution: Arc<ExecutionControl>,
        lane_root: bool,
    ) {
        let mut admissions = self
            .shared
            .admissions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(admission) = admissions.get_mut(&run_id)
            && Arc::ptr_eq(&admission.execution, &execution)
            && admission.snapshot().result.is_none()
        {
            admission.lane_root |= lane_root;
            return;
        }
        let (completion, result) = watch::channel(None);
        admissions.insert(
            run_id,
            Admission {
                queued: None,
                parent,
                lane_root,
                cancelled: execution.cancelled(),
                execution,
                ownership,
                completion: Some(completion),
                result,
            },
        );
    }

    /// Handoff already reserved this execution before taking Q. Only A is
    /// needed here; never reacquire I from the handoff callback.
    pub(crate) fn update_admission_owner(
        &self,
        run: RunId,
        execution: &Arc<ExecutionControl>,
        owner: Option<crate::ownership::OwnerPermit>,
    ) {
        let mut admissions = self
            .shared
            .admissions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(admission) = admissions.get_mut(&run)
            && Arc::ptr_eq(&admission.execution, execution)
        {
            admission.ownership = owner;
        }
    }

    pub(crate) fn fail_reserved_admission(
        &self,
        run: RunId,
        execution: &Arc<ExecutionControl>,
        reason: String,
    ) {
        let mut admissions = self
            .shared
            .admissions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(admission) = admissions.get_mut(&run)
            && Arc::ptr_eq(&admission.execution, execution)
            && let Some(completion) = admission.completion.take()
        {
            if let Some(store) = self.shared.run_store.get()
                && let Err(error) = store
                    .handle
                    .note_handoff_start_failure(run.to_string(), reason.clone())
            {
                tracing::warn!(%run,%error,"handoff admission failure receipt was not saved");
            }
            completion.send_replace(Some(Err(RuntimeError::Model { reason })));
        }
    }

    // Forward the prepared review seed without performing storage reads.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn register_without_admission(
        &self,
        run_id: RunId,
        parent: Option<RunId>,
        role: Role,
        prompt: String,
        config: RunConfig,
        continuation: RunContinuation,
        review_seed: Option<ReviewSeed>,
        execution: Arc<ExecutionControl>,
        intents: &HashMap<RunId, cancellation::SpawnIntent>,
    ) -> RunId {
        let mut admissions = self
            .shared
            .admissions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if admissions
            .get(&run_id)
            .is_some_and(|a| !Arc::ptr_eq(&a.execution, &execution))
        {
            admissions.remove(&run_id);
        }
        let cancelled = admissions.get(&run_id).is_some_and(|admission| {
            Arc::ptr_eq(&admission.execution, &execution) && admission.cancelled
        });
        if !cancelled {
            self.register_run(
                run_id,
                parent,
                role,
                prompt,
                config,
                continuation,
                review_seed,
                Arc::clone(&execution),
                intents,
            );
        } else {
            self.end_intent(
                run_id,
                &execution.publication().incarnation(),
                if matches!(*execution.cancel_tx.borrow(), RunInterrupt::Cancel) {
                    crate::quiesce::StopReason::Cancelled
                } else {
                    crate::quiesce::StopReason::OperatorStopped
                },
            );
            self.drop_unstarted_host_input(run_id, config.initial_prompt_id.as_deref());
            drop(continuation);
        }
        if let Some(admission) = admissions.get_mut(&run_id)
            && Arc::ptr_eq(&admission.execution, &execution)
            && let Some(completion) = admission.completion.take()
        {
            completion.send_replace(Some(if cancelled {
                Err(RuntimeError::RunTerminated {
                    run_id: run_id.to_string(),
                })
            } else {
                Ok(())
            }));
        }
        run_id
    }

    // The synchronous API reserves an ID; only successful admission registers a run.
    // Forward the prepared review seed without performing storage reads.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn admit_run(
        &self,
        run_id: RunId,
        parent: Option<RunId>,
        role: Role,
        prompt: String,
        config: RunConfig,
        continuation: RunContinuation,
        review_seed: Option<ReviewSeed>,
        execution: Arc<ExecutionControl>,
        intents: &HashMap<RunId, cancellation::SpawnIntent>,
    ) -> Result<RunId, RuntimeError> {
        let dispatch = self.dispatch_registration_guard()?;
        assert!(
            intents
                .get(&run_id)
                .and_then(|i| i.execution.as_ref())
                .is_some_and(|current| Arc::ptr_eq(current, &execution))
        );
        let lane_root = parent.is_none() && config.lane_scope == Some(event_bus::LaneScope::Lane);
        self.reserve_admission_for(
            run_id,
            parent,
            config.ownership.clone(),
            Arc::clone(&execution),
            lane_root,
        );
        let result = self
            .shared
            .admissions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get_mut(&run_id)
            .and_then(|admission| admission.completion.take())
            .expect("reserved admission has one completion sender");
        let goal_stamp = self.goal_input_stamp(run_id);
        let lane_execution = Arc::clone(&execution);
        let runtime = self.clone();
        let task = dispatch.scope(async move {
            let handoff = matches!(
                &continuation,
                RunContinuation::Handoff(_)
                    | RunContinuation::Resume {
                        handoff: Some(_),
                        ..
                    }
            );
            let invocation = crate::AgentInvocationContext {
                btw: None,
                attempts: None,
                turn: None,
                run_id: run_id.to_string(),
                model_preference: config.model_preference.clone(),
                category: config.category.clone(),
                purpose: event_bus::RequestPurpose::Agent,
            };
            let model = runtime.shared.model_for(&config);
            let mut cancellation = execution.cancel_tx.subscribe();
            let admission = crate::panic_capture::catch_panic(async {
                tokio::select! {
                    biased;
                    _ = cancellation.wait_for(|signal| matches!(signal, RunInterrupt::Stop | RunInterrupt::Cancel)) =>
                        Err(RuntimeError::RunTerminated { run_id: run_id.to_string() }),
                    result = model.admit(&invocation, role) => result,
                }
            }).await;
            let admission_panicked = admission.is_err();
            let mut admitted = admission.unwrap_or_else(|panic| {
                if runtime.shared.quiesce.strict_restart_accounting() {
                    runtime
                        .shared
                        .quiesce
                        .fail(format!("provider admission panicked: {}", panic.message));
                }
                Err(RuntimeError::Model {
                    reason: format!("provider admission panicked: {}", panic.message),
                })
            });
            // Admission may outlive a lane handoff. Reject its old generation
            // before registration can schedule even the initial snapshot write.
            if lane_root
                && admitted.is_ok()
                && let Some(permit) = config.ownership.as_ref()
            {
                let interrupts = cancellation.clone();
                let retry = runtime
                    .shared
                    .ownership_retry
                    .get()
                    .copied()
                    .unwrap_or_else(crate::ownership::RetryPolicy::unbounded);
                admitted = permit
                    .validate_generation_off_thread(retry, move || {
                        interrupts.borrow().kind().is_some()
                    })
                    .await
                    .map_err(|error| {
                        tracing::debug!(%run_id, %error, "lane admission ownership rejected");
                        RuntimeError::StaleOwnership {
                            run_id: run_id.to_string(),
                        }
                    });
            }
            #[cfg(test)]
            if lane_root && admitted.is_ok() {
                runtime.execution_hook(cancellation::HookPoint::LaneOwnershipValidated, &execution);
            }
            #[cfg(test)]
            if admitted.is_err() {
                runtime.execution_hook(cancellation::HookPoint::AdmissionFailed, &execution);
            }
            #[cfg(test)]
            if admitted.is_ok() {
                runtime.execution_hook(cancellation::HookPoint::AdmissionReady, &execution);
            }
            // Registration and generation replacement share I -> A. Never
            // acquire a publication gate under these global locks.
            let intents = runtime
                .shared
                .spawn_intents
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let admissions = runtime
                .shared
                .admissions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let current = intents
                .get(&run_id)
                .and_then(|i| i.execution.as_ref())
                .is_some_and(|e| Arc::ptr_eq(e, &execution));
            if !current
                || execution.cancelled()
                || intents.get(&run_id).is_some_and(|i| i.cancelled)
                || admissions
                    .get(&run_id)
                    .is_none_or(|a| !Arc::ptr_eq(&a.execution, &execution) || a.cancelled)
            {
                admitted = Err(RuntimeError::RunTerminated {
                    run_id: run_id.to_string(),
                });
            }
            if admitted.is_ok() {
                runtime.register_run(
                    run_id,
                    parent,
                    role,
                    prompt,
                    config,
                    continuation,
                    review_seed,
                    Arc::clone(&execution),
                    &intents,
                );
            } else {
                runtime.drop_unstarted_host_input(run_id, config.initial_prompt_id.as_deref());
                // Release before notifying waiters; retries must observe the durable seed.
                drop(continuation);
            }
            // Stop must never see a registered entry as a pending admission:
            // publish success before releasing the registration/cancellation fence.
            if admitted.is_ok() {
                result.send_replace(Some(Ok(())));
            }
            // A task panic is not a provider refusal. Preserve that distinction
            // for handoff recovery while still retiring its durable incarnation.
            let failure = (handoff && !admission_panicked)
                .then(|| admitted.as_ref().err())
                .flatten();
            drop(admissions);
            let reviewer = if current && failure.is_some() {
                #[cfg(test)]
                runtime
                    .execution_hook(cancellation::HookPoint::AdmissionGoalCheckpoint, &execution);
                let mut goals = runtime.goal_lock();
                let reviewer = runtime.stop_admission_goal(run_id, goal_stamp.as_ref(), &mut goals);
                drop(goals);
                reviewer
                    .and_then(|reviewer| intents.get(&reviewer).and_then(|i| i.execution.clone()))
            } else {
                None
            };
            drop(intents);
            if admitted.is_err() && current {
                let reason = match execution.cancel_tx.borrow().kind() {
                    Some(crate::InterruptKind::Stop) => crate::quiesce::StopReason::OperatorStopped,
                    Some(crate::InterruptKind::Cancel) => crate::quiesce::StopReason::Cancelled,
                    None => crate::quiesce::StopReason::Terminal,
                };
                runtime.end_intent(run_id, &execution.publication().incarnation(), reason);
            }
            #[cfg(test)]
            if admitted.is_ok() {
                runtime.execution_hook(cancellation::HookPoint::AdmissionRegistered, &execution);
            }
            #[cfg(test)]
            if admitted.is_ok() {
                let hook = runtime
                    .shared
                    .admission_registration_hook
                    .lock()
                    .unwrap()
                    .clone();
                if let Some(hook) = hook {
                    hook(run_id);
                }
            }
            if let Some(reviewer) = reviewer {
                runtime.cancel_execution(&reviewer);
            }
            // A cancelled admission is the user's stop, not an admission failure.
            if let Some(error) = failure
                && !matches!(error, RuntimeError::RunTerminated { .. })
            {
                if let Some(store) = runtime.shared.run_store.get()
                    && let Err(write) = store
                        .handle
                        .note_handoff_start_failure(run_id.to_string(), error.to_string())
                {
                    tracing::warn!(%run_id,%write,"handoff admission failure receipt was not saved");
                }
                runtime
                    .shared
                    .bus
                    .emit(Event::new(event_bus::DiagnosticEvent {
                        source: "escalation_handoff".into(),
                        severity: event_bus::DiagnosticSeverity::Error,
                        code: diagnostic_codes::ESCALATION_ADMISSION_FAILED.into(),
                        detail: error.to_string(),
                        run_id: Some(run_id.to_string()),
                        thread_id: Some(event_bus::escalation_thread_id(&run_id.to_string())),
                        call_id: None,
                    }));
            }
            #[cfg(test)]
            runtime.execution_hook(cancellation::HookPoint::AdmissionCompleted, &execution);
            if admitted.is_err() {
                result.send_replace(Some(admitted));
            }
        });
        if lane_root {
            self.shared
                .lane_admissions
                .spawn(run_id, lane_execution, task);
        } else {
            tokio::spawn(task);
        }
        Ok(run_id)
    }

    /// No loop will append this input. A restored prompt was already delivered
    /// in its snapshot transaction, so the accepted-only update leaves it alone.
    pub(super) fn drop_unstarted_host_input(&self, run_id: RunId, prompt_id: Option<&str>) {
        if let Some(prompt_id) = prompt_id {
            self.settle_host_inputs(
                run_id,
                &[storage::HostInputTransition::new(
                    prompt_id,
                    storage::HostInputStatus::Dropped,
                    Some("spawn_failed"),
                )],
            );
        }
    }

    /// Preserve the parent boundary before provider admission registers the run.
    /// Never acquire admissions while holding runs: registration locks admissions first.
    pub(crate) fn admission_snapshot(&self, run_id: RunId) -> Option<AdmissionSnapshot> {
        let admissions = self
            .shared
            .admissions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        admissions.get(&run_id).map(Admission::snapshot)
    }

    pub(crate) async fn wait_admission(&self, run_id: RunId) -> Result<(), RuntimeError> {
        let receiver = self
            .shared
            .admissions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&run_id)
            .map(|admission| admission.result.clone());
        if let Some(mut receiver) = receiver {
            loop {
                if let Some(result) = receiver.borrow_and_update().clone() {
                    return result;
                }
                receiver.changed().await.map_err(|_| RuntimeError::Model {
                    reason: "provider admission was interrupted".into(),
                })?;
            }
        }
        Ok(())
    }
}

impl AgentRuntime {
    pub(crate) fn dispatch_run_settled(&self, run: RunId, error: &RuntimeError) -> bool {
        if matches!(
            error,
            RuntimeError::UnknownRun { .. } | RuntimeError::RunTerminated { .. }
        ) {
            return true;
        }
        self.shared
            .admissions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&run)
            .is_some_and(|admission| {
                admission
                    .result
                    .borrow()
                    .as_ref()
                    .is_some_and(|result| result.as_ref().is_err_and(|settled| settled == error))
            })
    }
    pub(crate) fn dispatch_pending_admissions(&self) -> Vec<RunId> {
        self.shared
            .admissions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .filter(|(_, a)| a.result.borrow().is_none())
            .map(|(id, _)| *id)
            .collect()
    }
    pub(crate) async fn join_dispatch_admissions(&self) -> Result<(), RuntimeError> {
        let pending: Vec<_> = self
            .shared
            .admissions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .filter(|a| a.result.borrow().is_none())
            .map(|a| a.result.clone())
            .collect();
        for mut result in pending {
            result
                .wait_for(Option::is_some)
                .await
                .map_err(|_| RuntimeError::Model {
                    reason: "dispatch admission ended without receipt".into(),
                })?;
        }
        Ok(())
    }
    pub(crate) fn try_seal_dispatch(
        &self,
        fence: &crate::quiesce::DispatchDrainFence,
    ) -> Result<bool, RuntimeError> {
        let restart = fence.is_restart();
        // Recovery and registration both use I -> A -> R.
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
        if restart
            && runs
                .values()
                .any(|entry| entry.execution.has_accepted_resume())
        {
            return Ok(false);
        }
        if !restart
            && (intents.values().any(|i| i.paused_recovery.is_some())
                || admissions.values().any(|a| a.result.borrow().is_none())
                || runs.values().any(is_live))
        {
            return Ok(false);
        }
        self.seal_dispatch_fence(fence)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Model;
    #[async_trait::async_trait]
    impl AgentModel for Model {
        async fn complete(
            &self,
            _: &crate::AgentInvocationContext,
            _: Role,
            _: &[providers::Message],
            _: &[providers::ToolSpec],
        ) -> Result<providers::ChatResponse, RuntimeError> {
            panic!("a stopped handoff must not call its provider");
        }
        fn selected_model(&self, _: Role, _: Option<&str>) -> String {
            "test".into()
        }
    }

    #[tokio::test]
    async fn closed_background_registration_settles_admission_and_shutdown_only_ignores_settled_errors()
     {
        let bus = Arc::new(EventBus::new(32));
        let runtime = AgentRuntime::new(
            bus.clone(),
            Arc::new(ToolExecutor::new(bus)),
            Arc::new(Model),
        );
        assert!(runtime.shared.dispatch_tails.try_close());
        let run =
            runtime.delegate_background(Role::Worker, "rejected".into(), RunConfig::default());
        let error = runtime.wait(run).await.unwrap_err();
        assert!(matches!(error, RuntimeError::Model { .. }), "{error:?}");
        assert!(runtime.dispatch_run_settled(run, &error));
        assert!(!runtime.dispatch_run_settled(
            run,
            &RuntimeError::Model {
                reason: "unrelated storage failure".into()
            }
        ));
        assert!(runtime.dispatch_run_settled(
            run,
            &RuntimeError::UnknownRun {
                run_id: run.to_string()
            }
        ));
        assert!(runtime.dispatch_run_settled(
            run,
            &RuntimeError::RunTerminated {
                run_id: run.to_string()
            }
        ));
        assert!(runtime.list_agents().is_empty());
        let fence = runtime
            .fence_dispatch_shutdown(|| async { Ok::<_, RuntimeError>(()) })
            .await
            .unwrap();
        runtime.drain_dispatch_tails(&fence).await.unwrap();
    }

    #[tokio::test]
    async fn stopped_handoff_reservation_never_registers_without_provider_admission() {
        let bus = Arc::new(EventBus::new(32));
        let runtime = AgentRuntime::new(
            bus.clone(),
            Arc::new(ToolExecutor::new(bus)),
            Arc::new(Model),
        );
        let root = runtime.reserve_run_id();
        runtime.reserve_admission(root, None, None);
        runtime.stop(root, StopScope::SelfOnly).unwrap();
        runtime.spawn_reserved(
            root,
            None,
            Role::Orchestrator,
            "pending handoff",
            RunConfig::default(),
        );
        assert!(matches!(
            runtime.wait(root).await,
            Err(RuntimeError::RunTerminated { .. })
        ));
        assert!(runtime.list_agents().is_empty());
    }
}
