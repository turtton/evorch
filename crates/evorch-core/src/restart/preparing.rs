use super::*;
use runtime::learning_suspend::LearningSuspendWait;

// CPU + I/O loaded cohort max: 2.714s; 10s exceeds 2x p99 and leaves 3.68x
// observed-max margin. See measurements/4b-d-2b/README.md for methods/limits.
pub(super) const LEARNING_COHORT_LIMIT: std::time::Duration = std::time::Duration::from_secs(10);

impl RestartCoordinator {
    async fn prepare_drains(&self, id: RestartId) -> Result<Option<RestartAttempt>, RuntimeError> {
        self.0.preparing.evaluations.producers.drained().await;
        self.0
            .preparing
            .evaluations
            .drain_accepted()
            .await
            .map_err(|reason| RuntimeError::Model { reason })?;
        let (suspension, deadline, cohort_deadline) = {
            let mut state = self.0.state.lock().unwrap();
            if state.id != Some(id) || state.phase != RestartPhase::PreparingDrains {
                return Err(RuntimeError::RestartBusy);
            }
            let deadline = state.preparation_deadline.unwrap();
            if tokio::time::Instant::now() >= deadline {
                return Ok(None);
            }
            // Runtime registers each entry before acquiring its learning permit.
            // Pausing therefore fixes all admitted pipelines in this cohort;
            // later registrations cannot enter. A permit spans the pipeline:
            // draining it here would wait for the response suspend must cancel.
            let cohort_deadline =
                (tokio::time::Instant::now() + LEARNING_COHORT_LIMIT).min(deadline);
            let suspension = Arc::new(
                self.0
                    .runtime
                    .suspend_learning(state.learning_pause.as_ref().unwrap())
                    .map_err(|reason| RuntimeError::Model { reason })?,
            );
            state.suspension = Some(suspension.clone());
            self.publish(&mut state, RestartPhase::PreparingDrains);
            (suspension, deadline, cohort_deadline)
        };
        // One absolute deadline includes the fence writes and every member's return.
        match suspension
            .wait_until(cohort_deadline)
            .await
            .map_err(|reason| RuntimeError::Model { reason })?
        {
            LearningSuspendWait::DeadlineExceeded => return Ok(None),
            LearningSuspendWait::Complete => {}
        }
        let recipes = {
            let state = self.0.state.lock().unwrap();
            if state.id != Some(id) || state.phase != RestartPhase::PreparingDrains {
                return Err(RuntimeError::RestartBusy);
            }
            state.recipes.clone()
        };
        let preflight = if let Some(recipes) = recipes {
            let runtime = self.0.runtime.clone();
            #[cfg(test)]
            let hook = self.0.preflight_hook.lock().unwrap().clone();
            Some(
                tokio::task::spawn_blocking(move || {
                    #[cfg(test)]
                    if let Some(hook) = hook {
                        hook(id);
                    }
                    runtime.prepare_restart_quiesce(&recipes)
                })
                .await
                .map_err(|e| RuntimeError::Model {
                    reason: e.to_string(),
                })??,
            )
        } else {
            None
        };
        let mut state = self.0.state.lock().unwrap();
        if state.id != Some(id) || state.phase != RestartPhase::PreparingDrains {
            return Err(RuntimeError::RestartBusy);
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(None);
        }
        // Cancellation, the final health check, attempt creation and host Preparing
        // share this mutex. No await leaves a reservation/attempt gap.
        let attempt = if let Some(preflight) = preflight {
            self.0
                .runtime
                .begin_preflighted_restart_quiesce(preflight)?
        } else {
            self.0.runtime.begin_restart_quiesce()?
        };
        state.attempt = Some(attempt.clone());
        state.pauses = self.0.producers.iter().map(ProducerGate::pause).collect();
        self.0
            .gate
            .transition(id.generation, HostRestartPhase::Preparing);
        self.publish(&mut state, RestartPhase::Requested);
        Ok(Some(attempt))
    }

    pub(super) async fn drive_preparing(&self, id: RestartId) {
        let deadline = {
            let state = self.0.state.lock().unwrap();
            if state.id != Some(id) {
                return;
            }
            state.preparation_deadline.unwrap()
        };
        match tokio::time::timeout_at(deadline, self.prepare_drains(id)).await {
            Ok(Ok(Some(attempt))) => self.drive(id, attempt).await,
            Ok(Ok(None)) | Err(_) => {
                let _ = self.cancel(id);
            }
            Ok(Err(_)) => {
                let mut state = self.0.state.lock().unwrap();
                if state.id != Some(id) || state.phase != RestartPhase::PreparingDrains {
                    return;
                }
                state.pauses = self.0.producers.iter().map(ProducerGate::pause).collect();
                self.0
                    .gate
                    .transition(id.generation, HostRestartPhase::FailedClosed);
                self.publish(&mut state, RestartPhase::FailedClosed);
            }
        }
    }
}
