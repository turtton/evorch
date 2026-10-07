use super::*;

pub(super) type Admissions = Mutex<HashMap<RunId, Admission>>;

pub(super) struct Admission {
    parent: Option<RunId>,
    pub(super) cancelled: bool,
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
    /// Establish an awaitable identity before publishing a handoff. Provider
    /// admission reuses this channel so event consumers can immediately wait.
    pub(super) fn reserve_admission(
        &self,
        run_id: RunId,
        parent: Option<RunId>,
        ownership: Option<crate::ownership::OwnerPermit>,
    ) {
        let mut admissions = self
            .shared
            .admissions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(admission) = admissions.get(&run_id) {
            let pending = admission.snapshot().result.is_none();
            if pending {
                return;
            }
        }
        let (completion, result) = watch::channel(None);
        admissions.insert(
            run_id,
            Admission {
                parent,
                cancelled: false,
                ownership,
                completion: Some(completion),
                result,
            },
        );
    }

    pub(super) fn register_without_admission(
        &self,
        run_id: RunId,
        parent: Option<RunId>,
        role: Role,
        prompt: String,
        config: RunConfig,
        continuation: RunContinuation,
    ) -> RunId {
        let mut admissions = self
            .shared
            .admissions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let cancelled = admissions
            .get(&run_id)
            .is_some_and(|admission| admission.result.borrow().is_none() && admission.cancelled);
        if !cancelled {
            self.register_run(run_id, parent, role, prompt, config, continuation);
        }
        if let Some(admission) = admissions.get_mut(&run_id)
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
    pub(super) fn admit_run(
        &self,
        run_id: RunId,
        parent: Option<RunId>,
        role: Role,
        prompt: String,
        config: RunConfig,
        continuation: RunContinuation,
    ) -> RunId {
        self.reserve_admission(run_id, parent, config.ownership.clone());
        let result = self
            .shared
            .admissions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get_mut(&run_id)
            .and_then(|admission| admission.completion.take())
            .expect("reserved admission has one completion sender");
        let runtime = self.clone();
        tokio::spawn(async move {
            let handoff = matches!(&continuation, RunContinuation::Handoff(_));
            let invocation = crate::AgentInvocationContext {
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
            // Cancellation and registration share this fence; no await inside it.
            let admissions = runtime
                .shared
                .admissions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if admissions
                .get(&run_id)
                .is_some_and(|admission| admission.cancelled)
            {
                admitted = Err(RuntimeError::RunTerminated {
                    run_id: run_id.to_string(),
                });
            }
            if admitted.is_ok() {
                runtime.register_run(run_id, parent, role, prompt, config, continuation);
            }
            let failure = handoff
                .then(|| admitted.as_ref().err().map(ToString::to_string))
                .flatten();
            drop(admissions);
            if let Some(reason) = failure {
                runtime.goal_work_stopped(run_id);
                runtime
                    .shared
                    .bus
                    .emit(Event::new(event_bus::DiagnosticEvent {
                        source: "escalation_handoff".into(),
                        severity: event_bus::DiagnosticSeverity::Error,
                        code: "EscalationAdmissionFailed".into(),
                        detail: reason,
                        run_id: Some(run_id.to_string()),
                        thread_id: Some(event_bus::escalation_thread_id(&run_id.to_string())),
                        call_id: None,
                    }));
            }
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

    /// run へキャンセルを通知する。admission 待機中なら登録を抑止する。
    pub fn cancel(&self, run_id: RunId) -> Result<(), RuntimeError> {
        let mut admissions = self
            .shared
            .admissions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(admission) = admissions.get_mut(&run_id)
            && admission.result.borrow().is_none()
        {
            admission.cancelled = true;
            return Ok(());
        }
        let sender = self.entry(run_id)?.cancel_tx.clone();
        sender.send_replace(RunInterrupt::Cancel);
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
