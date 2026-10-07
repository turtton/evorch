//! Generic conversation objectives, independent of delivery or PR workflows.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThreadGoalPhase {
    Working,
    Checking,
    Reviewing,
    Repairing,
    Complete,
    Blocked,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThreadGoalCheck {
    /// Zero-based index into the goal's acceptance criteria.
    pub criterion: usize,
    pub met: bool,
    /// Concrete observation or artifact/source reference supporting this result.
    pub evidence: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadGoalUsage {
    pub model_requests: u32,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadGoalSnapshot {
    pub goal_id: String,
    pub thread_id: String,
    pub root_run_id: String,
    /// Earlier roots whose surviving descendants still belong to this objective.
    pub related_root_run_ids: Vec<String>,
    pub objective: String,
    pub criteria: Vec<String>,
    pub checks: Vec<ThreadGoalCheck>,
    pub phase: ThreadGoalPhase,
    pub review_enabled: bool,
    pub checks_paused: bool,
    pub work_stopped: bool,
    pub epoch: u64,
    pub review_round: u32,
    pub findings: Vec<String>,
    pub reason: Option<String>,
    pub usage: ThreadGoalUsage,
    pub max_review_rounds: u32,
    pub max_tokens: Option<u64>,
    /// The user's request, retained separately from the agent's proposed objective.
    pub original_request: String,
}
