use super::*;

pub(super) type Admissions =
    Mutex<HashMap<RunId, watch::Receiver<Option<Result<(), RuntimeError>>>>>;

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
            .insert(run_id, receiver);
        let runtime = self.clone();
        tokio::spawn(async move {
            let invocation = crate::AgentInvocationContext {
                run_id: run_id.to_string(),
                model_preference: config.model_preference.clone(),
                category: config.category.clone(),
            };
            let admitted = runtime.shared.model.admit(&invocation, role).await;
            if admitted.is_ok() {
                runtime.register_run(run_id, parent, role, prompt, config, handoff);
            }
            result.send_replace(Some(admitted));
        });
        run_id
    }

    pub(super) async fn wait_admission(&self, run_id: RunId) -> Result<(), RuntimeError> {
        let receiver = self
            .shared
            .admissions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&run_id)
            .cloned();
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
