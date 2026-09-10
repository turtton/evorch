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
        select(&self.traces)
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
