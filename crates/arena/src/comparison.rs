use crate::{ArenaError, ArenaReport, ArenaSpec, ArenaVariant};
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct VariantComparison {
    pub baseline: String,
    pub candidate: String,
    pub baseline_variant: ArenaVariant,
    pub candidate_variant: ArenaVariant,
    pub model_changed: bool,
    pub attribution_changed: bool,
    pub prompt_changed: bool,
    pub routing_changed: bool,
    pub topology_changed: bool,
    pub baseline_passed: bool,
    pub candidate_passed: bool,
    pub token_delta: i128,
    pub elapsed_ms_delta: i128,
}

impl ArenaReport {
    /// Deltas are candidate minus baseline; multiple changed axes are not causal attribution.
    pub fn compare(&self, baseline: &str) -> Result<Vec<VariantComparison>, ArenaError> {
        let base = self
            .traces()
            .iter()
            .find(|trace| trace.config_id == baseline)
            .ok_or(ArenaError::InvalidSpec("unknown baseline"))?;
        let spec: ArenaSpec = serde_json::from_str(&base.task_spec)
            .map_err(|_| ArenaError::InvalidSpec("comparison manifest required"))?;
        let baseline_config = spec
            .configs
            .iter()
            .find(|config| config.id == baseline)
            .ok_or(ArenaError::InvalidSpec("baseline missing from manifest"))?;
        self.traces()
            .iter()
            .filter(|trace| trace.config_id != baseline)
            .map(|trace| {
                let candidate = spec
                    .configs
                    .iter()
                    .find(|config| config.id == trace.config_id)
                    .ok_or(ArenaError::InvalidSpec("candidate missing from manifest"))?;
                Ok(VariantComparison {
                    baseline: baseline.into(),
                    candidate: trace.config_id.clone(),
                    baseline_variant: baseline_config.variant.clone(),
                    candidate_variant: candidate.variant.clone(),
                    model_changed: baseline_config.model != candidate.model,
                    attribution_changed: baseline_config.attribution != candidate.attribution,
                    prompt_changed: baseline_config.variant.prompt != candidate.variant.prompt,
                    routing_changed: baseline_config.variant.routing != candidate.variant.routing,
                    topology_changed: baseline_config.variant.topology
                        != candidate.variant.topology,
                    baseline_passed: base.failure.is_none(),
                    candidate_passed: trace.failure.is_none(),
                    token_delta: i128::from(trace.input_tokens) + i128::from(trace.output_tokens)
                        - i128::from(base.input_tokens)
                        - i128::from(base.output_tokens),
                    elapsed_ms_delta: i128::from(trace.elapsed_ms) - i128::from(base.elapsed_ms),
                })
            })
            .collect()
    }
}
