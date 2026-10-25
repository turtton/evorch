use super::*;
use event_bus::event::diagnostic_codes;

pub(super) type Admissions = Mutex<HashMap<RunId, Admission>>;

pub(super) struct Admission {
    parent: Option<RunId>,
    pub(super) cancelled: bool,
    pub(super) execution: Arc<ExecutionControl>,
    ownership: Option<crate::ownership::OwnerPermit>,
    completion: Option<watch::Sender<Option<Result<(), RuntimeError>>>>,
    pub(super) result: watch::Receiver<Option<Result<(), RuntimeError>>>,
}

pub(super) struct AdmissionSnapshot {
    pub parent: Option<RunId>,
    pub ownership: Option<crate::ownership::OwnerPermit>,
    pub result: Option<Result<(), RuntimeError>>,
}

impl Admission {
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
    pub(super) fn fail_review_admission(
        &self,
        run_id: RunId,
        parent: Option<RunId>,
        config: &RunConfig,
        reason: String,
    ) {
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
            completion.send_replace(Some(Err(Self::review_error(run_id, reason))));
        }
    }

    /// Reserve before publishing a handoff, so cancellation already has a
    /// generation-bound channel before provider admission or RunEntry exists.
    pub(super) fn reserve_admission(
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
        self.reserve_admission_for(run_id, parent, ownership, Arc::clone(&execution));
        execution
    }

    fn reserve_admission_for(
        &self,
        run_id: RunId,
        parent: Option<RunId>,
        ownership: Option<crate::ownership::OwnerPermit>,
        execution: Arc<ExecutionControl>,
    ) {
        let mut admissions = self
            .shared
            .admissions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if admissions
            .get(&run_id)
            .is_some_and(|a| Arc::ptr_eq(&a.execution, &execution) && a.snapshot().result.is_none())
        {
            return;
        }
        let (completion, result) = watch::channel(None);
        admissions.insert(
            run_id,
            Admission {
                parent,
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
    pub(super) fn update_admission_owner(
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

    pub(super) fn fail_reserved_admission(
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
    ) -> RunId {
        assert!(
            intents
                .get(&run_id)
                .and_then(|i| i.execution.as_ref())
                .is_some_and(|current| Arc::ptr_eq(current, &execution))
        );
        self.reserve_admission_for(
            run_id,
            parent,
            config.ownership.clone(),
            Arc::clone(&execution),
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
        let runtime = self.clone();
        tokio::spawn(async move {
            let handoff = matches!(
                &continuation,
                RunContinuation::Handoff(_)
                    | RunContinuation::Resume {
                        handoff: Some(_),
                        ..
                    }
            );
            let invocation = crate::AgentInvocationContext {
                turn: None,
                run_id: run_id.to_string(),
                model_preference: config.model_preference.clone(),
                category: config.category.clone(),
                purpose: event_bus::RequestPurpose::Agent,
            };
            let mut admitted = runtime
                .shared
                .model_for(&config)
                .admit(&invocation, role)
                .await;
            #[cfg(test)]
            if admitted.is_err() {
                runtime.execution_hook(cancellation::HookPoint::AdmissionFailed, &execution);
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
                // Release before notifying waiters; retries must observe the durable seed.
                drop(continuation);
            }
            let failure = handoff.then(|| admitted.as_ref().err()).flatten();
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
            #[cfg(test)]
            if admitted.is_ok() {
                runtime.execution_hook(cancellation::HookPoint::AdmissionRegistered, &execution);
            }
            if let Some(reviewer) = reviewer {
                runtime.cancel_execution(&reviewer);
            }
            // A cancelled admission is the user's stop, not an admission failure.
            if let Some(error) = failure
                && !matches!(error, RuntimeError::RunTerminated { .. })
            {
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
            result.send_replace(Some(admitted));
        });
        run_id
    }

    /// Preserve the parent boundary before provider admission registers the run.
    /// Never acquire admissions while holding runs: registration locks admissions first.
    pub(super) fn admission_snapshot(&self, run_id: RunId) -> Option<AdmissionSnapshot> {
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
