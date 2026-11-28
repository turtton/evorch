use super::*;

const ADMISSION_OBSERVATIONS: usize = 5;

pub(super) struct RecipeProof {
    pub(super) recipes: Recipes,
    pub(super) preflight: Arc<runtime::RestartPreflight>,
    pub(super) ledgers: RecipeLedgers,
    pub(super) bindings: workbench_host::lane::BindingObservation,
}

pub(crate) struct RecipeSourceProof {
    pub source: Arc<dyn RecipeSource>,
    pub bindings: workbench_host::lane::BindingObservation,
    pub ledgers: RecipeLedgers,
    pub preflight: Arc<runtime::RestartPreflight>,
}

impl PublicRestartCoordinator {
    pub(super) async fn observe_preflight(
        &self,
        expected: Option<&RecipeProof>,
    ) -> Result<RecipeProof, AdminFailure> {
        let runtime = &self.core.0.runtime;
        let baseline = runtime.observe_restart_recipes();
        let mut binding = expected.map(|p| p.bindings.clone());
        let mut ledgers = expected.map(|p| p.ledgers.clone());
        for _ in 0..ADMISSION_OBSERVATIONS {
            if binding.as_ref().is_some_and(|p| !p.is_current()) {
                return Err(binding_changed());
            }
            runtime
                .check_restart_recipe_authority(&baseline)
                .map_err(preflight_error)?;
            if let Some(expected) = expected {
                runtime
                    .check_restart_preflight_authority(&expected.preflight)
                    .map_err(preflight_error)?;
            }
            let observation = runtime.observe_restart_recipes();
            let observed = match self.recipes.collect().await {
                Ok(observed) => observed,
                Err(error) if ordinary_recipe_progress(&error) => continue,
                Err(error) => return Err(error),
            };
            if !observed.bindings.is_current()
                || binding
                    .as_ref()
                    .is_some_and(|p| !p.same_revision(&observed.bindings))
            {
                return Err(binding_changed());
            }
            if ledgers.as_ref().is_some_and(|p| *p != observed.ledgers) {
                return Err(refused("lane generation or thread/archive ledger changed"));
            }
            if expected.is_some_and(|p| {
                !crate::restart::preparing::same_members(&p.recipes, &observed.recipes)
            }) {
                return Err(refused("recipe members or ownership changed"));
            }
            binding.get_or_insert_with(|| observed.bindings.clone());
            ledgers.get_or_insert_with(|| observed.ledgers.clone());
            let runtime_for_db = runtime.clone();
            let recipes = observed.recipes;
            let (recipes, preflight) = tokio::task::spawn_blocking(move || {
                let preflight =
                    runtime_for_db.prepare_restart_quiesce_observed(&recipes, observation);
                (recipes, preflight)
            })
            .await
            .map_err(|_| failed())?;
            if !observed.bindings.is_current() {
                return Err(binding_changed());
            }
            runtime
                .check_restart_recipe_authority(&baseline)
                .map_err(preflight_error)?;
            if let Some(expected) = expected {
                runtime
                    .check_restart_preflight_authority(&expected.preflight)
                    .map_err(preflight_error)?;
            }
            match preflight {
                Ok(preflight) => {
                    return Ok(RecipeProof {
                        recipes,
                        preflight: Arc::new(preflight),
                        bindings: observed.bindings,
                        ledgers: observed.ledgers,
                    });
                }
                Err(runtime::RuntimeError::RestartObservationChanged) => continue,
                Err(error) => return Err(preflight_error(error)),
            }
        }
        Err(refused("normal progress exhausted the observation budget"))
    }

    pub(super) async fn refresh_prepared(
        &self,
        binding: &RestartBinding,
    ) -> Result<(), AdminFailure> {
        // Re-read the configured allowlist outside all transition locks. A
        // changed target is a refusal, never a reason to probe a replacement.
        self.targets.targets(self.epoch).await?;
        self.check_request(binding)?;
        let mut prepared = {
            let mut state = self.state.lock().unwrap();
            if self.canonical(&state, binding)?.is_some() {
                return Ok(());
            }
            let position = state
                .prepared
                .iter()
                .position(|p| p.binding == *binding)
                .ok_or_else(|| refused("inspection proof missing or expired"))?;
            state.prepared.remove(position).unwrap()
        };
        if !prepared.proof.bindings.is_current() {
            return Err(binding_changed());
        }
        match self
            .core
            .0
            .runtime
            .validate_restart_preflight(&prepared.proof.preflight, &prepared.proof.recipes)
        {
            Ok(()) => {}
            Err(runtime::RuntimeError::RestartObservationChanged) => {
                prepared.proof = self.observe_preflight(Some(&prepared.proof)).await?;
            }
            Err(error) => return Err(preflight_error(error)),
        }
        prepared.ledger_fence = self
            .recipes
            .verify_ledgers(prepared.proof.ledgers.clone())
            .await?;
        let mut state = self.state.lock().unwrap();
        if state.prepared.len() == 32 {
            state.prepared.pop_front();
        }
        state.prepared.push_back(prepared);
        Ok(())
    }
}

pub(super) fn refused(reason: &str) -> AdminFailure {
    error(ErrorCode::Conflict, &format!("restart refused: {reason}"))
}

pub(super) fn binding_changed() -> AdminFailure {
    refused("current chat, goal, project or lane authority changed")
}

pub(crate) fn ledger_changed() -> AdminFailure {
    refused("lane generation or thread/archive ledger changed")
}

pub(crate) fn ordinary_recipe_progress(error: &AdminFailure) -> bool {
    error.code == ErrorCode::Conflict && error.retry == RetryAdvice::AfterResync
}

pub(super) fn preflight_error(failure: runtime::RuntimeError) -> AdminFailure {
    use runtime::RuntimeError::*;
    match failure {
        RestartRunUnavailable { .. } => {
            error(ErrorCode::UnsupportedCapability, &failure.to_string())
        }
        RestartAuthorityChanged { .. } => refused(&failure.to_string()),
        RestartObservationChanged => {
            refused("observation changed at the commit boundary; inspect again")
        }
        RestartBusy => refused("lifecycle admission is no longer open"),
        _ => error(
            ErrorCode::StorageUnavailable,
            "restart storage preflight failed",
        ),
    }
}
