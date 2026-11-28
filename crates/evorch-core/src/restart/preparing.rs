use super::*;
use runtime::learning_suspend::LearningSuspendWait;

// CPU + I/O loaded cohort max: 2.714s; 10s exceeds 2x p99 and leaves 3.68x
// observed-max margin. See measurements/4b-d-2b/README.md for methods/limits.
pub(super) const LEARNING_COHORT_LIMIT: std::time::Duration = std::time::Duration::from_secs(10);

/// How many times one request may observe the recipes again after a run changed
/// state under it. Past this the request is cancelled, as it always was.
pub(super) const OBSERVATION_ATTEMPTS: usize = 100;

/// Between two observations: a run that is finishing a turn settles in
/// milliseconds, and spinning would only burn the attempts inside that change.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const OBSERVATION_PAUSE: std::time::Duration = std::time::Duration::from_millis(20);

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
enum Observed {
    Begun(RestartAttempt),
    Cancel(PreparationFailure, Option<RunId>),
    Again,
}

impl RestartCoordinator {
    async fn prepare_drains(&self, id: RestartId) -> Result<Observed, RuntimeError> {
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
                return Ok(Observed::Cancel(PreparationFailure::Deadline, None));
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
            state.preparation_stage = PreparationStage::LearningDrain;
            self.publish(&mut state, RestartPhase::PreparingDrains);
            (suspension, deadline, cohort_deadline)
        };
        // One absolute deadline includes the fence writes and every member's return.
        match suspension
            .wait_until(cohort_deadline)
            .await
            .map_err(|reason| RuntimeError::Model { reason })?
        {
            LearningSuspendWait::DeadlineExceeded => {
                return Ok(Observed::Cancel(PreparationFailure::Deadline, None));
            }
            LearningSuspendWait::Complete => {}
        }
        // A run changing state while the recipes are observed is ordinary: the
        // whole observation starts again from the same checks (no gate, park,
        // capture or transaction exists yet). It is bounded in count and by the
        // request's own deadline; past either the request is cancelled.
        self.preparation_stage(id, PreparationStage::RecipeObservation);
        for _ in 0..OBSERVATION_ATTEMPTS {
            match self.observe_and_begin(id, deadline).await? {
                Observed::Again => tokio::time::sleep(OBSERVATION_PAUSE).await,
                result => return Ok(result),
            }
        }
        Ok(Observed::Cancel(
            PreparationFailure::ObservationExhausted,
            None,
        ))
    }

    /// One pass of recipe observation, DB preflight and the begin of the
    /// quiesce. Nothing here is carried over to the next pass.
    async fn observe_and_begin(
        &self,
        id: RestartId,
        deadline: tokio::time::Instant,
    ) -> Result<Observed, RuntimeError> {
        let mut recipes = {
            let state = self.0.state.lock().unwrap();
            if state.id != Some(id) || state.phase != RestartPhase::PreparingDrains {
                return Err(RuntimeError::RestartBusy);
            }
            if tokio::time::Instant::now() >= deadline {
                return Ok(Observed::Cancel(PreparationFailure::Deadline, None));
            }
            state.recipes.clone()
        };
        #[cfg(target_os = "linux")]
        let source = self.0.state.lock().unwrap().recipe_source.clone();
        #[cfg(target_os = "linux")]
        let mut binding_observation = None;
        #[cfg(target_os = "linux")]
        let mut recipe_observation = None;
        #[cfg(target_os = "linux")]
        if let Some(expected) = &source {
            if !expected.bindings.is_current() {
                return Ok(Observed::Cancel(PreparationFailure::BindingChanged, None));
            }
            let observation = self.0.runtime.observe_restart_recipes();
            // This is still reversible: no host Preparing gate, park or tx.
            // Reject the whole request, not only the newly unsupported member.
            if let Err(error) = self
                .0
                .runtime
                .check_restart_preflight_authority(&expected.preflight)
            {
                return Ok(observation_failure(&error));
            }
            let observed = match expected.source.collect().await {
                Ok(observed) => observed,
                Err(e) if super::public::ordinary_recipe_progress(&e) => {
                    return Ok(Observed::Again);
                }
                Err(error) => {
                    use workbench_protocol::errors::ErrorCode;
                    let reason = match error.code {
                        ErrorCode::Conflict => PreparationFailure::BindingChanged,
                        ErrorCode::StorageUnavailable | ErrorCode::Busy => {
                            PreparationFailure::Storage
                        }
                        _ => PreparationFailure::RecipeUnavailable,
                    };
                    return Ok(Observed::Cancel(reason, None));
                }
            };
            if !expected.bindings.same_revision(&observed.bindings)
                || expected.ledgers != observed.ledgers
            {
                return Ok(Observed::Cancel(PreparationFailure::BindingChanged, None));
            }
            // The members and what is done with each are fixed by the request. A
            // member's restore descriptor is not: it moves on whenever the run
            // finishes a turn, and the capture must restore the run as it is now.
            let Some(requested) = recipes.as_mut() else {
                return Ok(Observed::Cancel(
                    PreparationFailure::RecipeUnavailable,
                    None,
                ));
            };
            if !same_members(requested, &observed.recipes) {
                return Ok(Observed::Cancel(PreparationFailure::MemberChanged, None));
            }
            if *requested != observed.recipes {
                let mut state = self.0.state.lock().unwrap();
                if state.id != Some(id) || state.phase != RestartPhase::PreparingDrains {
                    return Err(RuntimeError::RestartBusy);
                }
                state.recipes = Some(observed.recipes.clone());
                *requested = observed.recipes;
            }
            binding_observation = Some(observed.bindings);
            recipe_observation = Some(observation);
        }
        let preflight = if let Some(recipes) = recipes {
            let runtime = self.0.runtime.clone();
            #[cfg(test)]
            let hook = self.0.preflight_hook.lock().unwrap().clone();
            let result = tokio::task::spawn_blocking(move || {
                #[cfg(test)]
                if let Some(hook) = hook {
                    hook(id);
                }
                #[cfg(target_os = "linux")]
                if let Some(observation) = recipe_observation {
                    return runtime.prepare_restart_quiesce_observed(&recipes, observation);
                }
                runtime.prepare_restart_quiesce(&recipes)
            })
            .await
            .map_err(|e| RuntimeError::Model {
                reason: e.to_string(),
            })?;
            #[cfg(target_os = "linux")]
            if source.is_some() {
                match &result {
                    Err(RuntimeError::RestartObservationChanged) => return Ok(Observed::Again),
                    Err(error) => return Ok(observation_failure(error)),
                    Ok(_) => {}
                }
            }
            Some(result?)
        } else {
            None
        };
        let begin = || {
            // Binding -> coordinator -> runtime registration locks use the same
            // order as commit. No I/O/await lies inside this final transition.
            let mut state = self.0.state.lock().unwrap();
            if state.id != Some(id) || state.phase != RestartPhase::PreparingDrains {
                return Err(RuntimeError::RestartBusy);
            }
            if tokio::time::Instant::now() >= deadline {
                return Ok(Observed::Cancel(PreparationFailure::Deadline, None));
            }
            let attempt = if let Some(preflight) = preflight {
                self.0.runtime.begin_preflighted_restart_quiesce(preflight)
            } else {
                self.0.runtime.begin_restart_quiesce()
            };
            #[cfg(target_os = "linux")]
            if source.is_some() {
                match &attempt {
                    Err(RuntimeError::RestartObservationChanged) => return Ok(Observed::Again),
                    Err(error) => return Ok(observation_failure(error)),
                    Ok(_) => {}
                }
            }
            let attempt = attempt?;
            state.attempt = Some(attempt.clone());
            state.pauses = self.0.producers.iter().map(ProducerGate::pause).collect();
            self.0
                .gate
                .transition(id.generation, HostRestartPhase::Preparing);
            self.publish(&mut state, RestartPhase::Requested);
            Ok(Observed::Begun(attempt))
        };
        #[cfg(target_os = "linux")]
        if let Some(bindings) = binding_observation {
            // Runtime's existing registration exclusion covers new live members;
            // this guard independently excludes host binding publication.
            return bindings.with_current(begin).unwrap_or(Ok(Observed::Cancel(
                PreparationFailure::BindingChanged,
                None,
            )));
        }
        begin()
    }

    pub(super) async fn drive_preparing(&self, id: RestartId) {
        let deadline = {
            let state = self.0.state.lock().unwrap();
            if state.id != Some(id) {
                return;
            }
            state.preparation_deadline.unwrap()
        };
        let drained = tokio::select! {
            drained = tokio::time::timeout_at(deadline, self.prepare_drains(id)) => drained,
            pending = approval_raised(&self.0.runtime) => {
                let begun = self.0.state.lock().unwrap().attempt.clone();
                match begun {
                    Some(attempt) => self.drive(id, attempt).await,
                    None => self.cancel_for_approvals(id, pending),
                }
                return;
            }
        };
        match drained {
            Ok(Ok(Observed::Begun(attempt))) => self.drive(id, attempt).await,
            Ok(Ok(Observed::Cancel(reason, run))) => self.cancel_for_failure(id, reason, run),
            Err(_) => self.cancel_for_failure(id, PreparationFailure::Deadline, None),
            Ok(Ok(Observed::Again)) => unreachable!("observation loop returns a terminal result"),
            Ok(Err(error)) => {
                let mut state = self.0.state.lock().unwrap();
                if state.id != Some(id) || state.phase != RestartPhase::PreparingDrains {
                    return;
                }
                let reason = if self.0.runtime.check_restart_preparation().is_err() {
                    PreparationFailure::Storage
                } else {
                    PreparationFailure::DrainFailed
                };
                let run = match error {
                    RuntimeError::RestartRunUnavailable { run_id, .. } => Some(run_id),
                    _ => None,
                };
                self.record_preparation_failure(&mut state, reason, run);
                state.pauses = self.0.producers.iter().map(ProducerGate::pause).collect();
                self.0
                    .gate
                    .transition(id.generation, HostRestartPhase::FailedClosed);
                self.publish(&mut state, RestartPhase::FailedClosed);
            }
        }
    }
}

/// Same runs with the same policy, goal, team and lane; descriptors may differ.
#[cfg(target_os = "linux")]
pub(super) fn same_members(left: &public::Recipes, right: &public::Recipes) -> bool {
    let stripped = |recipes: &public::Recipes| {
        let mut recipes = recipes.clone();
        recipes
            .values_mut()
            .for_each(|recipe| recipe.descriptor = None);
        serde_json::to_string(&recipes).ok()
    };
    let left = stripped(left);
    left.is_some() && left == stripped(right)
}

#[cfg(target_os = "linux")]
fn observation_failure(error: &RuntimeError) -> Observed {
    match error {
        RuntimeError::RestartAuthorityChanged { run_id } => {
            Observed::Cancel(PreparationFailure::MemberChanged, Some(*run_id))
        }
        RuntimeError::RestartRunUnavailable { run_id, .. } => {
            Observed::Cancel(PreparationFailure::RecipeUnavailable, Some(*run_id))
        }
        _ => Observed::Cancel(PreparationFailure::Storage, None),
    }
}
