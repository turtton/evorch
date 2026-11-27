//! Execution namespace: stopping runs, goal operations, execution settings and
//! the read models of runs, goals and ledgers (ADR 0045).
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
    views::{MessageContent, Page, RunSummary},
    wire::{Revision, Sequence, UnixMillis},
};
use serde::{Deserialize, Serialize};
pub mod invocation;
pub mod plan;
pub mod shell;
pub use invocation::{
    EndInvocation, GetInvocation, InvocationEnded, InvocationStarted, InvocationView,
    StartInvocation,
};

pub const COMMAND_OPERATIONS: &[&str] = &[
    "stop_chat",
    "stop_run",
    "submit_goal",
    "set_goal_review",
    "set_goal_checks_paused",
    "set_agent_web_tools_enabled",
    "start_invocation",
    "end_invocation",
];
pub const QUERY_OPERATIONS: &[&str] = &[
    "list_runs",
    "list_run_details",
    "get_run",
    "get_thread_execution",
    "get_conversation_plan",
    "read_conversation_plan_tree",
    "get_conversation_plan_task",
    "read_conversation_plan_history",
    "get_invocation",
    "read_shell_output",
];
pub const EVENT_TYPES: &[&str] = &[
    "run_changed",
    "tool_started",
    "tool_finished",
    "shell_job_changed",
    "shell_job_output",
    "thread_execution_changed",
    "conversation_plan_changed",
];
/// Tagged choices inside a command's payload that classification names (none).
pub const NESTED_COMMANDS: &[super::NestedOperation] = &[];

/// Most criteria, findings or checks one goal shows.
pub const MAX_EXECUTION_ITEMS: usize = 32;
/// Most bytes of a run ledger body a [`LedgerView`] carries.
pub const MAX_LEDGER_BYTES: usize = 16 * 1024;
/// Most bytes of one criterion, finding or evidence text.
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

/// Goal and ledger of a thread, read together.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThreadExecution {
    pub thread: ThreadId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_run: Option<RunId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal: Option<GoalView>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ledger: Option<LedgerView>,
}

impl ThreadExecution {
    pub fn validate(&self) -> Result<(), ValidationError> {
        self.goal.as_ref().map_or(Ok(()), GoalView::validate)?;
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
    StartInvocation(StartInvocation),
    EndInvocation(EndInvocation),
}

impl ExecutionCommand {
    pub fn required_capability(&self) -> Capability {
        Capability::Commands
    }

    pub fn validate(&self) -> Result<(), ValidationError> {
        match self {
            Self::SubmitGoal(goal) => goal.validate(),
            Self::StartInvocation(start) => start.validate(),
            Self::StopChat(_)
            | Self::StopRun(_)
            | Self::EndInvocation(_)
            | Self::SetGoalReview(_)
            | Self::SetGoalChecksPaused(_)
            | Self::SetAgentWebToolsEnabled(_) => Ok(()),
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
pub enum ExecutionResult {
    InvocationStarted(InvocationStarted),
    InvocationEnded(InvocationEnded),
}

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
    /// Goal and ledger of a thread.
    GetThreadExecution(ThreadTarget),
    GetConversationPlan(ThreadTarget),
    ReadConversationPlanTree(plan::PlanTreeQuery),
    GetConversationPlanTask(plan::PlanTaskQuery),
    ReadConversationPlanHistory(plan::PlanHistoryQuery),
    GetInvocation(GetInvocation),
    ReadShellOutput(shell::ReadShellOutput),
}

impl ExecutionQuery {
    pub fn required_capability(&self) -> Capability {
        Capability::Queries
    }

    pub fn validate(&self, _limits: &WireLimits) -> Result<(), ValidationError> {
        match self {
            Self::ListRuns(q) => q.page.validate(),
            Self::ListRunDetails(q) => q.page.validate(),
            Self::GetRun(_) | Self::GetThreadExecution(_) | Self::GetConversationPlan(_) => Ok(()),
            Self::GetInvocation(_) => Ok(()),
            Self::ReadShellOutput(q) => q.validate(),
            Self::ReadConversationPlanTree(q) => {
                plan::limit(q.limit, 100)?;
                q.cursor
                    .as_ref()
                    .map_or(Ok(()), |c| check_bytes("cursor", c, 4096, false))
            }
            Self::GetConversationPlanTask(q) => crate::ids::validate_id("task_id", &q.task_id),
            Self::ReadConversationPlanHistory(q) => plan::limit(q.limit, 20),
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
    Run(Box<RunDetail>),
    ThreadExecution(Box<ThreadExecution>),
    ConversationPlan(Box<plan::PlanSummary>),
    ConversationPlanTree(Box<plan::PlanTreePage>),
    ConversationPlanTask(Box<plan::PlanTaskRead>),
    ConversationPlanHistory(Box<plan::PlanHistoryPage>),
    Invocation(Box<InvocationView>),
    ShellOutput(shell::ShellOutput),
}

impl ExecutionReply {
    pub fn validate(&self, _limits: &WireLimits) -> Result<(), ValidationError> {
        match self {
            Self::Runs(page) => page.validate(),
            Self::RunDetails(page) => page.validate_with(RunDetail::validate),
            Self::Run(run) => run.validate(),
            Self::ThreadExecution(execution) => execution.validate(),
            Self::ConversationPlan(v) => v.validate(),
            Self::ConversationPlanTree(v) => v.validate(),
            Self::ConversationPlanTask(v) => v.validate(),
            Self::ConversationPlanHistory(v) => v.validate(),
            Self::Invocation(v) => v.validate(),
            Self::ShellOutput(v) => v.validate(),
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
    ShellJobChanged {
        job: shell::ShellJobView,
    },
    ShellJobOutput {
        thread: ThreadId,
        run: RunId,
        job_uid: String,
        offset: u64,
        chunk: String,
    },
    ToolStarted {
        tool: ToolActivityView,
    },
    ToolFinished {
        thread: ThreadId,
        run: RunId,
        call_id: String,
        status: ToolActivityStatus,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u64>,
    },
    /// Invalidation only. Always re-read the projection; never replace state with this event.
    ConversationPlanChanged {
        thread: ThreadId,
        revision: Revision,
    },
    RunChanged {
        run: RunSummary,
    },
    /// The goal of a thread changed; absent means none now.
    ThreadExecutionChanged {
        thread: ThreadId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        goal: Option<GoalView>,
    },
}

/// Memory-only activity: snapshots contain running calls, never completed history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolActivityView {
    pub thread: ThreadId,
    pub run: RunId,
    pub call_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_call_id: Option<String>,
    pub tool_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<MessageContent>,
    /// Elapsed time at publication / snapshot cut, not at client receipt.
    pub elapsed_ms: u64,
}
impl ToolActivityView {
    pub fn validate(&self) -> Result<(), ValidationError> {
        check_bytes("call_id", &self.call_id, MAX_NAME_BYTES, false)?;
        check_bytes("tool_name", &self.tool_name, MAX_NAME_BYTES, false)?;
        if let Some(parent) = &self.parent_call_id {
            check_bytes("parent_call_id", parent, MAX_NAME_BYTES, false)?;
        }
        if let Some(summary) = &self.input_summary {
            check_bytes("input_summary", summary, MAX_TITLE_BYTES, false)?;
        }
        self.input.as_ref().map_or(Ok(()), |v| v.validate("input"))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolActivityStatus {
    Succeeded,
    Failed,
    Interrupted,
}

impl ExecutionEvent {
    pub fn validate(&self) -> Result<(), ValidationError> {
        match self {
            Self::ShellJobChanged { job } => job.validate(),
            Self::ShellJobOutput {
                job_uid,
                offset,
                chunk,
                ..
            } => {
                shell::identity(job_uid)?;
                if offset.checked_add(chunk.len() as u64).is_none() {
                    return Err(ValidationError::OutOfRange {
                        field: "offset",
                        reason: "output range overflow",
                    });
                }
                check_bytes("chunk", chunk, shell::MAX_SHELL_OUTPUT_EVENT_BYTES, true)
            }
            Self::ToolStarted { tool } => tool.validate(),
            Self::ToolFinished { call_id, .. } => {
                check_bytes("call_id", call_id, MAX_NAME_BYTES, false)
            }
            Self::RunChanged { .. } | Self::ConversationPlanChanged { .. } => Ok(()),
            Self::ThreadExecutionChanged { goal, .. } => {
                goal.as_ref().map_or(Ok(()), GoalView::validate)
            }
        }
    }
}
