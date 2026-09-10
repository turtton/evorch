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

    fn comparison_complete(&self) -> bool {
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
        Ok(config::RouteCandidateConfig {
            profile: trace.profile.clone(),
            model: Some(trace.model.clone()),
        })
    }
}
