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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArenaSpec {
    pub id: String,
    pub project: String,
    pub task: TaskSpec,
    pub configs: Vec<ArenaConfig>,
    pub max_output_tokens: u64,
    pub total_token_budget: u64,
    pub timeout_ms: u64,
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
