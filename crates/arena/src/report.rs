use crate::{ArenaError, EvalTrace, select};

#[derive(Debug, Clone, Default)]
pub struct ArenaReport {
    pub(crate) traces: Vec<EvalTrace>,
}

#[derive(Debug, Clone, Copy)]
pub enum Confirmation {
    Approved,
    Declined,
}

impl ArenaReport {
    pub fn traces(&self) -> &[EvalTrace] {
        &self.traces
    }

    pub fn selected(&self) -> Vec<String> {
        if self.comparison_complete() {
            select(&self.traces)
        } else {
            Vec::new()
        }
    }

    pub(crate) fn comparison_complete(&self) -> bool {
        let Some(first) = self.traces.first() else {
            return false;
        };
        let Ok(spec) = serde_json::from_str::<crate::ArenaSpec>(&first.task_spec) else {
            // Legacy traces do not prove which configurations were requested.
            return false;
        };
        spec.validate().is_ok()
            && spec.id == first.arena_id
            && spec.project == first.project
            && spec.task.id == first.task_id
            && spec.configs.len() == self.traces.len()
            && spec.configs.iter().all(|config| {
                self.traces.iter().any(|trace| {
                    trace.config_id == config.id
                        && trace.profile == config.profile
                        && trace.model == config.model
                        && trace.attribution == config.attribution
                        && execution_matches(config, trace)
                        && matches!(
                            trace.failure,
                            None | Some(crate::FailureAttribution::OutputMismatch)
                        )
                })
            })
    }

    pub fn from_traces(traces: Vec<EvalTrace>) -> Result<Self, ArenaError> {
        let mut ids = std::collections::BTreeSet::new();
        if let Some(first) = traces.first() {
            for trace in &traces {
                if trace.arena_id != first.arena_id
                    || trace.project != first.project
                    || trace.task_id != first.task_id
                    || trace.task_spec != first.task_spec
                    || !ids.insert(&trace.config_id)
                {
                    return Err(ArenaError::InvalidSpec(
                        "comparison requires one task and distinct configurations",
                    ));
                }
            }
        }
        Ok(Self { traces })
    }

    /// Returns a proposal only; never mutates the active routing table.
    pub fn promote(
        &self,
        id: &str,
        confirmation: Confirmation,
    ) -> Result<config::RouteCandidateConfig, ArenaError> {
        let config = self.promote_config(id, confirmation)?;
        if config.variant != crate::ArenaVariant::default() {
            return Err(ArenaError::InvalidSpec(
                "variant promotion requires promote_config",
            ));
        }
        Ok(config::RouteCandidateConfig {
            profile: config.profile,
            model: Some(config.model),
        })
    }

    pub fn promote_config(
        &self,
        id: &str,
        confirmation: Confirmation,
    ) -> Result<crate::ArenaConfig, ArenaError> {
        match confirmation {
            Confirmation::Declined => return Err(ArenaError::ConfirmationRequired),
            Confirmation::Approved => {}
        }
        if !self.selected().iter().any(|selected| selected == id) {
            return Err(ArenaError::Ineligible);
        }
        let trace = self
            .traces
            .iter()
            .find(|t| t.config_id == id)
            .ok_or(ArenaError::Ineligible)?;
        let spec: crate::ArenaSpec =
            serde_json::from_str(&trace.task_spec).map_err(|_| ArenaError::Ineligible)?;
        if spec.split == crate::EvaluationSplit::Train {
            return Err(ArenaError::Ineligible);
        }
        spec.configs
            .into_iter()
            .find(|config| config.id == id)
            .ok_or(ArenaError::Ineligible)
    }
}

fn execution_matches(config: &crate::ArenaConfig, trace: &EvalTrace) -> bool {
    let Some(execution) = &trace.execution else {
        return config.variant == crate::ArenaVariant::default();
    };
    execution.variant == config.variant
        && execution.steps.len() == config.roles().len()
        && execution
            .steps
            .iter()
            .zip(config.roles())
            .all(|(step, role)| {
                step.role == *role
                    && step.model == config.model_for(*role)
                    && step.input_tokens > 0
                    && step.output_tokens > 0
            })
        && execution
            .steps
            .last()
            .is_some_and(|step| step.output == trace.output)
        && execution
            .steps
            .iter()
            .try_fold(0u64, |sum, step| sum.checked_add(step.input_tokens))
            == Some(trace.input_tokens)
        && execution
            .steps
            .iter()
            .try_fold(0u64, |sum, step| sum.checked_add(step.output_tokens))
            == Some(trace.output_tokens)
}
