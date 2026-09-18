use super::*;

pub(super) type Admissions = Mutex<HashMap<RunId, Admission>>;

pub(super) struct Admission {
    cancelled: bool,
    result: watch::Receiver<Option<Result<(), RuntimeError>>>,
}

impl AgentRuntime {
    // The synchronous API reserves an ID; only successful admission registers a run.
    pub(super) fn admit_run(
        &self,
        run_id: RunId,
        parent: Option<RunId>,
        role: Role,
        prompt: String,
        config: RunConfig,
        handoff: Option<RunHandoff>,
    ) -> RunId {
        let (result, receiver) = watch::channel(None);
        self.shared
            .admissions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                run_id,
                Admission {
                    cancelled: false,
                    result: receiver,
                },
            );
        let runtime = self.clone();
        tokio::spawn(async move {
            let invocation = crate::AgentInvocationContext {
                run_id: run_id.to_string(),
                model_preference: config.model_preference.clone(),
                category: config.category.clone(),
            };
            let mut admitted = runtime.shared.model.admit(&invocation, role).await;
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
                runtime.register_run(run_id, parent, role, prompt, config, handoff);
            }
            result.send_replace(Some(admitted));
        });
        run_id
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
        sender.send_replace(true);
        Ok(())
    }
}
