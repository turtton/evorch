//! Execution namespace: stopping runs, goal operations and execution settings.
use crate::{
    commands::{
        SetAgentWebToolsEnabled, SetGoalChecksPaused, SetGoalReview, StopRun, SubmitGoal,
        ThreadTarget,
    },
    handshake::{Capability, WireLimits},
    queries::ThreadQuery,
    validation::ValidationError,
    views::{Page, RunSummary},
};
use serde::{Deserialize, Serialize};

pub const COMMAND_OPERATIONS: &[&str] = &[
    "stop_chat",
    "stop_run",
    "submit_goal",
    "set_goal_review",
    "set_goal_checks_paused",
    "set_agent_web_tools_enabled",
];
pub const QUERY_OPERATIONS: &[&str] = &["list_runs"];
pub const EVENT_TYPES: &[&str] = &["run_changed"];
/// Tagged choices inside a command's payload that classification names (none).
pub const NESTED_COMMANDS: &[super::NestedOperation] = &[];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum ExecutionCommand {
    StopChat(ThreadTarget),
    StopRun(StopRun),
    SubmitGoal(SubmitGoal),
    SetGoalReview(SetGoalReview),
    SetGoalChecksPaused(SetGoalChecksPaused),
    SetAgentWebToolsEnabled(SetAgentWebToolsEnabled),
}

impl ExecutionCommand {
    pub fn required_capability(&self) -> Capability {
        Capability::Commands
    }

    pub fn validate(&self) -> Result<(), ValidationError> {
        match self {
            Self::SubmitGoal(goal) => goal.validate(),
            Self::StopChat(_)
            | Self::StopRun(_)
            | Self::SetGoalReview(_)
            | Self::SetGoalChecksPaused(_)
            | Self::SetAgentWebToolsEnabled(_) => Ok(()),
        }
    }
}

no_results!(ExecutionResult);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum ExecutionQuery {
    ListRuns(ThreadQuery),
}

impl ExecutionQuery {
    pub fn required_capability(&self) -> Capability {
        Capability::Queries
    }

    pub fn validate(&self, _limits: &WireLimits) -> Result<(), ValidationError> {
        match self {
            Self::ListRuns(q) => q.page.validate(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum ExecutionReply {
    Runs(Page<RunSummary>),
}

impl ExecutionReply {
    pub fn validate(&self, _limits: &WireLimits) -> Result<(), ValidationError> {
        match self {
            Self::Runs(page) => page.validate(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum ExecutionEvent {
    RunChanged { run: RunSummary },
}

impl ExecutionEvent {
    pub fn validate(&self) -> Result<(), ValidationError> {
        match self {
            Self::RunChanged { .. } => Ok(()),
        }
    }
}
