//! Execution namespace: stopping runs, goal operations, execution settings and
//! the read models of runs, goals, todo lists and ledgers (ADR 0045).
use crate::{
    commands::{
        SetAgentWebToolsEnabled, SetGoalChecksPaused, SetGoalReview, StopRun, SubmitGoal,
        ThreadTarget,
    },
    handshake::{Capability, WireLimits},
    ids::{GoalId, RunId, ThreadId},
    limits::{MAX_NAME_BYTES, MAX_OBJECTIVE_BYTES, MAX_TITLE_BYTES},
    queries::{PageRequest, ThreadQuery},
    validation::{ValidationError, check_bytes, check_count},
    views::{Page, RunSummary},
    wire::{Revision, Sequence, UnixMillis},
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
pub const QUERY_OPERATIONS: &[&str] = &[
    "list_runs",
    "list_run_details",
    "get_run",
    "get_thread_execution",
];
pub const EVENT_TYPES: &[&str] = &["run_changed", "thread_execution_changed"];
/// Tagged choices inside a command's payload that classification names (none).
pub const NESTED_COMMANDS: &[super::NestedOperation] = &[];

/// Most criteria, findings, checks or todo items one goal or list shows.
pub const MAX_EXECUTION_ITEMS: usize = 32;
/// Most bytes of a run ledger body a [`LedgerView`] carries.
pub const MAX_LEDGER_BYTES: usize = 16 * 1024;
/// Most bytes of one criterion, finding, evidence or todo text.
pub const MAX_ITEM_TEXT_BYTES: usize = MAX_TITLE_BYTES * 2;

/// Where a run stands, as far as the core can tell. A run that left the
/// runtime stays visible: it is never dropped from a listing because it is gone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunLifecycle {
    /// Pending, running or waiting.
    Active,
    /// Ended by itself: done or failed.
    Terminal,
    /// Ended because somebody stopped or cancelled it.
    Stopped,
    /// Saved as active but not running in this core: waiting for recovery.
    Recovering,
}

/// Which source a run's detail comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunSource {
    /// The runtime still holds the run.
    Live,
    /// Only the persisted record remains.
    HistoryOnly,
}

/// Why a run ended, for a run that did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunTerminalReason {
    Completed,
    Failed,
    Stopped,
}

/// One run with what [`RunSummary`] leaves out. The summary stays as small as
/// it was; the extras live here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunDetail {
    pub summary: RunSummary,
    /// Display name (the delegated agent's name, else the role).
    pub name: String,
    pub lifecycle: RunLifecycle,
    pub source: RunSource,
    /// The model a live run selected; absent once only history remains.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_reason: Option<RunTerminalReason>,
    /// Direct children of the run, live and persisted together.
    pub child_count: u32,
    /// When the persisted record last changed; absent for a run with none yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<UnixMillis>,
    /// Changes with anything above; compare for equality only.
    pub revision: Revision,
}

impl RunDetail {
    pub fn validate(&self) -> Result<(), ValidationError> {
        check_bytes("name", &self.name, MAX_NAME_BYTES, true)?;
        if let Some(model) = &self.model {
            check_bytes("model", model, MAX_NAME_BYTES, true)?;
        }
        if let Some(category) = &self.category {
            check_bytes("category", category, MAX_NAME_BYTES, true)?;
        }
        Ok(())
    }
}

/// Which runs of a thread a listing shows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum RunScope {
    /// The runs that have no parent.
    Roots,
    /// The direct children (subagents) of one run.
    Children { parent: RunId },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunDetailsQuery {
    pub thread: ThreadId,
    pub scope: RunScope,
    #[serde(default)]
    pub page: PageRequest,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunRef {
    pub thread: ThreadId,
    pub run: RunId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalPhase {
    Working,
    Checking,
    Reviewing,
    Repairing,
    Complete,
    Blocked,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoalCheckView {
    /// Zero-based index into the goal's criteria.
    pub criterion: u32,
    pub met: bool,
    pub evidence: String,
}

/// A thread's goal, bounded for display.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoalView {
    pub goal: GoalId,
    pub root_run: RunId,
    pub objective: String,
    pub criteria: Vec<String>,
    pub checks: Vec<GoalCheckView>,
    pub phase: GoalPhase,
    pub review_enabled: bool,
    pub checks_paused: bool,
    pub work_stopped: bool,
    pub review_round: u32,
    pub max_review_rounds: u32,
    pub findings: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The goal's own epoch: it moves whenever the goal is invalidated.
    pub epoch: Revision,
}

impl GoalView {
    pub fn validate(&self) -> Result<(), ValidationError> {
        check_bytes("objective", &self.objective, MAX_OBJECTIVE_BYTES, true)?;
        check_count("criteria", self.criteria.len(), MAX_EXECUTION_ITEMS)?;
        check_count("checks", self.checks.len(), MAX_EXECUTION_ITEMS)?;
        check_count("findings", self.findings.len(), MAX_EXECUTION_ITEMS)?;
        for text in self.criteria.iter().chain(&self.findings) {
            check_bytes("goal", text, MAX_ITEM_TEXT_BYTES, true)?;
        }
        for check in &self.checks {
            check_bytes("evidence", &check.evidence, MAX_ITEM_TEXT_BYTES, true)?;
        }
        if let Some(reason) = &self.reason {
            check_bytes("reason", reason, MAX_ITEM_TEXT_BYTES, true)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
    Interrupted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TodoItemView {
    pub content: String,
    pub status: TodoStatus,
}

/// A thread's todo list. Read only: nothing here edits it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TodoView {
    pub list_id: String,
    pub revision: Revision,
    pub items: Vec<TodoItemView>,
}

impl TodoView {
    pub fn validate(&self) -> Result<(), ValidationError> {
        check_bytes("list_id", &self.list_id, MAX_NAME_BYTES, true)?;
        check_count("items", self.items.len(), MAX_EXECUTION_ITEMS)?;
        for item in &self.items {
            check_bytes("content", &item.content, MAX_ITEM_TEXT_BYTES, true)?;
        }
        Ok(())
    }
}

/// The latest working-note revision of a run (its ledger), cut to
/// [`MAX_LEDGER_BYTES`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LedgerView {
    pub run: RunId,
    pub seq: Sequence,
    pub body: String,
    pub truncated: bool,
}

impl LedgerView {
    pub fn validate(&self) -> Result<(), ValidationError> {
        check_bytes("body", &self.body, MAX_LEDGER_BYTES, true)
    }
}

/// Goal, todo list and ledger of a thread, read together.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThreadExecution {
    pub thread: ThreadId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_run: Option<RunId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal: Option<GoalView>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub todo: Option<TodoView>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ledger: Option<LedgerView>,
}

impl ThreadExecution {
    pub fn validate(&self) -> Result<(), ValidationError> {
        self.goal.as_ref().map_or(Ok(()), GoalView::validate)?;
        self.todo.as_ref().map_or(Ok(()), TodoView::validate)?;
        self.ledger.as_ref().map_or(Ok(()), LedgerView::validate)
    }
}

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
    /// Every run of a thread, as the small summary.
    ListRuns(ThreadQuery),
    /// The roots of a thread, or the children of one run, with details.
    ListRunDetails(RunDetailsQuery),
    GetRun(RunRef),
    /// Goal, todo list and ledger of a thread.
    GetThreadExecution(ThreadTarget),
}

impl ExecutionQuery {
    pub fn required_capability(&self) -> Capability {
        Capability::Queries
    }

    pub fn validate(&self, _limits: &WireLimits) -> Result<(), ValidationError> {
        match self {
            Self::ListRuns(q) => q.page.validate(),
            Self::ListRunDetails(q) => q.page.validate(),
            Self::GetRun(_) | Self::GetThreadExecution(_) => Ok(()),
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
    RunDetails(Page<RunDetail>),
    Run(RunDetail),
    ThreadExecution(ThreadExecution),
}

impl ExecutionReply {
    pub fn validate(&self, _limits: &WireLimits) -> Result<(), ValidationError> {
        match self {
            Self::Runs(page) => page.validate(),
            Self::RunDetails(page) => page.validate_with(RunDetail::validate),
            Self::Run(run) => run.validate(),
            Self::ThreadExecution(execution) => execution.validate(),
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
    RunChanged {
        run: RunSummary,
    },
    /// The goal or the todo list of a thread changed; absent means none now.
    ThreadExecutionChanged {
        thread: ThreadId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        goal: Option<GoalView>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        todo: Option<TodoView>,
    },
}

impl ExecutionEvent {
    pub fn validate(&self) -> Result<(), ValidationError> {
        match self {
            Self::RunChanged { .. } => Ok(()),
            Self::ThreadExecutionChanged { goal, todo, .. } => {
                goal.as_ref().map_or(Ok(()), GoalView::validate)?;
                todo.as_ref().map_or(Ok(()), TodoView::validate)
            }
        }
    }
}
