use crate::Attribution;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskSpec {
    pub id: String,
    pub prompt: String,
    pub expected_output: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArenaConfig {
    pub id: String,
    pub profile: String,
    pub model: String,
    pub attribution: Attribution,
    #[serde(default)]
    pub variant: crate::ArenaVariant,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArenaSpec {
    pub id: String,
    pub project: String,
    pub task: TaskSpec,
    #[serde(default)]
    pub split: EvaluationSplit,
    pub configs: Vec<ArenaConfig>,
    pub max_output_tokens: u64,
    /// Arena-wide ceiling, divided equally in advance; unused shares are never transferred.
    pub total_token_budget: u64,
    /// Arena-wide deadline; each candidate gets an equal independent time allowance.
    pub timeout_ms: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvaluationSplit {
    Train,
    #[default]
    Validation,
    Holdout,
    Redteam,
}

#[derive(Debug, thiserror::Error)]
pub enum ArenaError {
    #[error("invalid arena spec: {0}")]
    InvalidSpec(&'static str),
    #[error("storage: {0}")]
    Storage(#[from] storage::StorageError),
    #[error("storage task: {0}")]
    Join(#[from] tokio::task::JoinError),
    #[error("promotion requires explicit user confirmation")]
    ConfirmationRequired,
    #[error("configuration is not a selected routing candidate")]
    Ineligible,
    #[error("promotion I/O: {0}")]
    PromotionIo(#[from] std::io::Error),
}

impl ArenaSpec {
    pub fn validate(&self) -> Result<(), ArenaError> {
        if [&self.id, &self.project, &self.task.id, &self.task.prompt]
            .iter()
            .any(|s| s.trim().is_empty())
            || self.configs.len() < 2
            || self.configs.len() > 64
            || self.max_output_tokens == 0
            || self.total_token_budget == 0
            || self.timeout_ms == 0
            || self.timeout_ms > 3_600_000
        {
            return Err(ArenaError::InvalidSpec(
                "identity, 2..=64 configs and bounded positive limits required",
            ));
        }
        let mut ids = BTreeSet::new();
        for config in &self.configs {
            config.validate_variant()?;
            if [&config.id, &config.profile, &config.model]
                .iter()
                .any(|s| s.trim().is_empty())
                || !ids.insert(&config.id)
            {
                return Err(ArenaError::InvalidSpec(
                    "configuration identities must be unique and nonempty",
                ));
            }
        }
        if self
            .configs
            .iter()
            .any(|c| c.profile != self.configs[0].profile)
        {
            return Err(ArenaError::InvalidSpec(
                "runner requires configs on its single provider profile",
            ));
        }
        Ok(())
    }
}

impl ArenaConfig {
    pub fn roles(&self) -> &[Attribution] {
        match &self.variant.topology {
            crate::Topology::Single => std::slice::from_ref(&self.attribution),
            crate::Topology::Sequence { roles } => roles,
        }
    }

    pub fn model_for(&self, role: Attribution) -> &str {
        self.variant
            .routing
            .iter()
            .find(|route| route.role == role)
            .map_or(&self.model, |route| &route.model)
    }

    fn validate_variant(&self) -> Result<(), ArenaError> {
        let roles = self.roles();
        if roles.is_empty() || roles.len() > 8 {
            return Err(ArenaError::InvalidSpec("topology requires 1..=8 stages"));
        }
        if self
            .variant
            .prompt
            .as_ref()
            .is_some_and(|p| p.version.trim().is_empty() || p.system.trim().is_empty())
        {
            return Err(ArenaError::InvalidSpec(
                "prompt version and system must be nonempty",
            ));
        }
        for (index, route) in self.variant.routing.iter().enumerate() {
            if route.model.trim().is_empty()
                || !roles.contains(&route.role)
                || self.variant.routing[..index]
                    .iter()
                    .any(|r| r.role == route.role)
            {
                return Err(ArenaError::InvalidSpec(
                    "routing requires unique active roles and models",
                ));
            }
        }
        Ok(())
    }
}
