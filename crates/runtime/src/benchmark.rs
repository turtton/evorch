//! Trusted, opt-in recording and local replay of a sequential delegated run.
//!
//! The caller owns snapshots and must restore the recorded absolute workspace
//! before replay. This is a local intervention, not a whole-task continuation.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use event_bus::AgentRunPhase;
use providers::{Message, ToolSpec};
use serde::{Deserialize, Serialize};

use crate::{ModelPreference, Role, RunId, RuntimeError};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BenchmarkSelector {
    #[serde(with = "role_name")]
    pub role: Role,
    pub category: Option<String>,
    /// One-based matching delegation occurrence.
    pub occurrence: usize,
}

mod role_name {
    use super::*;
    pub fn serialize<S: serde::Serializer>(role: &Role, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&role.name().to_ascii_lowercase())
    }
    pub fn deserialize<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Role, D::Error> {
        let name = String::deserialize(deserializer)?;
        [
            Role::Orchestrator,
            Role::Explorer,
            Role::Worker,
            Role::Reviewer,
            Role::WebResearcher,
            Role::Planner,
            Role::Oracle,
            Role::MultimodalLooker,
        ]
        .into_iter()
        .find(|role| {
            role.name().eq_ignore_ascii_case(&name)
                || role
                    .name()
                    .replace('_', "")
                    .eq_ignore_ascii_case(&name.replace('_', ""))
        })
        .ok_or_else(|| serde::de::Error::custom(format!("unknown role: {name}")))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BenchmarkModelSettings {
    /// Replays keep the same serializer and its generation semantics.
    pub protocol: model::ApiProtocol,
    pub preference: ModelPreference,
    pub generation: config::GenerationOverridesConfig,
    pub service_tier: Option<providers::ServiceTier>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BenchmarkCheckpoint {
    pub version: u32,
    pub origin_run_id: RunId,
    pub parent_run_id: RunId,
    pub role: Role,
    pub category: Option<String>,
    pub prompt: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSpec>,
    pub workspace_root: PathBuf,
    pub model: BenchmarkModelSettings,
    /// Original family used for the recorded tool descriptions.
    pub selected_model: String,
    pub budget: crate::budget_tracker::BudgetSettings,
    pub load_skills: Vec<String>,
}

#[async_trait]
pub trait BenchmarkRecorder: Send + Sync {
    /// Must durably save the checkpoint and workspace before returning.
    async fn capture(&self, checkpoint: &BenchmarkCheckpoint) -> Result<(), String>;

    /// Called before terminal publication unblocks the parent, including failure
    /// and interruption. Only `Done` permits taking a completed-work snapshot.
    /// Interrupted synchronous processes may still be terminating after their
    /// kill signal; on every other phase the host must invalidate the record and
    /// avoid snapshotting or reusing the workspace without confirmed teardown.
    async fn completed(
        &self,
        _checkpoint: &BenchmarkCheckpoint,
        _phase: AgentRunPhase,
        _result: Option<&str>,
    ) -> Result<(), String> {
        Ok(())
    }
}

pub(crate) struct Recording {
    pub selector: BenchmarkSelector,
    pub recorder: Arc<dyn BenchmarkRecorder>,
    pub seen: Mutex<usize>,
}

pub(crate) fn unsupported(reason: impl std::fmt::Display) -> RuntimeError {
    RuntimeError::Model {
        reason: format!("benchmark unsupported: {reason}"),
    }
}

pub(crate) fn validate_config(config: &crate::RunConfig, role: Role) -> Result<(), RuntimeError> {
    if role.capabilities().check_tool(role.name(), "delegate")
        == agents::CapabilityDecision::Allowed
        || config.interactive
        || config.keep_alive
        || config.team.is_some()
        || config.ownership.is_some()
        || config.topology != crate::CoordinationTopology::Single
        || config.workspace_mode != crate::WorkspaceMode::Shared
        || config.learning_internal
        || config.initial_thread_goal.is_some()
        || config.conversation
        || config.purpose != crate::run::RunPurpose::General
    {
        return Err(unsupported(
            "requires a noninteractive shared-workspace leaf without team or durable ownership",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
